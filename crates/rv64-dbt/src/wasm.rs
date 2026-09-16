//! WebAssembly emitter for T1 regions.
//!
//! Pure regions use a small stackifier. Regions containing guest-memory
//! effects use one local per SSA value so every precise side exit can commit
//! the architectural state that existed immediately before the faulting
//! instruction.

use crate::ir::{
    BinaryOp, DivideOp, Effect, ExactFpOp, LoadKind, Op, Region, ReservationOp, SideExit,
    StoreKind, ValueData, ValueId, ValueType, VectorCompareOp, VectorDirect, VectorFloatSignOp,
    VectorLaneOp, VectorMaskOp, VectorOperand, VectorReductionOp,
};
use crate::lift::LoopBackedge;
use crate::structure::{self, Structure};
use crate::{
    JitLayout, MultiEntryState, ReservationCapability, SystemMemory, TlbMissPolicy, TranslationRow,
    VectorCapability, VectorStateLayout,
};
use std::collections::VecDeque;
use std::fmt;
use wasm_encoder::{
    BlockType, CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection,
    GlobalType, ImportSection, Instruction, MemArg, MemoryType, Module, TypeSection, ValType,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EmitError(pub String);

impl fmt::Display for EmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EmitError {}

#[derive(Clone, Copy, Debug, Default)]
struct HelperImports {
    fp: bool,
    reservation: Option<ReservationCapability>,
    vector: Option<VectorCapability>,
    tlb_fill: bool,
    bulk_copy: bool,
    chain: bool,
    tail_chain: bool,
}

impl HelperImports {
    fn for_region(region: &Region, layout: JitLayout) -> Self {
        Self {
            fp: region.has_fp_helper(),
            reservation: region
                .has_reservation_helper()
                .then_some(layout.reservation)
                .flatten(),
            vector: region
                .has_vector_helper()
                .then_some(layout.vector)
                .flatten(),
            tlb_fill: region_uses_memory(region)
                && layout
                    .sys
                    .is_some_and(|memory| memory.miss == TlbMissPolicy::Refill),
            bulk_copy: layout.sys.is_some() && bulk_copy_loop_plan(region).is_some(),
            chain: false,
            tail_chain: false,
        }
    }

    const fn fp_index(self) -> Option<u32> {
        if self.fp {
            Some(0)
        } else {
            None
        }
    }

    const fn reservation_index(self) -> Option<u32> {
        if self.reservation.is_some() {
            Some(self.fp as u32)
        } else {
            None
        }
    }

    const fn tlb_fill_index(self) -> Option<u32> {
        if self.tlb_fill {
            Some(self.fp as u32 + self.reservation.is_some() as u32 + self.vector.is_some() as u32)
        } else {
            None
        }
    }

    const fn vector_index(self) -> Option<u32> {
        if self.vector.is_some() {
            Some(self.fp as u32 + self.reservation.is_some() as u32)
        } else {
            None
        }
    }

    const fn bulk_copy_index(self) -> Option<u32> {
        if self.bulk_copy {
            Some(
                self.fp as u32
                    + self.reservation.is_some() as u32
                    + self.vector.is_some() as u32
                    + self.tlb_fill as u32,
            )
        } else {
            None
        }
    }

    const fn count(self) -> u32 {
        self.fp as u32
            + self.reservation.is_some() as u32
            + self.vector.is_some() as u32
            + self.tlb_fill as u32
            + self.bulk_copy as u32
            + self.chain as u32
            + self.tail_chain as u32
    }

    const fn chain_index(self) -> Option<u32> {
        if self.chain {
            Some(
                self.fp as u32
                    + self.reservation.is_some() as u32
                    + self.vector.is_some() as u32
                    + self.tlb_fill as u32
                    + self.bulk_copy as u32,
            )
        } else {
            None
        }
    }

    const fn tail_chain_index(self) -> Option<u32> {
        if self.tail_chain {
            Some(
                self.fp as u32
                    + self.reservation.is_some() as u32
                    + self.vector.is_some() as u32
                    + self.tlb_fill as u32
                    + self.bulk_copy as u32
                    + self.chain as u32,
            )
        } else {
            None
        }
    }

    fn include(&mut self, other: Self) -> Result<(), EmitError> {
        self.fp |= other.fp;
        self.tlb_fill |= other.tlb_fill;
        self.bulk_copy |= other.bulk_copy;
        self.chain |= other.chain;
        self.tail_chain |= other.tail_chain;
        match (self.reservation, other.reservation) {
            (Some(lhs), Some(rhs)) if lhs != rhs => {
                return Err(EmitError(
                    "one module cannot mix user and system reservation capabilities".into(),
                ));
            }
            (None, reservation) => self.reservation = reservation,
            _ => {}
        }
        match (self.vector, other.vector) {
            (Some(lhs), Some(rhs)) if lhs != rhs => {
                return Err(EmitError(
                    "one module cannot mix user and system vector capabilities".into(),
                ));
            }
            (None, vector) => self.vector = vector,
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct MemoryTemps {
    index: u32,
    offset: u32,
    context: u32,
    page: Option<u32>,
    load_cache: Option<TranslationCacheTemps>,
    store_cache: Option<TranslationCacheTemps>,
    copy: Option<DenseCopyTemps>,
    bulk_copy: Option<BulkCopyTemps>,
    member_range: Option<MemberRangeTemps>,
}

#[derive(Clone, Copy, Debug)]
struct TranslationCacheTemps {
    page: u32,
    offset: u32,
}

#[derive(Clone, Copy, Debug)]
struct DenseCopyTemps {
    source_address: u32,
    destination_address: u32,
    source_linear: u32,
    destination_linear: u32,
}

#[derive(Clone, Copy, Debug)]
struct BulkCopyTemps {
    request: u32,
    fuel_bytes: u32,
    result: u32,
}

#[derive(Clone, Copy, Debug)]
struct MemberRangeTemps {
    start: u32,
    load_linear: u32,
    store_linear: u32,
}

/// Shared temporaries for direct RVV-to-Wasm-SIMD lowering. One set is reused
/// by every mutually-exclusive member in a generated function.
#[derive(Clone, Copy, Debug)]
struct VectorTemps {
    vtype: u32,
    group_bytes: u32,
    span: u32,
    chunk: u32,
    splat: u32,
    result: u32,
    mask: u32,
    address: u32,
    stride: u32,
    last: u32,
    element: u32,
    index: u32,
    linear: u32,
    linear2: u32,
    context: Option<u32>,
    unit_lmul1_sew: Option<u32>,
}

/// Internal-only completion code. Runtime helpers return 0/1/2; direct SIMD
/// returns 3 so cached modules can retain scalar/FP locals that it provably
/// cannot modify.
const VECTOR_STATUS_DIRECT: i32 = 3;
/// Direct SIMD completed and wrote canonical scalar state. Cached modules must
/// reload their scalar/FP union just as they do after the opaque helper.
const VECTOR_STATUS_DIRECT_SCALAR: i32 = 4;

/// An exact architectural vector configuration established by a preceding
/// immediate configuration instruction in the same straight-line region.
/// Keeping this fact in the emitter lets later integer RVV instructions omit
/// repeated vtype/vl/vstart decoding without making any assumption about the
/// guest binary or its control-flow address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KnownVectorConfig {
    vtype: u64,
    vl: u64,
    vsew: u8,
    lmul_exp: i8,
    fractional_lmul: bool,
    span: u8,
    group_bytes: u8,
    vlmax: u64,
}

/// Exact register-group shape for a vector memory operation. Its effective
/// LMUL is `LMUL * EEW / SEW`, so mixed-width loads and stores can have a
/// different group shape from the current arithmetic configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KnownVectorMemoryConfig {
    fractional_lmul: bool,
    span: u8,
    group_bytes: u8,
}

impl KnownVectorConfig {
    fn decode(vtype: u64, vl: u64) -> Option<Self> {
        if vtype >> 8 != 0 {
            return None;
        }
        let lmul_exp = match vtype & 7 {
            0 => 0i8,
            1 => 1,
            2 => 2,
            3 => 3,
            5 => -3,
            6 => -2,
            7 => -1,
            _ => return None,
        };
        let vsew = ((vtype >> 3) & 7) as u8;
        if vsew > 3 || 3 + i16::from(vsew) > 6 + i16::from(lmul_exp) {
            return None;
        }
        let vlmax_log = 4 + i16::from(lmul_exp) - i16::from(vsew);
        if vlmax_log < 0 {
            return None;
        }
        let vlmax = 1u64 << vlmax_log;
        if vl > vlmax {
            return None;
        }
        let (fractional_lmul, span, group_bytes) = if lmul_exp < 0 {
            (true, 1, 16u8 >> (-lmul_exp as u32))
        } else {
            (false, 1u8 << (lmul_exp as u32), 16u8 << (lmul_exp as u32))
        };
        Some(Self {
            vtype,
            vl,
            vsew,
            lmul_exp,
            fractional_lmul,
            span,
            group_bytes,
            vlmax,
        })
    }

    fn memory_config(self, width: u8) -> Option<KnownVectorMemoryConfig> {
        let eew_exp = match width {
            8 => 0i8,
            16 => 1,
            32 => 2,
            64 => 3,
            _ => return None,
        };
        let emul_exp = self.lmul_exp + eew_exp - self.vsew as i8;
        if !(-3..=3).contains(&emul_exp) {
            return None;
        }
        let (fractional_lmul, span, group_bytes) = if emul_exp < 0 {
            (true, 1, 16u8 >> (-emul_exp as u32))
        } else {
            (false, 1u8 << (emul_exp as u32), 16u8 << (emul_exp as u32))
        };
        Some(KnownVectorMemoryConfig {
            fractional_lmul,
            span,
            group_bytes,
        })
    }
}

// RV64C's architecture-defined compact register bank, plus the dedicated
// return-address and stack-pointer registers used by compressed control/stack
// forms.  This is deliberately invariant across guests, PCs, and host engines.
const STRUCTURED_RESIDENT_X_MASK: u32 = 0x0000_ff06;

#[derive(Clone, Debug)]
struct CachedStateLocals {
    x: [Option<u32>; 32],
    /// Referenced integer registers whose canonical CPU cells are synchronized
    /// at every structured member boundary instead of retained function-wide.
    materialized_x: u32,
    f: [Option<u32>; 32],
    fcsr: Option<u32>,
    valid_x: Option<u32>,
    valid_f: Option<u32>,
    valid_fcsr: Option<u32>,
    write_x: u32,
    write_f: u32,
    write_fcsr: bool,
    pc: u32,
    retired: u32,
    fuel: Option<u32>,
}

impl CachedStateLocals {
    fn lazy(&self) -> bool {
        self.valid_x.is_some() || self.valid_f.is_some() || self.valid_fcsr.is_some()
    }
}

fn region_uses_memory(region: &Region) -> bool {
    region
        .values
        .iter()
        .any(|value| matches!(value.op, Op::Load { .. }))
        || region
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::Store { .. }))
}

fn region_has_direct_vector(region: &Region, layout: JitLayout) -> bool {
    layout.vector_state.is_some()
        && region.effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::Vector {
                    direct: Some(_),
                    ..
                }
            )
        })
}

fn region_has_unmasked_unit_stride(region: &Region, layout: JitLayout) -> bool {
    layout.vector_state.is_some()
        && region.effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::Vector {
                    direct: Some(VectorDirect::UnitStride { masked: false, .. }),
                    ..
                }
            )
        })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RegionMemoryProfile {
    loads: usize,
    stores: usize,
    direct_copies: usize,
}

fn region_memory_profile(region: &Region) -> RegionMemoryProfile {
    let loads = region
        .values
        .iter()
        .filter(|value| matches!(value.op, Op::Load { .. }))
        .count();
    let mut stores = 0;
    let mut direct_copies = 0;
    for effect in &region.effects {
        let Effect::Store {
            value,
            kind,
            condition,
            ..
        } = effect
        else {
            continue;
        };
        stores += 1;
        if condition.is_some() {
            continue;
        }
        if let Some(ValueData {
            op: Op::Load {
                kind: load_kind, ..
            },
            ..
        }) = region.values.get(value.0)
        {
            if load_kind.bytes() == kind.bytes() {
                direct_copies += 1;
            }
        }
    }
    RegionMemoryProfile {
        loads,
        stores,
        direct_copies,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DenseCopyAccess {
    load: ValueId,
    store_position: usize,
    store_address: ValueId,
    source_offset: u64,
    destination_offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DenseCopyPlan {
    /// Value/effect position immediately before which both ranges are
    /// validated and translated. Usually this is the first load. If the
    /// destination register is first read by the following store (the normal
    /// compiler shape for `ld tmp; sd tmp`), the first load remains ordinary
    /// and setup moves to the first store, whose precise exit already includes
    /// that completed load.
    setup_position: usize,
    source_root: ValueId,
    destination_root: ValueId,
    source_base_offset: i64,
    destination_base_offset: i64,
    bytes: u64,
    accesses: Vec<DenseCopyAccess>,
}

/// A deliberately narrow whole-loop form of [`DenseCopyPlan`].  Compilers
/// commonly lower RV64 memcpy/memmove to eight `ld`/`sd` pairs followed by
/// three 64-byte induction updates and `bltu 63, remaining, loop`.  Executing
/// that loop literally in generated Wasm still costs roughly twenty guest
/// operations per 64 bytes, while x86 engines collapse `rep movs` to a bulk
/// copy.  This plan proves the complete induction shape; the emitter also
/// guards the loop-limit register at runtime before calling the system helper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BulkCopyLoopPlan {
    source_reg: u8,
    destination_reg: u8,
    count_reg: u8,
    limit_reg: u8,
    value_reg: u8,
    condition: ValueId,
    next_pc: ValueId,
    entry_load: ValueId,
    bytes_per_iteration: u64,
    limit_value: u64,
    step: i64,
    exit_pc: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DenseStoreAccess {
    store_position: usize,
    store_address: ValueId,
    destination_offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DenseStorePlan {
    setup_position: usize,
    destination_root: ValueId,
    destination_base_offset: i64,
    bytes: u64,
    /// A common 64-bit value for every store, enabling a guarded bulk-memory
    /// fill when that word is a repeated byte (the compiler's memset shape).
    fill_value: Option<ValueId>,
    accesses: Vec<DenseStoreAccess>,
}

impl DenseStorePlan {
    fn store_access(&self, position: usize, address: ValueId) -> Option<DenseStoreAccess> {
        self.accesses
            .iter()
            .copied()
            .find(|access| access.store_position == position && access.store_address == address)
    }
}

impl DenseCopyPlan {
    fn load_access(&self, load: ValueId) -> Option<DenseCopyAccess> {
        if load.0 < self.setup_position {
            return None;
        }
        self.accesses
            .iter()
            .copied()
            .find(|access| access.load == load)
    }

    fn store_access(
        &self,
        position: usize,
        address: ValueId,
        value: ValueId,
    ) -> Option<DenseCopyAccess> {
        if position < self.setup_position {
            return None;
        }
        self.accesses.iter().copied().find(|access| {
            access.store_position == position
                && access.store_address == address
                && access.load == value
        })
    }
}

fn const_i64(region: &Region, value: ValueId) -> Option<i64> {
    match region.values.get(value.0)?.op {
        Op::ConstI64(constant) => Some(constant),
        _ => None,
    }
}

fn static_guest_pc(region: &Region, value: ValueId) -> Option<u64> {
    match region.values.get(value.0)?.op {
        Op::GuestPc(pc) => Some(pc),
        // Hand-built IR and older diagnostic callers may still deliberately
        // use a plain constant as a static successor.
        Op::ConstI64(pc) => Some(pc as u64),
        _ => None,
    }
}

fn read_x_reg(region: &Region, value: ValueId) -> Option<u8> {
    match region.values.get(value.0)?.op {
        Op::ReadX(reg) => Some(reg),
        _ => None,
    }
}

fn read_x_value(region: &Region, reg: u8) -> Option<ValueId> {
    region.values.iter().enumerate().find_map(|(index, data)| {
        matches!(data.op, Op::ReadX(candidate) if candidate == reg).then_some(ValueId(index))
    })
}

fn output_for_reg(region: &Region, reg: u8) -> Option<ValueId> {
    region
        .outputs
        .iter()
        .find_map(|&(candidate, value)| (candidate == reg).then_some(value))
}

fn add_constant_from(region: &Region, value: ValueId, root: ValueId) -> Option<i64> {
    let Op::Binary { op, lhs, rhs } = region.values.get(value.0)?.op else {
        return None;
    };
    match op {
        BinaryOp::I64Add if lhs == root => const_i64(region, rhs),
        BinaryOp::I64Add if rhs == root => const_i64(region, lhs),
        BinaryOp::I64Sub if lhs == root => const_i64(region, rhs)?.checked_neg(),
        _ => None,
    }
}

fn address_root_offset(region: &Region, value: ValueId) -> (ValueId, i64) {
    let Some(data) = region.values.get(value.0) else {
        return (value, 0);
    };
    match data.op {
        Op::Binary {
            op: BinaryOp::I64Add,
            lhs,
            rhs,
        } => {
            if let Some(offset) = const_i64(region, rhs) {
                let (root, base) = address_root_offset(region, lhs);
                return (root, base.wrapping_add(offset));
            }
            if let Some(offset) = const_i64(region, lhs) {
                let (root, base) = address_root_offset(region, rhs);
                return (root, base.wrapping_add(offset));
            }
            (value, 0)
        }
        Op::Binary {
            op: BinaryOp::I64Sub,
            lhs,
            rhs,
        } => {
            if let Some(offset) = const_i64(region, rhs) {
                let (root, base) = address_root_offset(region, lhs);
                (root, base.wrapping_sub(offset))
            } else {
                (value, 0)
            }
        }
        _ => (value, 0),
    }
}

const MEMBER_RANGE_MIN_ACCESSES: usize = 3;

/// One architecture-level bounds-check-elimination opportunity. Every covered
/// ordinary access is `ReadX(root_reg) + constant`, and the complete static
/// span fits one system page. Runtime permission rows still prove the actual
/// page before the direct body can run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MemberRangePlan {
    root: ValueId,
    min_offset: i64,
    span: u64,
    loads: usize,
    stores: usize,
}

impl MemberRangePlan {
    fn direct_offset(self, region: &Region, address: ValueId) -> Option<u64> {
        let (root, offset) = address_root_offset(region, address);
        if root != self.root {
            return None;
        }
        u64::try_from(offset.checked_sub(self.min_offset)?).ok()
    }

    fn root_reg(self, region: &Region) -> Option<u8> {
        match region.values.get(self.root.0)?.op {
            Op::ReadX(reg) => Some(reg),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MemberRangeGroup {
    root: ValueId,
    min_offset: i64,
    max_end: i64,
    loads: usize,
    stores: usize,
}

impl MemberRangeGroup {
    fn accesses(self) -> usize {
        self.loads + self.stores
    }
}

fn extend_member_range_group(
    groups: &mut Vec<MemberRangeGroup>,
    region: &Region,
    address: ValueId,
    bytes: u64,
    store: bool,
) {
    let (root, offset) = address_root_offset(region, address);
    if !matches!(
        region.values.get(root.0).map(|value| &value.op),
        Some(Op::ReadX(_))
    ) {
        return;
    }
    let Ok(bytes) = i64::try_from(bytes) else {
        return;
    };
    let Some(end) = offset.checked_add(bytes) else {
        return;
    };
    if let Some(group) = groups.iter_mut().find(|group| group.root == root) {
        group.min_offset = group.min_offset.min(offset);
        group.max_end = group.max_end.max(end);
        if store {
            group.stores += 1;
        } else {
            group.loads += 1;
        }
    } else {
        groups.push(MemberRangeGroup {
            root,
            min_offset: offset,
            max_end: end,
            loads: usize::from(!store),
            stores: usize::from(store),
        });
    }
}

fn member_range_plan(region: &Region) -> Option<MemberRangePlan> {
    let mut groups = Vec::new();
    for value in &region.values {
        if let Op::Load { address, kind, .. } = value.op {
            extend_member_range_group(&mut groups, region, address, kind.bytes(), false);
        }
    }
    for effect in &region.effects {
        if let Effect::Store {
            address,
            kind,
            condition: None,
            ..
        } = effect
        {
            extend_member_range_group(&mut groups, region, *address, kind.bytes(), true);
        }
    }
    groups.sort_unstable_by(|left, right| {
        right
            .accesses()
            .cmp(&left.accesses())
            .then_with(|| left.root.cmp(&right.root))
    });
    let group = groups.into_iter().next()?;
    if group.accesses() < MEMBER_RANGE_MIN_ACCESSES {
        return None;
    }
    let span = u64::try_from(group.max_end.checked_sub(group.min_offset)?).ok()?;
    if span == 0 || span > 4096 {
        return None;
    }
    Some(MemberRangePlan {
        root: group.root,
        min_offset: group.min_offset,
        span,
        loads: group.loads,
        stores: group.stores,
    })
}

fn outlined_member_range_plan(region: &Region, layout: JitLayout) -> Option<MemberRangePlan> {
    let memory = layout.sys?;
    // Existing dense-range and whole-copy paths already hoist or batch their
    // translations. Replacing them would discard a stronger lowering.
    let minimum = usize::from(memory.cache_min_accesses.max(1));
    if bulk_copy_loop_plan(region).is_some()
        || dense_copy_plan(region, minimum).is_some()
        || dense_store_plan(region, minimum).is_some()
    {
        return None;
    }
    let plan = member_range_plan(region)?;
    let page_bytes = 1u64.checked_shl(u32::from(memory.page_shift))?;
    (plan.span <= page_bytes).then_some(plan)
}

/// Recognize a straight-line, same-width unrolled copy. Translation and
/// same-page checks can be hoisted once for the complete ranges while the
/// original load/store order remains intact, preserving overlap semantics.
fn dense_copy_plan(region: &Region, minimum: usize) -> Option<DenseCopyPlan> {
    let profile = region_memory_profile(region);
    if profile.loads < minimum
        || profile.stores != profile.loads
        || profile.direct_copies != profile.loads
    {
        return None;
    }
    // Hoisting a translation check across a guard could fault an access the
    // guest would not execute. Dense-copy plans are deliberately confined to
    // one unconditional, ordered memory sequence.
    if region
        .effects
        .iter()
        .any(|effect| !matches!(effect, Effect::Store { .. }))
    {
        return None;
    }

    let mut raw: Vec<(ValueId, usize, ValueId, ValueId, i64, ValueId, i64)> =
        Vec::with_capacity(profile.loads);
    for effect in &region.effects {
        let Effect::Store {
            position,
            address: store_address,
            value,
            kind: StoreKind::I64,
            condition: None,
            ..
        } = effect
        else {
            continue;
        };
        let ValueData {
            op:
                Op::Load {
                    address: load_address,
                    kind: LoadKind::I64,
                    ..
                },
            ..
        } = region.values.get(value.0)?
        else {
            return None;
        };
        if value.0 >= *position || raw.iter().any(|(load, ..)| load == value) {
            return None;
        }
        let (source_root, source_offset) = address_root_offset(region, *load_address);
        let (destination_root, destination_offset) = address_root_offset(region, *store_address);
        raw.push((
            *value,
            *position,
            *store_address,
            source_root,
            source_offset,
            destination_root,
            destination_offset,
        ));
    }
    if raw.len() != profile.loads {
        return None;
    }

    let source_root = raw.first()?.3;
    let destination_root = raw.first()?.5;
    if raw
        .iter()
        .any(|entry| entry.3 != source_root || entry.5 != destination_root)
    {
        return None;
    }
    let first_load = raw.iter().map(|entry| entry.0).min()?;
    let first_store = raw.iter().map(|entry| entry.1).min()?;
    let setup_position = if source_root.0 < first_load.0 && destination_root.0 < first_load.0 {
        first_load.0
    } else {
        // IR store operands always precede the effect position. Delaying setup
        // until the first store is precise: earlier loads have no externally
        // visible effect, and this store's side exit captures their register
        // results before the interpreter resumes at the store.
        if source_root.0 >= first_store || destination_root.0 >= first_store {
            return None;
        }
        first_store
    };

    let source_base_offset = raw.iter().map(|entry| entry.4).min()?;
    let source_end_offset = raw.iter().map(|entry| entry.4).max()?;
    let destination_base_offset = raw.iter().map(|entry| entry.6).min()?;
    let destination_end_offset = raw.iter().map(|entry| entry.6).max()?;
    let bytes = u64::try_from(raw.len()).ok()?.checked_mul(8)?;
    let expected_span = i64::try_from(bytes.checked_sub(8)?).ok()?;
    if source_end_offset.checked_sub(source_base_offset)? != expected_span
        || destination_end_offset.checked_sub(destination_base_offset)? != expected_span
    {
        return None;
    }

    let mut seen = vec![false; raw.len()];
    let mut accesses = Vec::with_capacity(raw.len());
    for (load, store_position, store_address, _, source, _, destination) in raw {
        let source_offset = u64::try_from(source.checked_sub(source_base_offset)?).ok()?;
        let destination_offset =
            u64::try_from(destination.checked_sub(destination_base_offset)?).ok()?;
        if source_offset != destination_offset || source_offset & 7 != 0 {
            return None;
        }
        let slot = usize::try_from(source_offset / 8).ok()?;
        if slot >= seen.len() || seen[slot] {
            return None;
        }
        seen[slot] = true;
        accesses.push(DenseCopyAccess {
            load,
            store_position,
            store_address,
            source_offset,
            destination_offset,
        });
    }
    if seen.iter().any(|seen| !seen) {
        return None;
    }
    Some(DenseCopyPlan {
        setup_position,
        source_root,
        destination_root,
        source_base_offset,
        destination_base_offset,
        bytes,
        accesses,
    })
}

/// Prove the compiler-generated 64-byte copy loop used by the modern Alpine
/// benchmark binaries.  Keeping this stricter than the per-iteration dense
/// copy recognizer is intentional: a helper call may represent many guest
/// iterations, so every loop-carried architectural result must be known.
fn bulk_copy_loop_plan(region: &Region) -> Option<BulkCopyLoopPlan> {
    let copy = dense_copy_plan(region, 1)?;
    if !matches!(copy.bytes, 8 | 64)
        || copy.accesses.len() != usize::try_from(copy.bytes / 8).ok()?
    {
        return None;
    }

    let source_reg = read_x_reg(region, copy.source_root)?;
    let destination_reg = read_x_reg(region, copy.destination_root)?;
    if source_reg == destination_reg {
        return None;
    }
    let source_output = output_for_reg(region, source_reg)?;
    let destination_output = output_for_reg(region, destination_reg)?;
    let source_step = add_constant_from(region, source_output, copy.source_root)?;
    let destination_step = add_constant_from(region, destination_output, copy.destination_root)?;
    let iteration_step = i64::try_from(copy.bytes).ok()?;
    if source_step != destination_step
        || !matches!(source_step, step if step == -iteration_step || step == iteration_step)
    {
        return None;
    }
    let expected_base = if source_step > 0 { 0 } else { -iteration_step };
    if copy.source_base_offset != expected_base || copy.destination_base_offset != expected_base {
        return None;
    }

    let Op::SelectI64 {
        condition,
        if_true,
        if_false,
    } = region.values.get(region.next_pc.0)?.op
    else {
        return None;
    };
    if static_guest_pc(region, if_true)? != region.entry_pc {
        return None;
    }
    let exit_pc = static_guest_pc(region, if_false)?;
    if exit_pc != region.end_pc {
        return None;
    }
    let Op::Binary {
        op: BinaryOp::I64LtU,
        lhs: limit_root,
        rhs: count_output,
    } = region.values.get(condition.0)?.op
    else {
        return None;
    };
    let limit_reg = read_x_reg(region, limit_root)?;

    let (count_reg, count_root) =
        region
            .values
            .iter()
            .enumerate()
            .find_map(|(index, data)| match data.op {
                Op::ReadX(reg)
                    if output_for_reg(region, reg) == Some(count_output)
                        && add_constant_from(region, count_output, ValueId(index))
                            == Some(-iteration_step) =>
                {
                    Some((reg, ValueId(index)))
                }
                _ => None,
            })?;
    debug_assert_eq!(
        add_constant_from(region, count_output, count_root),
        Some(-iteration_step)
    );

    // The last load in guest order is the only live value of the scratch
    // register after an ordinary compiler-generated unrolled copy.
    let final_load = copy
        .accesses
        .iter()
        .max_by_key(|access| access.store_position)?
        .load;
    let value_reg = region
        .outputs
        .iter()
        .find_map(|&(reg, value)| (value == final_load).then_some(reg))?;

    let mut expected_outputs = vec![source_reg, destination_reg, count_reg, value_reg];
    expected_outputs.sort_unstable();
    expected_outputs.dedup();
    let mut actual_outputs: Vec<u8> = region.outputs.iter().map(|&(reg, _)| reg).collect();
    actual_outputs.sort_unstable();
    actual_outputs.dedup();
    if actual_outputs != expected_outputs
        || [source_reg, destination_reg, count_reg, limit_reg, value_reg]
            .into_iter()
            .any(|reg| reg == 0)
    {
        return None;
    }

    Some(BulkCopyLoopPlan {
        source_reg,
        destination_reg,
        count_reg,
        limit_reg,
        value_reg,
        condition,
        next_pc: region.next_pc,
        entry_load: copy.accesses.iter().map(|access| access.load).min()?,
        bytes_per_iteration: copy.bytes,
        limit_value: copy.bytes - 1,
        step: source_step,
        exit_pc,
    })
}

/// Recognize an unconditional contiguous run of 64-bit stores with no loads.
/// This covers compiler-generated memset/page-initialization blocks while
/// retaining every original scalar store and value in architectural order.
/// Proving the complete destination range once removes repeated TLB probes;
/// a failed proof exits before the first store so the interpreter observes the
/// exact original fault/MMIO/store-to-code behavior.
fn dense_store_plan(region: &Region, minimum: usize) -> Option<DenseStorePlan> {
    let profile = region_memory_profile(region);
    if profile.loads != 0 || profile.stores < minimum {
        return None;
    }
    if region
        .effects
        .iter()
        .any(|effect| !matches!(effect, Effect::Store { .. }))
    {
        return None;
    }

    let mut raw: Vec<(usize, ValueId, ValueId, ValueId, i64)> = Vec::with_capacity(profile.stores);
    for effect in &region.effects {
        let Effect::Store {
            position,
            address,
            value,
            kind: StoreKind::I64,
            condition: None,
            ..
        } = effect
        else {
            return None;
        };
        let (root, offset) = address_root_offset(region, *address);
        raw.push((*position, *address, *value, root, offset));
    }
    if raw.len() != profile.stores {
        return None;
    }
    let destination_root = raw.first()?.3;
    if raw.iter().any(|entry| entry.3 != destination_root) {
        return None;
    }
    let setup_position = raw.iter().map(|entry| entry.0).min()?;
    if destination_root.0 >= setup_position {
        return None;
    }

    let destination_base_offset = raw.iter().map(|entry| entry.4).min()?;
    let destination_end_offset = raw.iter().map(|entry| entry.4).max()?;
    let bytes = u64::try_from(raw.len()).ok()?.checked_mul(8)?;
    let expected_span = i64::try_from(bytes.checked_sub(8)?).ok()?;
    if destination_end_offset.checked_sub(destination_base_offset)? != expected_span {
        return None;
    }

    let mut seen = vec![false; raw.len()];
    let mut accesses = Vec::with_capacity(raw.len());
    let first_value = raw.first()?.2;
    let fill_value = raw
        .iter()
        .all(|entry| entry.2 == first_value)
        .then_some(first_value);
    for (store_position, store_address, _, _, destination) in raw {
        let destination_offset =
            u64::try_from(destination.checked_sub(destination_base_offset)?).ok()?;
        if destination_offset & 7 != 0 {
            return None;
        }
        let slot = usize::try_from(destination_offset / 8).ok()?;
        if slot >= seen.len() || seen[slot] {
            return None;
        }
        seen[slot] = true;
        accesses.push(DenseStoreAccess {
            store_position,
            store_address,
            destination_offset,
        });
    }
    if seen.iter().any(|seen| !seen) {
        return None;
    }
    Some(DenseStorePlan {
        setup_position,
        destination_root,
        destination_base_offset,
        bytes,
        fill_value,
        accesses,
    })
}

/// Report whether this region will use the range-checked dense-copy lowering
/// for the supplied production layout. Keep this beside the recognizer so
/// runtime diagnostics measure the emitter's actual selection rule rather
/// than maintaining a second, approximate copy of it.
pub(crate) fn uses_dense_copy_plan(region: &Region, layout: JitLayout) -> bool {
    let Some(memory) = layout.sys else {
        return false;
    };
    memory.cache_within_invocation
        && dense_copy_plan(region, usize::from(memory.cache_min_accesses.max(1))).is_some()
}

pub(crate) fn uses_dense_store_plan(region: &Region, layout: JitLayout) -> bool {
    let Some(memory) = layout.sys else {
        return false;
    };
    memory.cache_within_invocation
        && dense_store_plan(region, usize::from(memory.cache_min_accesses.max(1))).is_some()
}

pub(crate) fn uses_bulk_copy_loop_plan(region: &Region, layout: JitLayout) -> bool {
    layout.sys.is_some() && bulk_copy_loop_plan(region).is_some()
}

/// Use invocation-local translation caches only in dense direct-copy members.
/// Other members retain the cheaper direct TLB probe even when they share the
/// module function and its allocated cache locals.
fn memory_temps_for_region(
    mut temps: Option<MemoryTemps>,
    region: &Region,
    layout: JitLayout,
) -> Option<MemoryTemps> {
    let Some(ref mut temps) = temps else {
        return None;
    };
    let Some(memory) = layout.sys else {
        return Some(*temps);
    };
    let minimum = usize::from(memory.cache_min_accesses.max(1));
    let copy = dense_copy_plan(region, minimum).is_some();
    let store = dense_store_plan(region, minimum).is_some();
    if bulk_copy_loop_plan(region).is_none() {
        temps.bulk_copy = None;
    }
    if outlined_member_range_plan(region, layout).is_none() {
        temps.member_range = None;
    }
    if !copy && !store {
        temps.load_cache = None;
        temps.store_cache = None;
        temps.copy = None;
    } else if store {
        temps.load_cache = None;
    }
    Some(*temps)
}

fn allocate_memory_temps(
    local_types: &mut Vec<ValType>,
    region: &Region,
    layout: JitLayout,
) -> Option<MemoryTemps> {
    if layout.sys.is_none() || !region_uses_memory(region) {
        return None;
    }
    let memory = layout.sys.expect("system-memory layout");
    let minimum = usize::from(memory.cache_min_accesses.max(1));
    let cache_copy = memory.cache_within_invocation && dense_copy_plan(region, minimum).is_some();
    let cache_store = memory.cache_within_invocation && dense_store_plan(region, minimum).is_some();
    let bulk_copy = bulk_copy_loop_plan(region).is_some();
    let cache_range = cache_copy || cache_store;
    let page = cache_range.then(|| alloc_local(local_types, ValType::I64));
    let mut alloc_cache = || TranslationCacheTemps {
        page: alloc_local(local_types, ValType::I64),
        offset: alloc_local(local_types, ValType::I64),
    };
    let load_cache = cache_copy.then(&mut alloc_cache);
    let store_cache = cache_range.then(&mut alloc_cache);
    let copy = (cache_range || bulk_copy).then(|| DenseCopyTemps {
        source_address: alloc_local(local_types, ValType::I64),
        destination_address: alloc_local(local_types, ValType::I64),
        source_linear: alloc_local(local_types, ValType::I32),
        destination_linear: alloc_local(local_types, ValType::I32),
    });
    let bulk_copy = bulk_copy.then(|| BulkCopyTemps {
        request: alloc_local(local_types, ValType::I64),
        fuel_bytes: alloc_local(local_types, ValType::I64),
        result: alloc_local(local_types, ValType::I64),
    });
    Some(MemoryTemps {
        index: alloc_local(local_types, ValType::I32),
        offset: alloc_local(local_types, ValType::I64),
        context: alloc_local(local_types, ValType::I64),
        page,
        load_cache,
        store_cache,
        copy,
        bulk_copy,
        member_range: None,
    })
}

fn allocate_vector_temps(
    local_types: &mut Vec<ValType>,
    enabled: bool,
    unit_stride: bool,
    shared_context: Option<u32>,
    system: bool,
) -> Option<VectorTemps> {
    if !enabled {
        return None;
    }
    let context =
        system.then(|| shared_context.unwrap_or_else(|| alloc_local(local_types, ValType::I64)));
    let unit_lmul1_sew = unit_stride.then(|| alloc_local(local_types, ValType::I32));
    Some(VectorTemps {
        vtype: alloc_local(local_types, ValType::I64),
        group_bytes: alloc_local(local_types, ValType::I32),
        span: alloc_local(local_types, ValType::I32),
        chunk: alloc_local(local_types, ValType::I32),
        splat: alloc_local(local_types, ValType::V128),
        result: alloc_local(local_types, ValType::V128),
        mask: alloc_local(local_types, ValType::V128),
        address: alloc_local(local_types, ValType::I64),
        stride: alloc_local(local_types, ValType::I64),
        last: alloc_local(local_types, ValType::I64),
        element: alloc_local(local_types, ValType::I64),
        index: alloc_local(local_types, ValType::I32),
        linear: alloc_local(local_types, ValType::I32),
        linear2: alloc_local(local_types, ValType::I32),
        context,
        unit_lmul1_sew,
    })
}

/// Snapshot the effective system translation context for vector-only modules.
/// Modules that also lower scalar memory reuse `MemoryTemps::context`, which
/// `emit_memory_context_init` has already initialized.
fn emit_vector_memory_context_init(
    function: &mut Function,
    layout: JitLayout,
    temps: Option<VectorTemps>,
    memory_temps: Option<MemoryTemps>,
) {
    if memory_temps.is_some() {
        return;
    }
    let (Some(memory), Some(context)) = (layout.sys, temps.and_then(|temps| temps.context)) else {
        return;
    };
    function.instruction(&Instruction::I32Const(memory.context_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalSet(context));
}

/// Record `SEW + 1` when the live state describes a legal, full-VL, LMUL=1
/// vector group whose system VS field is already dirty. Zero selects the
/// general lowering. VLEN is 128 bits, so this state maps one architectural
/// vector register exactly to one Wasm v128. Requiring dirty state lets a
/// matching direct operation omit both the repeated enabled check and the
/// otherwise redundant mstatus read/modify/write.
fn emit_vector_unit_lmul1_refresh(
    function: &mut Function,
    layout: JitLayout,
    temps: Option<VectorTemps>,
) {
    let Some((temps, active, state)) = temps
        .zip(temps.and_then(|temps| temps.unit_lmul1_sew))
        .zip(layout.vector_state)
        .map(|((temps, active), state)| (temps, active, state))
    else {
        return;
    };

    function.instruction(&Instruction::I32Const(state.vtype_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalSet(temps.vtype));

    // No VILL or reserved high bits; policy bits 6/7 are immaterial here.
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(!0xffi64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Eqz);
    // LMUL=1.
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(7));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::I32And);
    // ELEN=64: e8/e16/e32/e64 only.
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(3));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Const(7));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::LocalTee(temps.index));
    function.instruction(&Instruction::I32Const(3));
    function.instruction(&Instruction::I32LeU);
    function.instruction(&Instruction::I32And);
    // A direct instruction always starts at element zero.
    function.instruction(&Instruction::I32Const(state.vstart_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::I32And);
    // Full LMUL=1 group: vl == 16 bytes >> log2(SEW bytes).
    function.instruction(&Instruction::I32Const(state.vl_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Const(16));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I64ExtendI32U);
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Eq);
    function.instruction(&Instruction::I32And);
    if layout.vector == Some(VectorCapability::System) {
        function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64Const(3 << 9));
        function.instruction(&Instruction::I64And);
        function.instruction(&Instruction::I64Const(3 << 9));
        function.instruction(&Instruction::I64Eq);
        function.instruction(&Instruction::I32And);
    }

    function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::LocalSet(active));
}

fn emit_vector_unit_lmul1_invalidate(function: &mut Function, temps: Option<VectorTemps>) {
    let Some(active) = temps.and_then(|temps| temps.unit_lmul1_sew) else {
        return;
    };
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(active));
}

/// `last` and `stride` double as an invocation-local load-translation cache
/// on the deterministic unit-stride path. All other vector lowerings may use
/// either scratch local, so invalidate the cached tag before emitting them.
fn emit_vector_unit_load_cache_invalidate(function: &mut Function, temps: Option<VectorTemps>) {
    let Some(temps) = temps.filter(|temps| temps.unit_lmul1_sew.is_some()) else {
        return;
    };
    function.instruction(&Instruction::I64Const(-1));
    function.instruction(&Instruction::LocalSet(temps.last));
}

/// Snapshot the runtime's effective data-access context once per generated
/// invocation. Privileged instructions are precise side exits, so this value
/// cannot change before the function returns (including internal CFG hops).
fn emit_memory_context_init(
    function: &mut Function,
    layout: JitLayout,
    temps: Option<MemoryTemps>,
) {
    let (Some(memory), Some(temps)) = (layout.sys, temps) else {
        return;
    };
    function.instruction(&Instruction::I32Const(memory.context_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalSet(temps.context));
    // Wasm locals start at zero. Page zero is a valid numeric page, so an
    // uninitialized cache tag must not be mistaken for a proven translation.
    for cache in [temps.load_cache, temps.store_cache].into_iter().flatten() {
        function.instruction(&Instruction::I64Const(-1));
        function.instruction(&Instruction::LocalSet(cache.page));
    }
}

fn val_type(ty: ValueType) -> ValType {
    match ty {
        ValueType::I32 => ValType::I32,
        ValueType::I64 => ValType::I64,
    }
}

const fn memarg(align: u32, offset: u64) -> MemArg {
    MemArg {
        offset,
        align,
        memory_index: 0,
    }
}

/// Add a static amount to one embedding-owned diagnostic counter.  Callers
/// only reach this helper when `JitLayout::structured_profile` is present, so
/// disabled production emission pays neither code size nor a runtime branch.
fn emit_profile_counter_add(function: &mut Function, address: u32, amount: u64) {
    if address == 0 || amount == 0 {
        return;
    }
    function.instruction(&Instruction::I32Const(address as i32));
    function.instruction(&Instruction::I32Const(address as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Const(amount as i64));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
}

fn emit_structured_profile(function: &mut Function, layout: JitLayout, region: &Region) {
    let Some(counters) = layout.structured_profile else {
        return;
    };
    emit_profile_counter_add(function, counters[0], 1);
    emit_profile_counter_add(function, counters[1], u64::from(region.retired));
    for (address, amount) in counters[2..7].iter().zip(region.trace_mix) {
        emit_profile_counter_add(function, *address, u64::from(amount));
    }
    if region.writes_x2 {
        emit_profile_counter_add(function, counters[7], 1);
    }
    emit_profile_counter_add(function, counters[8], u64::from(region.trace_stack_memory));
}

/// Emit an architectural address whose value is derived from a guest PC.
/// Ordinary modules retain the single absolute constant they used before.
/// Position-independent page modules import one immutable base and encode only
/// the wrapping offset, making their bytes independent of an ASLR alias.
fn emit_guest_pc(function: &mut Function, pc: u64, layout: JitLayout) {
    if let Some(code_base) = layout.pic_code_base {
        function.instruction(&Instruction::GlobalGet(0));
        let offset = pc.wrapping_sub(code_base) as i64;
        if offset != 0 {
            function.instruction(&Instruction::I64Const(offset));
            function.instruction(&Instruction::I64Add);
        }
    } else {
        function.instruction(&Instruction::I64Const(pc as i64));
    }
}

pub(crate) fn emit(
    region: &Region,
    layout: JitLayout,
    loop_backedge: Option<LoopBackedge>,
) -> Result<Vec<u8>, EmitError> {
    validate_emission(region, layout)?;
    let helpers = HelperImports::for_region(region, layout);
    let function = emit_function(region, layout, loop_backedge, helpers)?;
    Ok(finish_module(function, helpers, layout))
}

/// Emit several independently validated region bodies behind one in-module
/// PC dispatcher. All public entries may point at the same dispatcher: it
/// reads the architectural PC, selects a body with a balanced decision tree,
/// and keeps following covered edges until fuel is spent or execution leaves
/// the member set. This is portable core Wasm and needs neither JS callbacks
/// nor the tail-call proposal.
pub(crate) fn emit_multi_entry(
    regions: &[(&Region, Option<LoopBackedge>)],
    layout: JitLayout,
    export_members: bool,
) -> Result<Vec<u8>, EmitError> {
    emit_multi_entry_mode(
        regions,
        layout,
        export_members,
        MultiEntryState::RegisterEager,
    )
}

/// Emit a multi-entry module with either invocation-local architectural state
/// or body-boundary materialization. The latter is useful for measured regions
/// that usually leave after one body: it avoids loading a large register union
/// merely to cache it for no internal edge. Both modes share validation,
/// helpers, dispatch semantics, and precise exits.
pub(crate) fn emit_multi_entry_mode(
    regions: &[(&Region, Option<LoopBackedge>)],
    layout: JitLayout,
    export_members: bool,
    state: MultiEntryState,
) -> Result<Vec<u8>, EmitError> {
    if regions.len() < 2 {
        return Err(EmitError(
            "a multi-entry module requires at least two regions".into(),
        ));
    }

    let mut helpers = HelperImports::default();
    let mut seen = std::collections::BTreeSet::new();
    for &(region, _) in regions {
        validate_emission(region, layout)?;
        if !seen.insert(region.entry_pc) {
            return Err(EmitError("duplicate multi-entry PC".into()));
        }
        helpers.include(HelperImports::for_region(region, layout))?;
    }

    // General multi-entry regions keep their architectural union in locals.
    // A single-latch member becomes a nested local loop over the same cached
    // state, so hot backedges avoid both architectural materialization and the
    // outer PC decision tree.
    if matches!(
        state,
        MultiEntryState::RegisterEager
            | MultiEntryState::RegisterLazy
            | MultiEntryState::RegisterDirect
            | MultiEntryState::RegisterCfg
            | MultiEntryState::RegisterStructured
    ) {
        return emit_cached_multi_entry(
            regions,
            layout,
            helpers,
            export_members,
            state == MultiEntryState::RegisterLazy,
            matches!(
                state,
                MultiEntryState::RegisterDirect | MultiEntryState::RegisterCfg
            ),
            state == MultiEntryState::RegisterStructured,
        );
    }

    let mut functions = Vec::with_capacity(regions.len());
    for &(region, loop_backedge) in regions {
        functions.push(emit_function(region, layout, loop_backedge, helpers)?);
    }
    let entries: Vec<u64> = regions.iter().map(|(region, _)| region.entry_pc).collect();
    let tail_calls = state == MultiEntryState::MemoryTailCall && layout.fuel_addr != 0;
    let wrappers = if tail_calls {
        let body_base = helpers.count();
        let wrapper_base = body_base + regions.len() as u32;
        regions
            .iter()
            .enumerate()
            .map(|(index, (region, _))| {
                let target = static_guest_pc(region, region.next_pc).and_then(|pc| {
                    entries
                        .iter()
                        .position(|entry| *entry == pc)
                        .map(|member| (pc, wrapper_base + member as u32))
                });
                emit_tail_wrapper(body_base + index as u32, target, layout)
            })
            .collect()
    } else {
        Vec::new()
    };
    let dispatch_base = if wrappers.is_empty() {
        helpers.count()
    } else {
        helpers.count() + regions.len() as u32
    };
    let dispatcher = emit_multi_dispatch(&entries, layout, dispatch_base);
    Ok(finish_multi_module(
        functions,
        wrappers,
        dispatcher,
        helpers,
        export_members,
        layout,
    ))
}

fn alloc_local(local_types: &mut Vec<ValType>, ty: ValType) -> u32 {
    let local = 1 + local_types.len() as u32;
    local_types.push(ty);
    local
}

fn emit_cached_multi_entry(
    regions: &[(&Region, Option<LoopBackedge>)],
    layout: JitLayout,
    mut helpers: HelperImports,
    export_members: bool,
    lazy: bool,
    direct_dispatch: bool,
    structured_cfg: bool,
) -> Result<Vec<u8>, EmitError> {
    // Cross-module chaining is useful only after the in-module structured CFG
    // has exhausted its covered successors. A function import avoids the
    // shared-table publication cost that made per-module table imports scale
    // quadratically in V8.
    helpers.chain = structured_cfg && crate::chain_enabled() && layout.fuel_addr != 0;
    helpers.tail_chain = structured_cfg
        && crate::region_tail_chain_enabled()
        && layout.fuel_addr != 0
        && layout.dispatch_base != 0
        && layout.map_gen_addr != 0
        && layout.chain_hops_addr != 0;
    // The public cached dispatcher is the first defined function. Selected
    // members get one private ordinary body immediately after it, allowing a
    // cold translation/refill/fault path without cloning that path into the
    // embedding engine's hot optimized function.
    let mut fallback_indices = vec![None; regions.len()];
    let mut fallbacks = Vec::new();
    if structured_cfg {
        for (member, &(region, _)) in regions.iter().enumerate() {
            if outlined_member_range_plan(region, layout).is_some() {
                fallback_indices[member] = Some(helpers.count() + 1 + fallbacks.len() as u32);
                fallbacks.push(emit_function(region, layout, None, helpers)?);
            }
        }
    }
    let mut need_x = 0u32;
    let mut need_f = 0u32;
    let mut need_fcsr = false;
    let mut write_x = 0u32;
    let mut write_f = 0u32;
    let mut write_fcsr = false;
    for &(region, _) in regions {
        for value in &region.values {
            match value.op {
                Op::ReadX(reg) => need_x |= 1u32 << reg,
                Op::ReadF(reg) => need_f |= 1u32 << reg,
                Op::ReadFcsr => need_fcsr = true,
                _ => {}
            }
        }
        for &(reg, _) in &region.outputs {
            need_x |= 1u32 << reg;
            write_x |= 1u32 << reg;
        }
        for &(reg, _) in &region.f_outputs {
            need_f |= 1u32 << reg;
            write_f |= 1u32 << reg;
        }
        need_fcsr |= region.fcsr_output.is_some();
        write_fcsr |= region.fcsr_output.is_some();
    }

    let mut local_types = Vec::new();
    let mut state = CachedStateLocals {
        x: [None; 32],
        materialized_x: if structured_cfg {
            need_x & !STRUCTURED_RESIDENT_X_MASK & !1
        } else {
            0
        },
        f: [None; 32],
        fcsr: None,
        valid_x: None,
        valid_f: None,
        valid_fcsr: None,
        write_x,
        write_f,
        write_fcsr,
        pc: 0,
        retired: 0,
        fuel: None,
    };
    for reg in 1..32 {
        if need_x & (1u32 << reg) != 0 && state.materialized_x & (1u32 << reg) == 0 {
            state.x[reg] = Some(alloc_local(&mut local_types, ValType::I64));
        }
    }
    for reg in 0..32 {
        if need_f & (1u32 << reg) != 0 {
            state.f[reg] = Some(alloc_local(&mut local_types, ValType::I64));
        }
    }
    if need_fcsr {
        state.fcsr = Some(alloc_local(&mut local_types, ValType::I32));
    }
    if lazy {
        if need_x != 0 {
            state.valid_x = Some(alloc_local(&mut local_types, ValType::I64));
        }
        if need_f != 0 {
            state.valid_f = Some(alloc_local(&mut local_types, ValType::I64));
        }
        if need_fcsr {
            state.valid_fcsr = Some(alloc_local(&mut local_types, ValType::I32));
        }
    }
    state.pc = alloc_local(&mut local_types, ValType::I64);
    state.retired = alloc_local(&mut local_types, ValType::I64);
    if layout.fuel_addr != 0 {
        state.fuel = Some(alloc_local(&mut local_types, ValType::I64));
    }

    // Member bodies are mutually exclusive within one dispatcher iteration,
    // and every architectural result is copied into `state` before another
    // body can run. Reuse type-specific SSA temporary pools across members
    // instead of exposing the sum of all member values as Wasm locals. This
    // bounds the embedding engine's local/register-allocation pressure by the
    // largest member, not by region population.
    let max_i32 = regions
        .iter()
        .map(|(region, _)| {
            region
                .values
                .iter()
                .filter(|value| value.ty == ValueType::I32)
                .count()
        })
        .max()
        .unwrap_or(0);
    let max_i64 = regions
        .iter()
        .map(|(region, _)| {
            region
                .values
                .iter()
                .filter(|value| value.ty == ValueType::I64)
                .count()
        })
        .max()
        .unwrap_or(0);
    let i32_pool: Vec<u32> = (0..max_i32)
        .map(|_| alloc_local(&mut local_types, ValType::I32))
        .collect();
    let i64_pool: Vec<u32> = (0..max_i64)
        .map(|_| alloc_local(&mut local_types, ValType::I64))
        .collect();
    let mut local_maps = Vec::with_capacity(regions.len());
    for &(region, _) in regions {
        let mut next_i32 = 0;
        let mut next_i64 = 0;
        let map = region
            .values
            .iter()
            .map(|value| {
                Some(match value.ty {
                    ValueType::I32 => {
                        let local = i32_pool[next_i32];
                        next_i32 += 1;
                        local
                    }
                    ValueType::I64 => {
                        let local = i64_pool[next_i64];
                        next_i64 += 1;
                        local
                    }
                })
            })
            .collect();
        local_maps.push(map);
    }
    let memory_temps =
        if layout.sys.is_some() && regions.iter().any(|(region, _)| region_uses_memory(region)) {
            let memory = layout.sys.expect("system-memory layout");
            let minimum = usize::from(memory.cache_min_accesses.max(1));
            let cache_copy = memory.cache_within_invocation
                && regions
                    .iter()
                    .any(|(region, _)| dense_copy_plan(region, minimum).is_some());
            let cache_store_range = memory.cache_within_invocation
                && regions
                    .iter()
                    .any(|(region, _)| dense_store_plan(region, minimum).is_some());
            let has_bulk_copy = regions
                .iter()
                .any(|(region, _)| bulk_copy_loop_plan(region).is_some());
            let cache_range = cache_copy || cache_store_range;
            let mut alloc_cache = || TranslationCacheTemps {
                page: alloc_local(&mut local_types, ValType::I64),
                offset: alloc_local(&mut local_types, ValType::I64),
            };
            let load_cache = cache_copy.then(&mut alloc_cache);
            let store_cache = cache_range.then(&mut alloc_cache);
            let page = cache_range.then(|| alloc_local(&mut local_types, ValType::I64));
            let copy = (cache_range || has_bulk_copy).then(|| DenseCopyTemps {
                source_address: alloc_local(&mut local_types, ValType::I64),
                destination_address: alloc_local(&mut local_types, ValType::I64),
                source_linear: alloc_local(&mut local_types, ValType::I32),
                destination_linear: alloc_local(&mut local_types, ValType::I32),
            });
            let bulk_copy = has_bulk_copy.then(|| BulkCopyTemps {
                request: alloc_local(&mut local_types, ValType::I64),
                fuel_bytes: alloc_local(&mut local_types, ValType::I64),
                result: alloc_local(&mut local_types, ValType::I64),
            });
            let has_member_range = structured_cfg
                && regions
                    .iter()
                    .any(|(region, _)| outlined_member_range_plan(region, layout).is_some());
            let member_range = has_member_range.then(|| MemberRangeTemps {
                start: alloc_local(&mut local_types, ValType::I64),
                load_linear: alloc_local(&mut local_types, ValType::I32),
                store_linear: alloc_local(&mut local_types, ValType::I32),
            });
            Some(MemoryTemps {
                index: alloc_local(&mut local_types, ValType::I32),
                offset: alloc_local(&mut local_types, ValType::I64),
                context: alloc_local(&mut local_types, ValType::I64),
                page,
                load_cache,
                store_cache,
                copy,
                bulk_copy,
                member_range,
            })
        } else {
            None
        };
    let selector_local = alloc_local(&mut local_types, ValType::I32);
    let hops_local = alloc_local(&mut local_types, ValType::I32);
    let chain_start_local =
        (helpers.chain || helpers.tail_chain).then(|| alloc_local(&mut local_types, ValType::I64));
    let tail_dispatch_local = helpers
        .tail_chain
        .then(|| alloc_local(&mut local_types, ValType::I32));
    let vector_status = regions
        .iter()
        .any(|(region, _)| region.has_vector_helper())
        .then(|| alloc_local(&mut local_types, ValType::I32));
    let vector_temps = allocate_vector_temps(
        &mut local_types,
        regions
            .iter()
            .any(|(region, _)| region_has_direct_vector(region, layout)),
        regions
            .iter()
            .any(|(region, _)| region_has_unmasked_unit_stride(region, layout)),
        memory_temps.map(|temps| temps.context),
        layout.sys.is_some(),
    );
    let mut function = Function::new_with_locals_types(local_types);

    emit_memory_context_init(&mut function, layout, memory_temps);
    emit_vector_memory_context_init(&mut function, layout, vector_temps, memory_temps);
    if layout.sys.is_some() {
        emit_vector_unit_load_cache_invalidate(&mut function, vector_temps);
    }
    emit_vector_unit_lmul1_refresh(&mut function, layout, vector_temps);
    emit_cached_state_load(&mut function, layout, &state);
    if let Some(chain_start) = chain_start_local {
        function.instruction(&Instruction::LocalGet(state.retired));
        function.instruction(&Instruction::LocalSet(chain_start));
    }
    if let Some(temps) = memory_temps {
        // A logical page is at most 2^(64-page_shift)-1, so -1 is an
        // impossible tag for every validated system page geometry.
        for cache in [temps.load_cache, temps.store_cache].into_iter().flatten() {
            function.instruction(&Instruction::I64Const(-1));
            function.instruction(&Instruction::LocalSet(cache.page));
        }
    }
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(hops_local));
    if structured_cfg {
        emit_cached_structured_cfg(
            &mut function,
            regions,
            &local_maps,
            layout,
            helpers,
            memory_temps,
            &state,
            &fallback_indices,
            selector_local,
            hops_local,
            vector_status,
            vector_temps,
        )?;
    } else if direct_dispatch {
        emit_cached_direct_dispatch(
            &mut function,
            regions,
            &local_maps,
            layout,
            helpers,
            memory_temps,
            &state,
            selector_local,
            hops_local,
            vector_status,
            vector_temps,
        )?;
    } else {
        function.instruction(&Instruction::Block(BlockType::Empty));
        function.instruction(&Instruction::Loop(BlockType::Empty));
        function.instruction(&Instruction::I32Const(0));
        function.instruction(&Instruction::LocalSet(selector_local));

        let mut order: Vec<usize> = (0..regions.len()).collect();
        order.sort_unstable_by_key(|&index| regions[index].0.entry_pc);
        emit_cached_dispatch_tree(
            &mut function,
            &order,
            regions,
            &local_maps,
            layout,
            helpers,
            memory_temps,
            &state,
            selector_local,
            vector_status,
            vector_temps,
        )?;

        function.instruction(&Instruction::LocalGet(selector_local));
        function.instruction(&Instruction::I32Eqz);
        function.instruction(&Instruction::BrIf(1));
        if let Some(fuel) = state.fuel {
            function.instruction(&Instruction::LocalGet(state.retired));
            function.instruction(&Instruction::LocalGet(fuel));
            function.instruction(&Instruction::I64GeU);
            function.instruction(&Instruction::BrIf(1));
        } else {
            function.instruction(&Instruction::LocalGet(hops_local));
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::I32Add);
            function.instruction(&Instruction::LocalTee(hops_local));
            function.instruction(&Instruction::I32Const(MULTI_ENTRY_HOP_CAP));
            function.instruction(&Instruction::I32GeU);
            function.instruction(&Instruction::BrIf(1));
        }
        function.instruction(&Instruction::Br(0));
        function.instruction(&Instruction::End);
        function.instruction(&Instruction::End);
    }

    emit_cached_state_commit(&mut function, layout, &state);
    if let (Some(chain_start), Some(chain_index)) = (chain_start_local, helpers.chain_index()) {
        // Never recurse after a precise first-instruction side exit: without
        // this progress check the same entry would call itself until the
        // defensive runtime depth cap on every TLB/MMIO miss.
        function.instruction(&Instruction::LocalGet(state.retired));
        function.instruction(&Instruction::LocalGet(chain_start));
        function.instruction(&Instruction::I64GtU);
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::LocalGet(0));
        function.instruction(&Instruction::Call(chain_index));
        function.instruction(&Instruction::End);
    }
    if let (Some(chain_start), Some(dispatch_local), Some(fuel), Some(tail_chain_index)) = (
        chain_start_local,
        tail_dispatch_local,
        state.fuel,
        helpers.tail_chain_index(),
    ) {
        emit_region_tail_chain(
            &mut function,
            layout,
            &state,
            chain_start,
            dispatch_local,
            fuel,
            tail_chain_index,
        );
    }
    function.instruction(&Instruction::End);
    Ok(finish_shared_module(
        function,
        fallbacks,
        helpers,
        regions.len() as u32,
        export_members,
        layout,
    ))
}

fn emit_cached_state_load(function: &mut Function, layout: JitLayout, state: &CachedStateLocals) {
    if !state.lazy() {
        for (reg, local) in state.x.iter().copied().enumerate() {
            if let Some(local) = local {
                function.instruction(&Instruction::I32Const(layout.x_base as i32));
                function.instruction(&Instruction::I64Load(memarg(3, reg as u64 * 8)));
                function.instruction(&Instruction::LocalSet(local));
            }
        }
        for (reg, local) in state.f.iter().copied().enumerate() {
            if let Some(local) = local {
                function.instruction(&Instruction::I32Const(layout.f_base as i32));
                function.instruction(&Instruction::I64Load(memarg(3, reg as u64 * 8)));
                function.instruction(&Instruction::LocalSet(local));
            }
        }
        if let Some(local) = state.fcsr {
            function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
            function.instruction(&Instruction::I32Load(memarg(2, 0)));
            function.instruction(&Instruction::LocalSet(local));
        }
    }
    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalSet(state.pc));
    function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalSet(state.retired));
    if let Some(local) = state.fuel {
        function.instruction(&Instruction::I32Const(layout.fuel_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::LocalSet(local));
    }
}

/// An RVV instruction may update any scalar register or fcsr. Eager cached
/// modules reload their allocated architectural union immediately; lazy
/// modules clear validity so each later read fetches canonical helper state.
/// PC, retirement, fuel, and translation context remain owned by the enclosing
/// generated invocation and are deliberately not reloaded here.
fn emit_cached_state_reload_after_vector(
    function: &mut Function,
    layout: JitLayout,
    state: &CachedStateLocals,
) {
    if state.lazy() {
        if let Some(valid) = state.valid_x {
            function.instruction(&Instruction::I64Const(0));
            function.instruction(&Instruction::LocalSet(valid));
        }
        if let Some(valid) = state.valid_f {
            function.instruction(&Instruction::I64Const(0));
            function.instruction(&Instruction::LocalSet(valid));
        }
        if let Some(valid) = state.valid_fcsr {
            function.instruction(&Instruction::I32Const(0));
            function.instruction(&Instruction::LocalSet(valid));
        }
        return;
    }

    for (reg, local) in state.x.iter().copied().enumerate() {
        if let Some(local) = local {
            function.instruction(&Instruction::I32Const(layout.x_base as i32));
            function.instruction(&Instruction::I64Load(memarg(3, reg as u64 * 8)));
            function.instruction(&Instruction::LocalSet(local));
        }
    }
    for (reg, local) in state.f.iter().copied().enumerate() {
        if let Some(local) = local {
            function.instruction(&Instruction::I32Const(layout.f_base as i32));
            function.instruction(&Instruction::I64Load(memarg(3, reg as u64 * 8)));
            function.instruction(&Instruction::LocalSet(local));
        }
    }
    if let Some(local) = state.fcsr {
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        function.instruction(&Instruction::I32Load(memarg(2, 0)));
        function.instruction(&Instruction::LocalSet(local));
    }
}

/// A directly lowered vector instruction cannot modify scalar or FP state,
/// but the precise pre-instruction publication may have written values that
/// were produced earlier in the current member while the function-wide cache
/// still holds the member-entry value. Reconcile exactly those published
/// values without reloading the entire architectural union from memory.
fn emit_cached_state_reconcile_after_direct_vector(
    function: &mut Function,
    state: &CachedStateLocals,
    local_map: &[Option<u32>],
    exit: &SideExit,
) -> Result<(), EmitError> {
    for &(reg, value) in &exit.outputs {
        let Some(state_local) = state.x[reg as usize] else {
            continue;
        };
        let value_local = local_map[value.0]
            .ok_or_else(|| EmitError("missing direct-vector integer exit value".into()))?;
        function.instruction(&Instruction::LocalGet(value_local));
        function.instruction(&Instruction::LocalSet(state_local));
        if let Some(valid) = state.valid_x {
            emit_mark_valid_i64(function, valid, reg as usize);
        }
    }
    for &(reg, value) in &exit.f_outputs {
        let Some(state_local) = state.f[reg as usize] else {
            continue;
        };
        let value_local = local_map[value.0]
            .ok_or_else(|| EmitError("missing direct-vector FP exit value".into()))?;
        function.instruction(&Instruction::LocalGet(value_local));
        function.instruction(&Instruction::LocalSet(state_local));
        if let Some(valid) = state.valid_f {
            emit_mark_valid_i64(function, valid, reg as usize);
        }
    }
    if let (Some(value), Some(state_local)) = (exit.fcsr_output, state.fcsr) {
        let value_local = local_map[value.0]
            .ok_or_else(|| EmitError("missing direct-vector fcsr exit value".into()))?;
        function.instruction(&Instruction::LocalGet(value_local));
        function.instruction(&Instruction::LocalSet(state_local));
        if let Some(valid) = state.valid_fcsr {
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::LocalSet(valid));
        }
    }
    Ok(())
}

fn emit_cached_state_commit(function: &mut Function, layout: JitLayout, state: &CachedStateLocals) {
    for (reg, local) in state.x.iter().copied().enumerate() {
        let Some(local) = local else { continue };
        if state.write_x & (1u32 << reg) == 0 {
            continue;
        }
        if let Some(valid) = state.valid_x {
            emit_valid_i64(function, valid, reg);
            function.instruction(&Instruction::If(BlockType::Empty));
        }
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        function.instruction(&Instruction::LocalGet(local));
        function.instruction(&Instruction::I64Store(memarg(3, reg as u64 * 8)));
        if state.valid_x.is_some() {
            function.instruction(&Instruction::End);
        }
    }
    for (reg, local) in state.f.iter().copied().enumerate() {
        let Some(local) = local else { continue };
        if state.write_f & (1u32 << reg) == 0 {
            continue;
        }
        if let Some(valid) = state.valid_f {
            emit_valid_i64(function, valid, reg);
            function.instruction(&Instruction::If(BlockType::Empty));
        }
        function.instruction(&Instruction::I32Const(layout.f_base as i32));
        function.instruction(&Instruction::LocalGet(local));
        function.instruction(&Instruction::I64Store(memarg(3, reg as u64 * 8)));
        if state.valid_f.is_some() {
            function.instruction(&Instruction::End);
        }
    }
    if let Some(local) = state.fcsr.filter(|_| state.write_fcsr) {
        if let Some(valid) = state.valid_fcsr {
            function.instruction(&Instruction::LocalGet(valid));
            function.instruction(&Instruction::If(BlockType::Empty));
        }
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        function.instruction(&Instruction::LocalGet(local));
        function.instruction(&Instruction::I32Store(memarg(2, 0)));
        if state.valid_fcsr.is_some() {
            function.instruction(&Instruction::End);
        }
    }
    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
    function.instruction(&Instruction::LocalGet(state.pc));
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
    function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
    function.instruction(&Instruction::LocalGet(state.retired));
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
}

/// Continue at an already-published generated entry without growing the Wasm
/// call stack. The dispatch line is only a fast-path hint: matching the full
/// architectural PC and the current mapping generation is the same proof the
/// outer runtime requires before entering generated code. Any miss simply
/// returns to that runtime, which owns mapping validation and compilation.
fn emit_region_tail_chain(
    function: &mut Function,
    layout: JitLayout,
    state: &CachedStateLocals,
    chain_start: u32,
    dispatch_local: u32,
    fuel: u32,
    tail_chain_index: u32,
) {
    debug_assert_ne!(layout.dispatch_base, 0);
    debug_assert_ne!(layout.map_gen_addr, 0);
    debug_assert_ne!(layout.chain_hops_addr, 0);

    // A precise side exit can retire zero instructions. It must return to T0
    // instead of tail-calling the same entry forever. Fuel is cumulative for
    // the complete chain, so a transfer cannot evade the scheduler budget.
    function.instruction(&Instruction::LocalGet(state.retired));
    function.instruction(&Instruction::LocalGet(chain_start));
    function.instruction(&Instruction::I64GtU);
    function.instruction(&Instruction::LocalGet(state.retired));
    function.instruction(&Instruction::LocalGet(fuel));
    function.instruction(&Instruction::I64LtU);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::If(BlockType::Empty));

    // Fold the next bank of PC bits into the low direct-map index.  This keeps
    // one lookup while preventing equally aligned functions in independently
    // randomized mappings from deterministically evicting each other.
    function.instruction(&Instruction::LocalGet(state.pc));
    function.instruction(&Instruction::I64Const(1));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::LocalGet(state.pc));
    function.instruction(&Instruction::I64Const(i64::from(
        layout.dispatch_mask.count_ones() + 1,
    )));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Xor);
    function.instruction(&Instruction::I64Const(i64::from(layout.dispatch_mask)));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::I32Const(4));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Const(layout.dispatch_base as i32));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalSet(dispatch_local));

    // Full PC tag, live map generation, non-sentinel generation, and a
    // published non-negative table index jointly authorize the fast transfer.
    function.instruction(&Instruction::LocalGet(dispatch_local));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalGet(state.pc));
    function.instruction(&Instruction::I64Eq);

    function.instruction(&Instruction::LocalGet(dispatch_local));
    function.instruction(&Instruction::I32Load(memarg(2, 12)));
    function.instruction(&Instruction::I32Const(layout.map_gen_addr as i32));
    function.instruction(&Instruction::I32Load(memarg(2, 0)));
    function.instruction(&Instruction::I32Eq);
    function.instruction(&Instruction::I32And);

    function.instruction(&Instruction::LocalGet(dispatch_local));
    function.instruction(&Instruction::I32Load(memarg(2, 12)));
    function.instruction(&Instruction::I32Const(-1));
    function.instruction(&Instruction::I32Ne);
    function.instruction(&Instruction::I32And);

    function.instruction(&Instruction::LocalGet(dispatch_local));
    function.instruction(&Instruction::I32Load(memarg(2, 8)));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::I32GeS);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::If(BlockType::Empty));

    function.instruction(&Instruction::I32Const(layout.chain_hops_addr as i32));
    function.instruction(&Instruction::I32Const(layout.chain_hops_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Const(1));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::I64Store(memarg(3, 0)));

    // Tail-call the single host-created trampoline. It alone imports the
    // shared function table and performs return_call_indirect, so generated
    // modules remain table-independent and table.set does not become
    // O(generated instances) in V8. Clearing SB_IDX_BIT converts the runtime's
    // region-attribution tag back to the actual function-table slot.
    function.instruction(&Instruction::LocalGet(0));
    function.instruction(&Instruction::LocalGet(dispatch_local));
    function.instruction(&Instruction::I32Load(memarg(2, 8)));
    function.instruction(&Instruction::I32Const(!crate::SB_IDX_BIT));
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::ReturnCall(tail_chain_index));
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);
}

#[allow(clippy::too_many_arguments)]
fn emit_cached_direct_dispatch(
    function: &mut Function,
    regions: &[(&Region, Option<LoopBackedge>)],
    local_maps: &[Vec<Option<u32>>],
    layout: JitLayout,
    helpers: HelperImports,
    memory_temps: Option<MemoryTemps>,
    state: &CachedStateLocals,
    selector_local: u32,
    hops_local: u32,
    vector_status: Option<u32>,
    vector_temps: Option<VectorTemps>,
) -> Result<(), EmitError> {
    function.instruction(&Instruction::I32Const(-1));
    function.instruction(&Instruction::LocalSet(selector_local));
    function.instruction(&Instruction::Block(BlockType::Empty));
    function.instruction(&Instruction::Loop(BlockType::Empty));

    // A known covered successor leaves its dense member index in the local.
    // Only an external/dynamic successor pays the PC-to-index comparison tree.
    function.instruction(&Instruction::LocalGet(selector_local));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::I32LtS);
    function.instruction(&Instruction::If(BlockType::Empty));
    let mut order: Vec<usize> = (0..regions.len()).collect();
    order.sort_unstable_by_key(|&index| regions[index].0.entry_pc);
    emit_cached_index_tree(function, &order, regions, layout, state, selector_local);
    function.instruction(&Instruction::End);

    // Classic structured switch: target i leaves the i innermost case blocks,
    // landing immediately before body i. The default leaves the outer exit
    // block, after which the common architectural commit runs.
    for _ in regions {
        function.instruction(&Instruction::Block(BlockType::Empty));
    }
    let targets: Vec<u32> = (0..regions.len() as u32).collect();
    function.instruction(&Instruction::LocalGet(selector_local));
    function.instruction(&Instruction::BrTable(
        targets.into(),
        regions.len() as u32 + 1,
    ));

    for (index, &(region, loop_backedge)) in regions.iter().enumerate() {
        function.instruction(&Instruction::End);
        emit_cached_body(
            function,
            region,
            layout,
            helpers,
            memory_temps,
            state,
            &local_maps[index],
            loop_backedge,
            None,
            vector_status,
            vector_temps,
        )?;

        let member_for = |value: ValueId| {
            let pc = static_guest_pc(region, value)?;
            regions
                .iter()
                .position(|(candidate, _)| candidate.entry_pc == pc)
                .map(|member| member as i32)
        };
        if let Some(pc) = static_guest_pc(region, region.next_pc) {
            let successor = regions
                .iter()
                .position(|(candidate, _)| candidate.entry_pc == pc)
                .map(|member| member as i32)
                .unwrap_or(-1);
            function.instruction(&Instruction::I32Const(successor));
        } else {
            match region.values.get(region.next_pc.0).map(|value| &value.op) {
                Some(Op::SelectI64 {
                    condition,
                    if_true,
                    if_false,
                }) => {
                    // A CFG basic block exposes both conditional successors as
                    // constants. Select their dense indices directly; an uncovered
                    // side uses -1 and falls through the exact PC lookup/exit path.
                    function
                        .instruction(&Instruction::I32Const(member_for(*if_true).unwrap_or(-1)));
                    function
                        .instruction(&Instruction::I32Const(member_for(*if_false).unwrap_or(-1)));
                    function.instruction(&Instruction::LocalGet(
                        local_maps[index][condition.0].expect("cached branch condition local"),
                    ));
                    function.instruction(&Instruction::Select);
                }
                _ => {
                    function.instruction(&Instruction::I32Const(-1));
                }
            }
        }
        function.instruction(&Instruction::LocalSet(selector_local));

        let loop_depth = (regions.len() - 1 - index) as u32;
        let exit_depth = loop_depth + 1;
        if let Some(fuel) = state.fuel {
            function.instruction(&Instruction::LocalGet(state.retired));
            function.instruction(&Instruction::LocalGet(fuel));
            function.instruction(&Instruction::I64GeU);
            function.instruction(&Instruction::BrIf(exit_depth));
        } else {
            function.instruction(&Instruction::LocalGet(hops_local));
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::I32Add);
            function.instruction(&Instruction::LocalTee(hops_local));
            function.instruction(&Instruction::I32Const(MULTI_ENTRY_HOP_CAP));
            function.instruction(&Instruction::I32GeU);
            function.instruction(&Instruction::BrIf(exit_depth));
        }
        function.instruction(&Instruction::Br(loop_depth));
    }

    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);
    Ok(())
}

const STRUCTURED_CFG_DUPLICATION_LIMIT: usize = 250;

#[derive(Clone, Copy, Debug)]
struct StructuredDestination {
    scope: u32,
    selector: Option<i32>,
}

enum StructuredWork {
    Node(Structure),
    End {
        scope: u32,
        targets: Vec<usize>,
        previous: Vec<Option<StructuredDestination>>,
    },
}

fn cfg_member_for_value(
    regions: &[(&Region, Option<LoopBackedge>)],
    region: &Region,
    value: ValueId,
) -> Option<usize> {
    let pc = static_guest_pc(region, value)?;
    regions
        .iter()
        .position(|(candidate, _)| candidate.entry_pc == pc)
}

fn cfg_successors(regions: &[(&Region, Option<LoopBackedge>)]) -> Vec<Vec<usize>> {
    regions
        .iter()
        .map(|(region, _)| {
            let mut successors = Vec::with_capacity(2);
            match region.values.get(region.next_pc.0).map(|value| &value.op) {
                Some(Op::ConstI64(_) | Op::GuestPc(_)) => {
                    if let Some(member) = cfg_member_for_value(regions, region, region.next_pc) {
                        successors.push(member);
                    }
                }
                Some(Op::SelectI64 {
                    if_true, if_false, ..
                }) => {
                    for value in [*if_true, *if_false] {
                        if let Some(member) = cfg_member_for_value(regions, region, value) {
                            if !successors.contains(&member) {
                                successors.push(member);
                            }
                        }
                    }
                }
                _ => {}
            }
            successors
        })
        .collect()
}

fn structured_depth(active_scopes: &[u32], scope: u32) -> Result<u32, EmitError> {
    active_scopes
        .iter()
        .rev()
        .position(|active| *active == scope)
        .map(|depth| depth as u32)
        .ok_or_else(|| EmitError("structured CFG branch target is not active".into()))
}

fn emit_structured_unconditional_branch(
    function: &mut Function,
    destination: StructuredDestination,
    selector_local: u32,
    active_scopes: &[u32],
) -> Result<(), EmitError> {
    if let Some(selector) = destination.selector {
        function.instruction(&Instruction::I32Const(selector));
        function.instruction(&Instruction::LocalSet(selector_local));
    }
    function.instruction(&Instruction::Br(structured_depth(
        active_scopes,
        destination.scope,
    )?));
    Ok(())
}

fn emit_structured_conditional_branch(
    function: &mut Function,
    condition_local: u32,
    invert: bool,
    destination: StructuredDestination,
    selector_local: u32,
    active_scopes: &[u32],
) -> Result<(), EmitError> {
    function.instruction(&Instruction::LocalGet(condition_local));
    if invert {
        function.instruction(&Instruction::I32Eqz);
    }
    if let Some(selector) = destination.selector {
        // The selector write must happen only on the taken arm. The temporary
        // `if` adds one control depth around the requested target.
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::I32Const(selector));
        function.instruction(&Instruction::LocalSet(selector_local));
        function.instruction(&Instruction::Br(
            structured_depth(active_scopes, destination.scope)? + 1,
        ));
        function.instruction(&Instruction::End);
    } else {
        function.instruction(&Instruction::BrIf(structured_depth(
            active_scopes,
            destination.scope,
        )?));
    }
    Ok(())
}

fn emit_structured_selector_branch(
    function: &mut Function,
    member: usize,
    destination: StructuredDestination,
    selector_local: u32,
    active_scopes: &[u32],
) -> Result<(), EmitError> {
    function.instruction(&Instruction::LocalGet(selector_local));
    function.instruction(&Instruction::I32Const(member as i32));
    function.instruction(&Instruction::I32Eq);
    if let Some(selector) = destination.selector {
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::I32Const(selector));
        function.instruction(&Instruction::LocalSet(selector_local));
        function.instruction(&Instruction::Br(
            structured_depth(active_scopes, destination.scope)? + 1,
        ));
        function.instruction(&Instruction::End);
    } else {
        function.instruction(&Instruction::BrIf(structured_depth(
            active_scopes,
            destination.scope,
        )?));
    }
    Ok(())
}

fn emit_structured_safety(
    function: &mut Function,
    state: &CachedStateLocals,
    hops_local: u32,
    exit_scope: u32,
    active_scopes: &[u32],
) -> Result<(), EmitError> {
    let exit_depth = structured_depth(active_scopes, exit_scope)?;
    if let Some(fuel) = state.fuel {
        function.instruction(&Instruction::LocalGet(state.retired));
        function.instruction(&Instruction::LocalGet(fuel));
        function.instruction(&Instruction::I64GeU);
        function.instruction(&Instruction::BrIf(exit_depth));
    } else {
        function.instruction(&Instruction::LocalGet(hops_local));
        function.instruction(&Instruction::I32Const(1));
        function.instruction(&Instruction::I32Add);
        function.instruction(&Instruction::LocalTee(hops_local));
        function.instruction(&Instruction::I32Const(MULTI_ENTRY_HOP_CAP));
        function.instruction(&Instruction::I32GeU);
        function.instruction(&Instruction::BrIf(exit_depth));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum StructuredPath {
    Fallthrough { selector: Option<i32> },
    Branch(StructuredDestination),
}

fn structured_path(
    member: Option<usize>,
    next_heads: &[usize],
    labels: &[Option<StructuredDestination>],
    exit: StructuredDestination,
) -> Result<StructuredPath, EmitError> {
    let Some(member) = member else {
        return Ok(StructuredPath::Branch(exit));
    };
    if next_heads.contains(&member) {
        return Ok(StructuredPath::Fallthrough {
            selector: (next_heads.len() > 1).then_some(member as i32),
        });
    }
    Ok(StructuredPath::Branch(labels[member].ok_or_else(|| {
        EmitError(format!(
            "structured CFG has no active label for member {member}"
        ))
    })?))
}

fn emit_structured_fallthrough_selector(
    function: &mut Function,
    selector: Option<i32>,
    selector_local: u32,
) {
    if let Some(selector) = selector {
        function.instruction(&Instruction::I32Const(selector));
        function.instruction(&Instruction::LocalSet(selector_local));
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_structured_successor(
    function: &mut Function,
    regions: &[(&Region, Option<LoopBackedge>)],
    region_index: usize,
    local_map: &[Option<u32>],
    next_heads: &[usize],
    labels: &[Option<StructuredDestination>],
    selector_local: u32,
    main_scope: u32,
    exit_scope: u32,
    active_scopes: &[u32],
) -> Result<(), EmitError> {
    let region = regions[region_index].0;
    let exit = StructuredDestination {
        scope: exit_scope,
        selector: None,
    };
    match region.values.get(region.next_pc.0).map(|value| &value.op) {
        Some(Op::ConstI64(_) | Op::GuestPc(_)) => {
            let member = cfg_member_for_value(regions, region, region.next_pc);
            match structured_path(member, next_heads, labels, exit)? {
                StructuredPath::Fallthrough { selector } => {
                    emit_structured_fallthrough_selector(function, selector, selector_local);
                }
                StructuredPath::Branch(destination) => emit_structured_unconditional_branch(
                    function,
                    destination,
                    selector_local,
                    active_scopes,
                )?,
            }
        }
        Some(Op::SelectI64 {
            condition,
            if_true,
            if_false,
        }) => {
            let condition_local = local_map[condition.0]
                .ok_or_else(|| EmitError("missing structured branch condition local".into()))?;
            let if_true = structured_path(
                cfg_member_for_value(regions, region, *if_true),
                next_heads,
                labels,
                exit,
            )?;
            let if_false = structured_path(
                cfg_member_for_value(regions, region, *if_false),
                next_heads,
                labels,
                exit,
            )?;
            match (if_true, if_false) {
                (
                    StructuredPath::Fallthrough {
                        selector: true_selector,
                    },
                    StructuredPath::Fallthrough {
                        selector: false_selector,
                    },
                ) => match (true_selector, false_selector) {
                    (Some(if_true), Some(if_false)) if if_true != if_false => {
                        function.instruction(&Instruction::I32Const(if_true));
                        function.instruction(&Instruction::I32Const(if_false));
                        function.instruction(&Instruction::LocalGet(condition_local));
                        function.instruction(&Instruction::Select);
                        function.instruction(&Instruction::LocalSet(selector_local));
                    }
                    (selector, _) => {
                        emit_structured_fallthrough_selector(function, selector, selector_local)
                    }
                },
                (StructuredPath::Fallthrough { selector }, StructuredPath::Branch(destination)) => {
                    emit_structured_conditional_branch(
                        function,
                        condition_local,
                        true,
                        destination,
                        selector_local,
                        active_scopes,
                    )?;
                    emit_structured_fallthrough_selector(function, selector, selector_local);
                }
                (StructuredPath::Branch(destination), StructuredPath::Fallthrough { selector }) => {
                    emit_structured_conditional_branch(
                        function,
                        condition_local,
                        false,
                        destination,
                        selector_local,
                        active_scopes,
                    )?;
                    emit_structured_fallthrough_selector(function, selector, selector_local);
                }
                (StructuredPath::Branch(if_true), StructuredPath::Branch(if_false)) => {
                    emit_structured_conditional_branch(
                        function,
                        condition_local,
                        false,
                        if_true,
                        selector_local,
                        active_scopes,
                    )?;
                    emit_structured_unconditional_branch(
                        function,
                        if_false,
                        selector_local,
                        active_scopes,
                    )?;
                }
            }
        }
        _ => {
            // Dynamic control can re-enter this module, but only through the
            // single outer entry dispatcher. It resolves the exact PC on the
            // next loop iteration and exits if the destination is external.
            function.instruction(&Instruction::I32Const(-1));
            function.instruction(&Instruction::LocalSet(selector_local));
            function.instruction(&Instruction::Br(structured_depth(
                active_scopes,
                main_scope,
            )?));
        }
    }
    Ok(())
}

fn emit_member_range_row_probe(
    function: &mut Function,
    memory: SystemMemory,
    row: TranslationRow,
    temps: MemoryTemps,
    range: MemberRangeTemps,
    linear_local: u32,
) {
    emit_translation_probe(function, row, temps, range.start, memory.page_shift);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    function.instruction(&Instruction::LocalGet(range.start));
    emit_translation_offset(function, row, temps);
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::LocalSet(linear_local));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::End);
}

fn emit_member_range_condition(
    function: &mut Function,
    region: &Region,
    plan: MemberRangePlan,
    layout: JitLayout,
    memory_temps: MemoryTemps,
    state: &CachedStateLocals,
) -> Result<(), EmitError> {
    let memory = layout
        .sys
        .ok_or_else(|| EmitError("member range requires system memory".into()))?;
    let range = memory_temps
        .member_range
        .ok_or_else(|| EmitError("member range lacks allocated temporaries".into()))?;
    let root_reg = plan
        .root_reg(region)
        .ok_or_else(|| EmitError("member range root is not an architectural register".into()))?;
    if let Some(root_local) = state.x[usize::from(root_reg)] {
        emit_lazy_state_read_i64(
            function,
            layout.x_base,
            root_local,
            state.valid_x,
            usize::from(root_reg),
        );
    } else if is_materialized_x(state, root_reg) {
        // Hybrid structured state keeps nonresident registers canonical at
        // every member boundary, which is exactly where this guard executes.
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        function.instruction(&Instruction::I64Load(memarg(3, u64::from(root_reg) * 8)));
    } else {
        return Err(EmitError(format!(
            "member range root x{root_reg} has no cached representation"
        )));
    }
    if plan.min_offset != 0 {
        function.instruction(&Instruction::I64Const(plan.min_offset));
        function.instruction(&Instruction::I64Add);
    }
    function.instruction(&Instruction::LocalSet(range.start));

    let page_bytes = 1u64
        .checked_shl(u32::from(memory.page_shift))
        .ok_or_else(|| EmitError("invalid member-range page size".into()))?;
    let last_start = page_bytes
        .checked_sub(plan.span)
        .ok_or_else(|| EmitError("member range exceeds the system page".into()))?;
    function.instruction(&Instruction::LocalGet(range.start));
    function.instruction(&Instruction::I64Const((page_bytes - 1) as i64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(last_start as i64));
    function.instruction(&Instruction::I64LeU);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));

    // This proof derives its page from the selected architectural root. A page
    // local allocated for an unrelated dense member can hold stale scratch.
    let mut probe_temps = memory_temps;
    probe_temps.page = None;
    emit_translation_index(function, memory, probe_temps, range.start);
    if plan.loads != 0 {
        emit_member_range_row_probe(
            function,
            memory,
            memory.load,
            probe_temps,
            range,
            range.load_linear,
        );
        if plan.stores != 0 {
            function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
            emit_member_range_row_probe(
                function,
                memory,
                memory.store,
                probe_temps,
                range,
                range.store_linear,
            );
            function.instruction(&Instruction::Else);
            function.instruction(&Instruction::I32Const(0));
            function.instruction(&Instruction::End);
        }
    } else {
        emit_member_range_row_probe(
            function,
            memory,
            memory.store,
            probe_temps,
            range,
            range.store_linear,
        );
    }
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::End);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_cached_structured_member(
    function: &mut Function,
    region: &Region,
    layout: JitLayout,
    helpers: HelperImports,
    memory_temps: Option<MemoryTemps>,
    state: &CachedStateLocals,
    local_map: &[Option<u32>],
    fallback_index: Option<u32>,
    vector_status: Option<u32>,
    vector_temps: Option<VectorTemps>,
) -> Result<(), EmitError> {
    let plan = outlined_member_range_plan(region, layout);
    let range = memory_temps.and_then(|temps| temps.member_range);
    if let (Some(plan), Some(range), Some(memory_temps), Some(fallback_index)) =
        (plan, range, memory_temps, fallback_index)
    {
        emit_member_range_condition(function, region, plan, layout, memory_temps, state)?;
        function.instruction(&Instruction::If(BlockType::Empty));
        emit_cached_body(
            function,
            region,
            layout,
            helpers,
            Some(memory_temps),
            state,
            local_map,
            None,
            Some((plan, range)),
            vector_status,
            vector_temps,
        )?;
        function.instruction(&Instruction::Else);
        // Keep the ordinary translation/refill/fault path out of the hot
        // structured function. Canonical state is exact at the call boundary;
        // the private function executes this one member and returns directly
        // to the system scheduler.
        emit_cached_state_commit(function, layout, state);
        function.instruction(&Instruction::LocalGet(0));
        function.instruction(&Instruction::Call(fallback_index));
        function.instruction(&Instruction::Return);
        function.instruction(&Instruction::End);
        Ok(())
    } else if plan.is_none() && fallback_index.is_none() {
        emit_cached_body(
            function,
            region,
            layout,
            helpers,
            memory_temps,
            state,
            local_map,
            None,
            None,
            vector_status,
            vector_temps,
        )
    } else {
        Err(EmitError(
            "outlined member-range plan and fallback allocation disagree".into(),
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_cached_structured_cfg(
    function: &mut Function,
    regions: &[(&Region, Option<LoopBackedge>)],
    local_maps: &[Vec<Option<u32>>],
    layout: JitLayout,
    helpers: HelperImports,
    memory_temps: Option<MemoryTemps>,
    state: &CachedStateLocals,
    fallback_indices: &[Option<u32>],
    selector_local: u32,
    hops_local: u32,
    vector_status: Option<u32>,
    vector_temps: Option<VectorTemps>,
) -> Result<(), EmitError> {
    let successors = cfg_successors(regions);
    let entries: Vec<usize> = (0..regions.len()).collect();
    let structures = structure::stackify(&successors, &entries, STRUCTURED_CFG_DUPLICATION_LIMIT);

    function.instruction(&Instruction::I32Const(-1));
    function.instruction(&Instruction::LocalSet(selector_local));

    let exit_scope = 0u32;
    let main_scope = 1u32;
    let mut next_scope = 2u32;
    let mut active_scopes = vec![exit_scope, main_scope];
    function.instruction(&Instruction::Block(BlockType::Empty));
    function.instruction(&Instruction::Loop(BlockType::Empty));

    // A direct structured edge already carries a dense selector when one is
    // required. Initial and dynamic entries resolve the architectural PC once.
    function.instruction(&Instruction::LocalGet(selector_local));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::I32LtS);
    function.instruction(&Instruction::If(BlockType::Empty));
    let mut pc_order: Vec<usize> = (0..regions.len()).collect();
    pc_order.sort_unstable_by_key(|&index| regions[index].0.entry_pc);
    emit_cached_index_tree(function, &pc_order, regions, layout, state, selector_local);
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::LocalGet(selector_local));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::I32LtS);
    function.instruction(&Instruction::BrIf(structured_depth(
        &active_scopes,
        exit_scope,
    )?));

    let mut labels = vec![None; regions.len()];
    let mut work: VecDeque<StructuredWork> =
        structures.into_iter().map(StructuredWork::Node).collect();

    while let Some(item) = work.pop_front() {
        let next_heads = work.iter().find_map(|pending| match pending {
            StructuredWork::Node(structure) => Some(structure.head()),
            StructuredWork::End { .. } => None,
        });
        match item {
            StructuredWork::Node(Structure::Basic(member)) => {
                // Stackification can duplicate a basic member. Instrument the
                // emitted occurrence, rather than its unique IR node, so the
                // counters follow the path the generated Wasm actually takes.
                // A precise side exit can retire a prefix of the scheduled
                // member; `DPROF_REGION_INSNS` remains the authoritative exact
                // retirement total and makes that difference observable.
                emit_structured_profile(function, layout, regions[member].0);
                emit_cached_structured_member(
                    function,
                    regions[member].0,
                    layout,
                    helpers,
                    memory_temps,
                    state,
                    &local_maps[member],
                    fallback_indices[member],
                    vector_status,
                    vector_temps,
                )?;
                emit_structured_safety(function, state, hops_local, exit_scope, &active_scopes)?;
                emit_structured_successor(
                    function,
                    regions,
                    member,
                    &local_maps[member],
                    next_heads.as_deref().unwrap_or(&[]),
                    &labels,
                    selector_local,
                    main_scope,
                    exit_scope,
                    &active_scopes,
                )?;
            }
            StructuredWork::Node(Structure::Dispatcher(dispatch_entries)) => {
                let next_heads = next_heads.unwrap_or_default();
                for member in dispatch_entries {
                    if next_heads.contains(&member) {
                        continue;
                    }
                    let destination = labels[member].ok_or_else(|| {
                        EmitError(format!(
                            "structured dispatcher has no label for member {member}"
                        ))
                    })?;
                    emit_structured_selector_branch(
                        function,
                        member,
                        destination,
                        selector_local,
                        &active_scopes,
                    )?;
                }
                // Valid selectors matching a head reach the next structure by
                // fallthrough. Every module/dynamic entry was range-checked at
                // the main-loop header.
            }
            StructuredWork::Node(Structure::Block(children)) => {
                let targets = next_heads.unwrap_or_default();
                let scope = next_scope;
                next_scope += 1;
                function.instruction(&Instruction::Block(BlockType::Empty));
                active_scopes.push(scope);
                let multi_target = targets.len() > 1;
                let mut previous = Vec::with_capacity(targets.len());
                for &target in &targets {
                    previous.push(labels[target].replace(StructuredDestination {
                        scope,
                        selector: multi_target.then_some(target as i32),
                    }));
                }
                work.push_front(StructuredWork::End {
                    scope,
                    targets,
                    previous,
                });
                for child in children.into_iter().rev() {
                    work.push_front(StructuredWork::Node(child));
                }
            }
            StructuredWork::Node(Structure::Loop(children)) => {
                let targets = children.first().map_or_else(Vec::new, Structure::head);
                if targets.is_empty() {
                    return Err(EmitError("structured CFG contains an empty loop".into()));
                }
                let scope = next_scope;
                next_scope += 1;
                function.instruction(&Instruction::Loop(BlockType::Empty));
                active_scopes.push(scope);
                let multi_target = targets.len() > 1;
                let mut previous = Vec::with_capacity(targets.len());
                for &target in &targets {
                    previous.push(labels[target].replace(StructuredDestination {
                        scope,
                        selector: multi_target.then_some(target as i32),
                    }));
                }
                work.push_front(StructuredWork::End {
                    scope,
                    targets,
                    previous,
                });
                for child in children.into_iter().rev() {
                    work.push_front(StructuredWork::Node(child));
                }
            }
            StructuredWork::End {
                scope,
                targets,
                previous,
            } => {
                if active_scopes.pop() != Some(scope) {
                    return Err(EmitError("unbalanced structured CFG scope".into()));
                }
                for (target, old) in targets.into_iter().zip(previous) {
                    labels[target] = old;
                }
                function.instruction(&Instruction::End);
            }
        }
    }

    if active_scopes != [exit_scope, main_scope] {
        return Err(EmitError("structured CFG left an open scope".into()));
    }
    // Defensive fallthrough: a well-formed terminal basic block has already
    // branched either to a covered successor or the common exact exit.
    function.instruction(&Instruction::Br(structured_depth(
        &active_scopes,
        exit_scope,
    )?));
    active_scopes.pop();
    function.instruction(&Instruction::End);
    active_scopes.pop();
    function.instruction(&Instruction::End);
    Ok(())
}

fn emit_cached_index_tree(
    function: &mut Function,
    order: &[usize],
    regions: &[(&Region, Option<LoopBackedge>)],
    layout: JitLayout,
    state: &CachedStateLocals,
    selector_local: u32,
) {
    if order.len() <= 3 {
        for &index in order {
            function.instruction(&Instruction::LocalGet(state.pc));
            emit_guest_pc(function, regions[index].0.entry_pc, layout);
            function.instruction(&Instruction::I64Eq);
            function.instruction(&Instruction::If(BlockType::Empty));
            function.instruction(&Instruction::I32Const(index as i32));
            function.instruction(&Instruction::LocalSet(selector_local));
            function.instruction(&Instruction::End);
        }
        return;
    }

    let middle = order.len() / 2;
    let pivot = regions[order[middle]].0.entry_pc;
    function.instruction(&Instruction::LocalGet(state.pc));
    emit_guest_pc(function, pivot, layout);
    function.instruction(&Instruction::I64LtU);
    function.instruction(&Instruction::If(BlockType::Empty));
    emit_cached_index_tree(
        function,
        &order[..middle],
        regions,
        layout,
        state,
        selector_local,
    );
    function.instruction(&Instruction::Else);
    emit_cached_index_tree(
        function,
        &order[middle..],
        regions,
        layout,
        state,
        selector_local,
    );
    function.instruction(&Instruction::End);
}

fn emit_valid_i64(function: &mut Function, valid_local: u32, bit: usize) {
    function.instruction(&Instruction::LocalGet(valid_local));
    function.instruction(&Instruction::I64Const(1i64 << bit));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(0));
    function.instruction(&Instruction::I64Ne);
}

fn emit_mark_valid_i64(function: &mut Function, valid_local: u32, bit: usize) {
    function.instruction(&Instruction::LocalGet(valid_local));
    function.instruction(&Instruction::I64Const(1i64 << bit));
    function.instruction(&Instruction::I64Or);
    function.instruction(&Instruction::LocalSet(valid_local));
}

fn emit_lazy_state_read_i64(
    function: &mut Function,
    base: u32,
    state_local: u32,
    valid_local: Option<u32>,
    reg: usize,
) {
    if let Some(valid) = valid_local {
        emit_valid_i64(function, valid, reg);
        function.instruction(&Instruction::I32Eqz);
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::I32Const(base as i32));
        function.instruction(&Instruction::I64Load(memarg(3, reg as u64 * 8)));
        function.instruction(&Instruction::LocalSet(state_local));
        emit_mark_valid_i64(function, valid, reg);
        function.instruction(&Instruction::End);
    }
    function.instruction(&Instruction::LocalGet(state_local));
}

fn is_materialized_x(state: &CachedStateLocals, reg: u8) -> bool {
    state.materialized_x & (1u32 << reg) != 0
}

fn cached_x_member_local(
    region: &Region,
    state: &CachedStateLocals,
    local_map: &[Option<u32>],
    reg: u8,
) -> Result<u32, EmitError> {
    if let Some(local) = state.x[reg as usize] {
        return Ok(local);
    }
    if !is_materialized_x(state, reg) {
        return Err(EmitError(format!(
            "x{reg} has neither resident nor materialized cached state"
        )));
    }
    let value = read_x_value(region, reg)
        .ok_or_else(|| EmitError(format!("materialized x{reg} has no member ReadX value")))?;
    local_map[value.0]
        .ok_or_else(|| EmitError(format!("materialized x{reg} has no member SSA local")))
}

/// Make one cached-member integer input available in its canonical local.
/// Resident state keeps the existing eager/lazy behavior. Materialized state
/// loads directly into the member's ReadX SSA local.
fn emit_cached_x_member_input(
    function: &mut Function,
    region: &Region,
    layout: JitLayout,
    state: &CachedStateLocals,
    local_map: &[Option<u32>],
    reg: u8,
) -> Result<u32, EmitError> {
    let local = cached_x_member_local(region, state, local_map, reg)?;
    if state.x[reg as usize].is_some() {
        emit_lazy_state_read_i64(function, layout.x_base, local, state.valid_x, reg as usize);
        function.instruction(&Instruction::Drop);
    } else {
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        function.instruction(&Instruction::I64Load(memarg(3, u64::from(reg) * 8)));
        function.instruction(&Instruction::LocalSet(local));
    }
    Ok(local)
}

fn emit_cached_vector_x_inputs(
    function: &mut Function,
    layout: JitLayout,
    state: &CachedStateLocals,
    exit: &SideExit,
    direct: VectorDirect,
) -> Result<(), EmitError> {
    let inputs = vector_direct_x_inputs(direct);
    for (slot, reg) in inputs.iter().copied().enumerate() {
        let Some(reg) = reg else { continue };
        if reg == 0 || inputs[..slot].contains(&Some(reg)) {
            continue;
        }
        if exit.outputs.iter().any(|(output, _)| *output == reg) {
            // The scalar epoch publication immediately before this helper has
            // already synchronized the precise current-member value.
            continue;
        }
        let Some(state_local) = state.x[reg as usize] else {
            // Materialized state is synchronized at its defining operation.
            continue;
        };
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        emit_lazy_state_read_i64(
            function,
            layout.x_base,
            state_local,
            state.valid_x,
            reg as usize,
        );
        function.instruction(&Instruction::I64Store(memarg(3, u64::from(reg) * 8)));
    }
    Ok(())
}

/// Feed a unit-stride vector memory operation directly from the cached scalar
/// state when its base is already available as a Wasm local. The cold helper
/// arm still publishes the complete precise side-exit state before use.
fn emit_cached_vector_unit_address(
    function: &mut Function,
    layout: JitLayout,
    state: &CachedStateLocals,
    local_map: &[Option<u32>],
    exit: &SideExit,
    direct: VectorDirect,
    temps: Option<VectorTemps>,
) -> Result<bool, EmitError> {
    let VectorDirect::UnitStride { base, .. } = direct else {
        return Ok(false);
    };
    let Some(temps) = temps else { return Ok(false) };

    // A value produced earlier in this member is newer than the function-wide
    // cached register. Its precise side-exit SSA value is already available.
    if let Some(&(_, value)) = exit.outputs.iter().find(|&&(reg, _)| reg == base) {
        let value_local = local_map[value.0]
            .ok_or_else(|| EmitError("missing vector-memory base exit value".into()))?;
        function.instruction(&Instruction::LocalGet(value_local));
        function.instruction(&Instruction::LocalSet(temps.address));
        return Ok(true);
    }

    let Some(state_local) = state.x[base as usize] else {
        return Ok(false);
    };
    emit_lazy_state_read_i64(
        function,
        layout.x_base,
        state_local,
        state.valid_x,
        base as usize,
    );
    function.instruction(&Instruction::LocalSet(temps.address));
    Ok(true)
}

fn emit_cached_vector_epoch_outputs(
    function: &mut Function,
    layout: JitLayout,
    local_map: &[Option<u32>],
    exit: &SideExit,
) -> Result<(), EmitError> {
    // Vector effects terminate scalar SSA forwarding. Values produced in this
    // member must therefore reach canonical memory even on a direct vector
    // arm: a later epoch may materialize them instead of retaining a cached
    // local. Function-wide state from earlier members and the precise PC can
    // still be deferred to the cold helper arm.
    for &(reg, value) in &exit.outputs {
        let value_local = local_map[value.0]
            .ok_or_else(|| EmitError("missing direct-vector integer epoch value".into()))?;
        emit_materialized_x_store(function, layout, reg, value_local);
    }
    for &(reg, value) in &exit.f_outputs {
        let value_local = local_map[value.0]
            .ok_or_else(|| EmitError("missing direct-vector FP epoch value".into()))?;
        function.instruction(&Instruction::I32Const(layout.f_base as i32));
        function.instruction(&Instruction::LocalGet(value_local));
        function.instruction(&Instruction::I64Store(memarg(3, u64::from(reg) * 8)));
    }
    if let Some(value) = exit.fcsr_output {
        let value_local = local_map[value.0]
            .ok_or_else(|| EmitError("missing direct-vector fcsr epoch value".into()))?;
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        function.instruction(&Instruction::LocalGet(value_local));
        function.instruction(&Instruction::I32Store(memarg(2, 0)));
    }
    Ok(())
}

fn emit_materialized_x_store(
    function: &mut Function,
    layout: JitLayout,
    reg: u8,
    value_local: u32,
) {
    function.instruction(&Instruction::I32Const(layout.x_base as i32));
    function.instruction(&Instruction::LocalGet(value_local));
    function.instruction(&Instruction::I64Store(memarg(3, u64::from(reg) * 8)));
}

fn emit_lazy_fcsr_read(
    function: &mut Function,
    layout: JitLayout,
    state_local: u32,
    valid_local: Option<u32>,
) {
    if let Some(valid) = valid_local {
        function.instruction(&Instruction::LocalGet(valid));
        function.instruction(&Instruction::I32Eqz);
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        function.instruction(&Instruction::I32Load(memarg(2, 0)));
        function.instruction(&Instruction::LocalSet(state_local));
        function.instruction(&Instruction::I32Const(1));
        function.instruction(&Instruction::LocalSet(valid));
        function.instruction(&Instruction::End);
    }
    function.instruction(&Instruction::LocalGet(state_local));
}

#[allow(clippy::too_many_arguments)]
fn emit_cached_dispatch_tree(
    function: &mut Function,
    order: &[usize],
    regions: &[(&Region, Option<LoopBackedge>)],
    local_maps: &[Vec<Option<u32>>],
    layout: JitLayout,
    helpers: HelperImports,
    memory_temps: Option<MemoryTemps>,
    state: &CachedStateLocals,
    matched_local: u32,
    vector_status: Option<u32>,
    vector_temps: Option<VectorTemps>,
) -> Result<(), EmitError> {
    if order.len() <= 3 {
        for &index in order {
            let (region, loop_backedge) = regions[index];
            function.instruction(&Instruction::LocalGet(state.pc));
            emit_guest_pc(function, region.entry_pc, layout);
            function.instruction(&Instruction::I64Eq);
            function.instruction(&Instruction::If(BlockType::Empty));
            emit_cached_body(
                function,
                region,
                layout,
                helpers,
                memory_temps,
                state,
                &local_maps[index],
                loop_backedge,
                None,
                vector_status,
                vector_temps,
            )?;
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::LocalSet(matched_local));
            function.instruction(&Instruction::End);
        }
        return Ok(());
    }

    let middle = order.len() / 2;
    let pivot = regions[order[middle]].0.entry_pc;
    function.instruction(&Instruction::LocalGet(state.pc));
    emit_guest_pc(function, pivot, layout);
    function.instruction(&Instruction::I64LtU);
    function.instruction(&Instruction::If(BlockType::Empty));
    emit_cached_dispatch_tree(
        function,
        &order[..middle],
        regions,
        local_maps,
        layout,
        helpers,
        memory_temps,
        state,
        matched_local,
        vector_status,
        vector_temps,
    )?;
    function.instruction(&Instruction::Else);
    emit_cached_dispatch_tree(
        function,
        &order[middle..],
        regions,
        local_maps,
        layout,
        helpers,
        memory_temps,
        state,
        matched_local,
        vector_status,
        vector_temps,
    )?;
    function.instruction(&Instruction::End);
    Ok(())
}

fn emit_copy_base_address(
    function: &mut Function,
    root_local: u32,
    offset: i64,
    destination_local: u32,
) {
    function.instruction(&Instruction::LocalGet(root_local));
    if offset != 0 {
        function.instruction(&Instruction::I64Const(offset));
        function.instruction(&Instruction::I64Add);
    }
    function.instruction(&Instruction::LocalSet(destination_local));
}

#[allow(clippy::too_many_arguments)]
fn emit_dense_copy_setup(
    function: &mut Function,
    plan: &DenseCopyPlan,
    layout: JitLayout,
    helpers: HelperImports,
    temps: MemoryTemps,
    local_map: &[Option<u32>],
    mut side_exit: impl FnMut(&mut Function) -> Result<(), EmitError>,
) -> Result<(), EmitError> {
    let copy = temps
        .copy
        .ok_or_else(|| EmitError("dense-copy plan lacks address temporaries".into()))?;
    emit_copy_base_address(
        function,
        local_map[plan.source_root.0].expect("dense-copy source root local"),
        plan.source_base_offset,
        copy.source_address,
    );
    emit_memory_address(
        function,
        layout,
        helpers,
        Some(temps),
        copy.source_address,
        plan.bytes,
        false,
        |function| side_exit(function),
    )?;
    function.instruction(&Instruction::LocalSet(copy.source_linear));

    emit_copy_base_address(
        function,
        local_map[plan.destination_root.0].expect("dense-copy destination root local"),
        plan.destination_base_offset,
        copy.destination_address,
    );
    emit_memory_address(
        function,
        layout,
        helpers,
        Some(temps),
        copy.destination_address,
        plan.bytes,
        true,
        |function| side_exit(function),
    )?;
    function.instruction(&Instruction::LocalSet(copy.destination_linear));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_dense_store_setup(
    function: &mut Function,
    plan: &DenseStorePlan,
    layout: JitLayout,
    helpers: HelperImports,
    temps: MemoryTemps,
    local_map: &[Option<u32>],
    mut side_exit: impl FnMut(&mut Function) -> Result<(), EmitError>,
) -> Result<(), EmitError> {
    let range = temps
        .copy
        .ok_or_else(|| EmitError("dense-store plan lacks address temporaries".into()))?;
    emit_copy_base_address(
        function,
        local_map[plan.destination_root.0].expect("dense-store destination root local"),
        plan.destination_base_offset,
        range.destination_address,
    );
    emit_memory_address(
        function,
        layout,
        helpers,
        Some(temps),
        range.destination_address,
        plan.bytes,
        true,
        |function| side_exit(function),
    )?;
    function.instruction(&Instruction::LocalSet(range.destination_linear));
    Ok(())
}

/// Execute a common-word dense store as one `memory.fill` when the runtime
/// value is a broadcast byte (zeroing and ordinary memset). The guarded scalar
/// arm preserves exact behavior for every other repeated 64-bit word. The
/// caller has already proved the complete range is ordinary direct RAM, so no
/// guest-visible fault or MMIO observation can occur between these stores.
fn emit_dense_fill(
    function: &mut Function,
    plan: &DenseStorePlan,
    destination_linear: u32,
    value_local: u32,
) -> Result<(), EmitError> {
    let bytes = i32::try_from(plan.bytes)
        .map_err(|_| EmitError("dense-store range exceeds Wasm32 bulk-memory length".into()))?;

    function.instruction(&Instruction::LocalGet(value_local));
    function.instruction(&Instruction::I64Const(0xff));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(0x0101_0101_0101_0101));
    function.instruction(&Instruction::I64Mul);
    function.instruction(&Instruction::LocalGet(value_local));
    function.instruction(&Instruction::I64Eq);
    function.instruction(&Instruction::If(BlockType::Empty));
    function.instruction(&Instruction::LocalGet(destination_linear));
    function.instruction(&Instruction::LocalGet(value_local));
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::I32Const(bytes));
    function.instruction(&Instruction::MemoryFill(0));
    function.instruction(&Instruction::Else);
    let mut accesses = plan.accesses.clone();
    accesses.sort_unstable_by_key(|access| access.destination_offset);
    for access in accesses {
        function.instruction(&Instruction::LocalGet(destination_linear));
        function.instruction(&Instruction::LocalGet(value_local));
        function.instruction(&Instruction::I64Store(memarg(3, access.destination_offset)));
    }
    function.instruction(&Instruction::End);
    Ok(())
}

fn emit_fuel_value(function: &mut Function, fuel_local: Option<u32>, fuel_addr: u32) -> bool {
    if let Some(local) = fuel_local {
        function.instruction(&Instruction::LocalGet(local));
        true
    } else if fuel_addr != 0 {
        function.instruction(&Instruction::I32Const(fuel_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        true
    } else {
        false
    }
}

fn emit_min_i64_local(function: &mut Function, target: u32, candidate: u32) {
    function.instruction(&Instruction::LocalGet(target));
    function.instruction(&Instruction::LocalGet(candidate));
    function.instruction(&Instruction::LocalGet(target));
    function.instruction(&Instruction::LocalGet(candidate));
    function.instruction(&Instruction::I64LtU);
    function.instruction(&Instruction::Select);
    function.instruction(&Instruction::LocalSet(target));
}

/// Call the runtime's proved-loop bulk-copy helper from standalone and cached
/// loop bodies. The helper validates each source/destination page pair before
/// modifying memory and returns only the number of complete guest iterations
/// it committed. A zero return falls through to the untouched scalar loop.
#[allow(clippy::too_many_arguments)]
fn emit_bulk_copy_call(
    function: &mut Function,
    plan: BulkCopyLoopPlan,
    region_retired: u32,
    helpers: HelperImports,
    temps: BulkCopyTemps,
    source_local: u32,
    destination_local: u32,
    count_local: u32,
    limit_local: u32,
    retired_local: u32,
    fuel_local: Option<u32>,
    fuel_addr: u32,
) -> Result<(), EmitError> {
    // The proved loop runs floor(count/iteration_bytes) iterations. The guard
    // on the invariant limit register is essential: it is initialized
    // by the predecessor block and therefore appears as ReadX, not a constant,
    // in the loop member itself.
    function.instruction(&Instruction::LocalGet(count_local));
    function.instruction(&Instruction::I64Const(-(plan.bytes_per_iteration as i64)));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::LocalSet(temps.request));

    // Preserve the scalar loop's fuel contract.  The first iteration is
    // always permitted; subsequent iterations fit ceil(remaining/body_cost).
    if emit_fuel_value(function, fuel_local, fuel_addr) {
        function.instruction(&Instruction::LocalGet(retired_local));
        function.instruction(&Instruction::I64GtU);
        function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
        emit_fuel_value(function, fuel_local, fuel_addr);
        function.instruction(&Instruction::LocalGet(retired_local));
        function.instruction(&Instruction::I64Sub);
        function.instruction(&Instruction::Else);
        function.instruction(&Instruction::I64Const(0));
        function.instruction(&Instruction::End);
        function.instruction(&Instruction::LocalSet(temps.fuel_bytes));

        function.instruction(&Instruction::LocalGet(temps.fuel_bytes));
        function.instruction(&Instruction::I64Const(i64::from(region_retired)));
        function.instruction(&Instruction::I64DivU);
        function.instruction(&Instruction::LocalGet(temps.fuel_bytes));
        function.instruction(&Instruction::I64Const(i64::from(region_retired)));
        function.instruction(&Instruction::I64RemU);
        function.instruction(&Instruction::I64Const(0));
        function.instruction(&Instruction::I64Ne);
        function.instruction(&Instruction::I64ExtendI32U);
        function.instruction(&Instruction::I64Add);
        function.instruction(&Instruction::LocalSet(temps.fuel_bytes));

        function.instruction(&Instruction::LocalGet(temps.fuel_bytes));
        function.instruction(&Instruction::I64Const(1));
        function.instruction(&Instruction::I64LtU);
        function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
        function.instruction(&Instruction::I64Const(1));
        function.instruction(&Instruction::Else);
        function.instruction(&Instruction::LocalGet(temps.fuel_bytes));
        function.instruction(&Instruction::End);
        function.instruction(&Instruction::I64Const(plan.bytes_per_iteration as i64));
        function.instruction(&Instruction::I64Mul);
        function.instruction(&Instruction::LocalSet(temps.fuel_bytes));

        emit_min_i64_local(function, temps.request, temps.fuel_bytes);
    }

    function.instruction(&Instruction::LocalGet(limit_local));
    function.instruction(&Instruction::I64Const(plan.limit_value as i64));
    function.instruction(&Instruction::I64Eq);
    function.instruction(&Instruction::LocalGet(temps.request));
    function.instruction(&Instruction::I64Const(plan.bytes_per_iteration as i64));
    function.instruction(&Instruction::I64GeU);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::If(BlockType::Empty));
    function.instruction(&Instruction::LocalGet(0));
    function.instruction(&Instruction::LocalGet(source_local));
    function.instruction(&Instruction::LocalGet(destination_local));
    function.instruction(&Instruction::LocalGet(temps.request));
    function.instruction(&Instruction::I32Const(plan.bytes_per_iteration as i32));
    function.instruction(&Instruction::I32Const(i32::from(plan.step < 0)));
    function.instruction(&Instruction::I32Const(i32::from(plan.value_reg)));
    function.instruction(&Instruction::Call(helpers.bulk_copy_index().ok_or_else(
        || EmitError("bulk-copy loop lacks the system helper import".into()),
    )?));
    function.instruction(&Instruction::LocalSet(temps.result));
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::I64Const(0));
    function.instruction(&Instruction::LocalSet(temps.result));
    function.instruction(&Instruction::End);
    Ok(())
}

fn emit_add_or_sub_local(
    function: &mut Function,
    target_local: u32,
    amount_local: u32,
    subtract: bool,
) {
    function.instruction(&Instruction::LocalGet(target_local));
    function.instruction(&Instruction::LocalGet(amount_local));
    function.instruction(&if subtract {
        Instruction::I64Sub
    } else {
        Instruction::I64Add
    });
    function.instruction(&Instruction::LocalSet(target_local));
}

fn emit_bulk_retired(
    function: &mut Function,
    retired_local: u32,
    result_local: u32,
    bytes_per_iteration: u64,
    retired_per_iteration: u32,
) {
    function.instruction(&Instruction::LocalGet(retired_local));
    function.instruction(&Instruction::LocalGet(result_local));
    function.instruction(&Instruction::I64Const(bytes_per_iteration as i64));
    function.instruction(&Instruction::I64DivU);
    function.instruction(&Instruction::I64Const(i64::from(retired_per_iteration)));
    function.instruction(&Instruction::I64Mul);
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::LocalSet(retired_local));
}

#[allow(clippy::too_many_arguments)]
fn emit_cached_body(
    function: &mut Function,
    region: &Region,
    layout: JitLayout,
    helpers: HelperImports,
    memory_temps: Option<MemoryTemps>,
    state: &CachedStateLocals,
    local_map: &[Option<u32>],
    loop_backedge: Option<LoopBackedge>,
    direct_range: Option<(MemberRangePlan, MemberRangeTemps)>,
    vector_status: Option<u32>,
    vector_temps: Option<VectorTemps>,
) -> Result<(), EmitError> {
    let loop_backedge = (!region.has_vector_helper())
        .then_some(loop_backedge)
        .flatten();
    let memory_temps = memory_temps_for_region(memory_temps, region, layout);
    let copy_plan = match (layout.sys, memory_temps.and_then(|temps| temps.copy)) {
        (Some(memory), Some(_)) => {
            dense_copy_plan(region, usize::from(memory.cache_min_accesses.max(1)))
        }
        _ => None,
    }
    // A failed whole-loop helper must fall back to the exact scalar body. The
    // per-iteration range hoist would itself side-exit when either 64-byte
    // range straddles a page, resuming at interior ld/sd PCs and permanently
    // fragmenting the canonical loop header that the bulk path needs.
    .filter(|_| bulk_copy_loop_plan(region).is_none());
    let store_plan = match (layout.sys, memory_temps.and_then(|temps| temps.copy)) {
        (Some(memory), Some(_)) => {
            dense_store_plan(region, usize::from(memory.cache_min_accesses.max(1)))
        }
        _ => None,
    };
    // Structured-CFG modules own backedges outside this member and therefore
    // intentionally pass `None` here. The IR recognizer itself proves the
    // canonical backedge, so it is the authority for the bulk path; the
    // optional marker controls only whether the scalar fallback forms a
    // nested local Wasm loop.
    let bulk_plan = bulk_copy_loop_plan(region);
    let bulk_temps = bulk_plan.and_then(|_| memory_temps.and_then(|temps| temps.bulk_copy));
    let all_defined = vec![true; region.values.len()];
    let mut bulk_preloaded_x = 0u32;
    let bulk_x_locals = if let (Some(plan), Some(temps)) = (bulk_plan, bulk_temps) {
        let mut inputs = vec![
            plan.source_reg,
            plan.destination_reg,
            plan.count_reg,
            plan.limit_reg,
        ];
        inputs.sort_unstable();
        inputs.dedup();
        for reg in inputs {
            emit_cached_x_member_input(function, region, layout, state, local_map, reg)?;
            if is_materialized_x(state, reg) {
                bulk_preloaded_x |= 1u32 << reg;
            }
        }
        let source_local = cached_x_member_local(region, state, local_map, plan.source_reg)?;
        let destination_local =
            cached_x_member_local(region, state, local_map, plan.destination_reg)?;
        let count_local = cached_x_member_local(region, state, local_map, plan.count_reg)?;
        let limit_local = cached_x_member_local(region, state, local_map, plan.limit_reg)?;
        emit_bulk_copy_call(
            function,
            plan,
            region.retired,
            helpers,
            temps,
            source_local,
            destination_local,
            count_local,
            limit_local,
            state.retired,
            state.fuel,
            layout.fuel_addr,
        )?;
        function.instruction(&Instruction::LocalGet(temps.result));
        function.instruction(&Instruction::I64Eqz);
        function.instruction(&Instruction::If(BlockType::Empty));
        Some((source_local, destination_local, count_local, limit_local))
    } else {
        None
    };
    if loop_backedge.is_some() {
        function.instruction(&Instruction::Loop(BlockType::Empty));
    }
    // This is intentionally local to one Region. A successful vsetivli has
    // exact vtype/vl/vstart results; every subsequently completed ordinary
    // vector instruction preserves that configuration and resets vstart to
    // zero. Any configuration form we cannot resolve clears the fact.
    let mut known_vector_config = None;
    for position in 0..=region.values.len() {
        for effect in &region.effects {
            match effect {
                Effect::Store {
                    position: effect_position,
                    address,
                    value,
                    kind,
                    condition,
                    exit,
                } if *effect_position == position => {
                    if let Some(condition) = condition {
                        function.instruction(&Instruction::LocalGet(
                            local_map[condition.0].expect("cached condition local"),
                        ));
                        function.instruction(&Instruction::If(BlockType::Empty));
                    }
                    let copy_access = copy_plan
                        .as_ref()
                        .and_then(|plan| plan.store_access(*effect_position, *address, *value));
                    let address_local = local_map[address.0].expect("cached address local");
                    let direct_access = direct_range.and_then(|(plan, temps)| {
                        condition
                            .is_none()
                            .then(|| plan.direct_offset(region, *address))
                            .flatten()
                            .map(|offset| (temps.store_linear, offset))
                    });
                    if let Some((base, offset)) = direct_access {
                        function.instruction(&Instruction::LocalGet(base));
                        function.instruction(&Instruction::LocalGet(
                            local_map[value.0].expect("cached direct-store value local"),
                        ));
                        function.instruction(&match kind {
                            StoreKind::I8 => Instruction::I64Store8(memarg(0, offset)),
                            StoreKind::I16 => Instruction::I64Store16(memarg(0, offset)),
                            StoreKind::I32 => Instruction::I64Store32(memarg(0, offset)),
                            StoreKind::I64 => Instruction::I64Store(memarg(0, offset)),
                        });
                    } else if let (Some(access), Some(copy)) =
                        (copy_access, memory_temps.and_then(|temps| temps.copy))
                    {
                        if copy_plan
                            .as_ref()
                            .is_some_and(|plan| plan.setup_position == *effect_position)
                        {
                            emit_dense_copy_setup(
                                function,
                                copy_plan.as_ref().expect("copy plan"),
                                layout,
                                helpers,
                                memory_temps.expect("copy memory temporaries"),
                                local_map,
                                |function| {
                                    emit_cached_side_exit(
                                        function, region, layout, state, local_map, exit, None,
                                    )
                                },
                            )?;
                        }
                        function.instruction(&Instruction::LocalGet(copy.destination_linear));
                        function.instruction(&Instruction::LocalGet(
                            local_map[value.0].expect("cached copy value local"),
                        ));
                        function.instruction(&Instruction::I64Store(memarg(
                            3,
                            access.destination_offset,
                        )));
                    } else if let (Some(access), Some(range)) = (
                        store_plan
                            .as_ref()
                            .and_then(|plan| plan.store_access(*effect_position, *address)),
                        memory_temps.and_then(|temps| temps.copy),
                    ) {
                        if store_plan
                            .as_ref()
                            .is_some_and(|plan| plan.setup_position == *effect_position)
                        {
                            emit_dense_store_setup(
                                function,
                                store_plan.as_ref().expect("store plan"),
                                layout,
                                helpers,
                                memory_temps.expect("store memory temporaries"),
                                local_map,
                                |function| {
                                    emit_cached_side_exit(
                                        function, region, layout, state, local_map, exit, None,
                                    )
                                },
                            )?;
                        }
                        if let Some(fill_value) =
                            store_plan.as_ref().and_then(|plan| plan.fill_value)
                        {
                            if store_plan
                                .as_ref()
                                .is_some_and(|plan| plan.setup_position == *effect_position)
                            {
                                emit_dense_fill(
                                    function,
                                    store_plan.as_ref().expect("store plan"),
                                    range.destination_linear,
                                    local_map[fill_value.0].expect("cached fill value local"),
                                )?;
                            }
                        } else {
                            function.instruction(&Instruction::LocalGet(range.destination_linear));
                            function.instruction(&Instruction::LocalGet(
                                local_map[value.0].expect("cached store value local"),
                            ));
                            function.instruction(&Instruction::I64Store(memarg(
                                3,
                                access.destination_offset,
                            )));
                        }
                    } else {
                        emit_memory_address(
                            function,
                            layout,
                            helpers,
                            memory_temps,
                            address_local,
                            kind.bytes(),
                            true,
                            |function| {
                                emit_cached_side_exit(
                                    function, region, layout, state, local_map, exit, None,
                                )
                            },
                        )?;
                        function.instruction(&Instruction::LocalGet(
                            local_map[value.0].expect("cached store value local"),
                        ));
                        function.instruction(&match kind {
                            StoreKind::I8 => Instruction::I64Store8(memarg(0, 0)),
                            StoreKind::I16 => Instruction::I64Store16(memarg(0, 0)),
                            StoreKind::I32 => Instruction::I64Store32(memarg(0, 0)),
                            StoreKind::I64 => Instruction::I64Store(memarg(0, 0)),
                        });
                    }
                    if condition.is_some() {
                        function.instruction(&Instruction::End);
                        emit_reservation_clear(function, helpers, address_local)?;
                    }
                }
                Effect::Guard {
                    position: effect_position,
                    condition,
                    exit,
                } if *effect_position == position => {
                    function.instruction(&Instruction::LocalGet(
                        local_map[condition.0].expect("cached guard local"),
                    ));
                    function.instruction(&Instruction::If(BlockType::Empty));
                    emit_cached_side_exit(function, region, layout, state, local_map, exit, None)?;
                    function.instruction(&Instruction::Return);
                    function.instruction(&Instruction::End);
                }
                Effect::GuardTarget {
                    position: effect_position,
                    target,
                    expected,
                    exit,
                } if *effect_position == position => {
                    let target_local = local_map[target.0].expect("cached target local");
                    function.instruction(&Instruction::LocalGet(target_local));
                    emit_guest_pc(function, *expected, layout);
                    function.instruction(&Instruction::I64Ne);
                    function.instruction(&Instruction::If(BlockType::Empty));
                    emit_cached_side_exit(
                        function,
                        region,
                        layout,
                        state,
                        local_map,
                        exit,
                        Some(target_local),
                    )?;
                    emit_ic_guard_miss(function, region, layout, target_local);
                    function.instruction(&Instruction::Return);
                    function.instruction(&Instruction::End);
                }
                Effect::FpState {
                    position: effect_position,
                    dirty,
                    exit,
                } if *effect_position == position => {
                    emit_fp_state(function, layout, *dirty, |function| {
                        emit_cached_side_exit(
                            function, region, layout, state, local_map, exit, None,
                        )
                    })?;
                }
                Effect::VectorState {
                    position: effect_position,
                    exit,
                } if *effect_position == position => {
                    emit_vector_state(function, layout, |function| {
                        emit_cached_side_exit(
                            function, region, layout, state, local_map, exit, None,
                        )
                    })?;
                }
                Effect::Vector {
                    position: effect_position,
                    insn,
                    direct,
                    fallthrough,
                    exit,
                } if *effect_position == position => {
                    let status = vector_status.ok_or_else(|| {
                        EmitError("cached vector effect lacks a status local".into())
                    })?;
                    if vector_config_instruction(*insn) {
                        emit_vector_unit_lmul1_invalidate(function, vector_temps);
                    }
                    if !matches!(
                        *direct,
                        Some(VectorDirect::UnitStride { masked: false, .. })
                    ) {
                        emit_vector_unit_load_cache_invalidate(function, vector_temps);
                    }
                    let retained_config = known_vector_config
                        .and_then(|config| known_vector_retaining_config_transition(config, *insn));
                    let deferred_fallback =
                        retained_config.is_some() || vector_cached_direct_deferable(*direct);
                    let mut address_preloaded = false;
                    if deferred_fallback {
                        emit_cached_vector_epoch_outputs(function, layout, local_map, exit)?;
                        if let Some(direct) = *direct {
                            address_preloaded = emit_cached_vector_unit_address(
                                function,
                                layout,
                                state,
                                local_map,
                                exit,
                                direct,
                                vector_temps,
                            )?;
                            if !address_preloaded {
                                emit_cached_vector_x_inputs(function, layout, state, exit, direct)?;
                            }
                        }
                    } else {
                        emit_cached_side_exit_state(
                            function, region, layout, state, local_map, exit, None,
                        )?;
                    }
                    let mut prepare_fallback = |function: &mut Function| {
                        if deferred_fallback {
                            emit_cached_side_exit_state(
                                function, region, layout, state, local_map, exit, None,
                            )
                        } else {
                            Ok(())
                        }
                    };
                    let known_direct = known_vector_config.and_then(|config| {
                        direct
                            .filter(|direct| vector_direct_available(layout, Some(*direct)))
                            .and_then(|direct| {
                                known_vector_direct_tail_merge(config, direct)
                                    .map(|tail_merge| (config, direct, tail_merge))
                            })
                    });
                    if let Some(config) = retained_config {
                        emit_known_vector_config_execution(function, layout, config, status)?;
                    } else if let Some((config, direct, tail_merge)) = known_direct {
                        emit_known_vector_execution(
                            function,
                            layout,
                            helpers,
                            *insn,
                            direct,
                            config,
                            tail_merge,
                            vector_temps,
                            status,
                            &mut prepare_fallback,
                            false,
                            address_preloaded,
                        )?;
                    } else {
                        emit_vector_execution(
                            function,
                            layout,
                            helpers,
                            *insn,
                            *direct,
                            vector_temps,
                            status,
                            &mut prepare_fallback,
                            address_preloaded,
                        )?;
                    }

                    function.instruction(&Instruction::LocalGet(status));
                    function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
                    function.instruction(&Instruction::I32Eq);
                    function.instruction(&Instruction::If(BlockType::Empty));
                    emit_cached_state_reconcile_after_direct_vector(
                        function, state, local_map, exit,
                    )?;
                    function.instruction(&Instruction::Else);

                    function.instruction(&Instruction::LocalGet(status));
                    function.instruction(&Instruction::I32Eqz);
                    function.instruction(&Instruction::If(BlockType::Empty));
                    emit_cached_retirement(function, layout, state, exit.retired);
                    function.instruction(&Instruction::Return);
                    function.instruction(&Instruction::End);

                    function.instruction(&Instruction::LocalGet(status));
                    function.instruction(&Instruction::I32Const(2));
                    function.instruction(&Instruction::I32Eq);
                    function.instruction(&Instruction::If(BlockType::Empty));
                    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
                    emit_guest_pc(function, *fallthrough, layout);
                    function.instruction(&Instruction::I64Store(memarg(3, 0)));
                    emit_cached_retirement(function, layout, state, exit.retired.saturating_add(1));
                    function.instruction(&Instruction::Return);
                    function.instruction(&Instruction::End);

                    emit_cached_state_reload_after_vector(function, layout, state);
                    function.instruction(&Instruction::End);

                    if vector_config_instruction(*insn) {
                        emit_vector_unit_lmul1_refresh(function, layout, vector_temps);
                        known_vector_config = match direct {
                            Some(VectorDirect::ConfigImmediate { vtype, vl, .. }) => {
                                KnownVectorConfig::decode(*vtype, *vl)
                            }
                            Some(VectorDirect::ConfigRetainFull { vtype, vlmax }) => {
                                KnownVectorConfig::decode(*vtype, *vlmax)
                            }
                            _ => retained_config,
                        };
                    }
                }
                _ => {}
            }
        }

        let Some(data) = region.values.get(position) else {
            continue;
        };
        let output_local = local_map[position].expect("cached SSA local");
        match &data.op {
            Op::ReadX(reg) => {
                if let Some(state_local) = state.x[*reg as usize] {
                    emit_lazy_state_read_i64(
                        function,
                        layout.x_base,
                        state_local,
                        state.valid_x,
                        *reg as usize,
                    );
                } else if is_materialized_x(state, *reg) {
                    if bulk_preloaded_x & (1u32 << *reg) != 0 {
                        function.instruction(&Instruction::LocalGet(output_local));
                    } else {
                        function.instruction(&Instruction::I32Const(layout.x_base as i32));
                        function.instruction(&Instruction::I64Load(memarg(3, u64::from(*reg) * 8)));
                    }
                } else {
                    return Err(EmitError(format!(
                        "cached integer input x{reg} has no state representation"
                    )));
                }
            }
            Op::ReadF(reg) => {
                emit_lazy_state_read_i64(
                    function,
                    layout.f_base,
                    state.f[*reg as usize].expect("cached FP input"),
                    state.valid_f,
                    *reg as usize,
                );
            }
            Op::ReadFcsr => {
                emit_lazy_fcsr_read(
                    function,
                    layout,
                    state.fcsr.expect("cached fcsr input"),
                    state.valid_fcsr,
                );
            }
            Op::Load {
                address,
                kind,
                exit,
            } => {
                let copy_access = copy_plan
                    .as_ref()
                    .and_then(|plan| plan.load_access(ValueId(position)));
                let direct_access = direct_range.and_then(|(plan, temps)| {
                    plan.direct_offset(region, *address)
                        .map(|offset| (temps.load_linear, offset))
                });
                if let Some((base, offset)) = direct_access {
                    function.instruction(&Instruction::LocalGet(base));
                    function.instruction(&match kind {
                        LoadKind::I8S => Instruction::I64Load8S(memarg(0, offset)),
                        LoadKind::I16S => Instruction::I64Load16S(memarg(0, offset)),
                        LoadKind::I32S => Instruction::I64Load32S(memarg(0, offset)),
                        LoadKind::I64 => Instruction::I64Load(memarg(0, offset)),
                        LoadKind::I8U => Instruction::I64Load8U(memarg(0, offset)),
                        LoadKind::I16U => Instruction::I64Load16U(memarg(0, offset)),
                        LoadKind::I32U => Instruction::I64Load32U(memarg(0, offset)),
                    });
                } else if let (Some(access), Some(copy)) =
                    (copy_access, memory_temps.and_then(|temps| temps.copy))
                {
                    if copy_plan
                        .as_ref()
                        .is_some_and(|plan| plan.setup_position == position)
                    {
                        emit_dense_copy_setup(
                            function,
                            copy_plan.as_ref().expect("copy plan"),
                            layout,
                            helpers,
                            memory_temps.expect("copy memory temporaries"),
                            local_map,
                            |function| {
                                emit_cached_side_exit(
                                    function, region, layout, state, local_map, exit, None,
                                )
                            },
                        )?;
                    }
                    function.instruction(&Instruction::LocalGet(copy.source_linear));
                    function.instruction(&Instruction::I64Load(memarg(3, access.source_offset)));
                } else {
                    let address_local = local_map[address.0].expect("cached load address local");
                    emit_memory_address(
                        function,
                        layout,
                        helpers,
                        memory_temps,
                        address_local,
                        kind.bytes(),
                        false,
                        |function| {
                            emit_cached_side_exit(
                                function, region, layout, state, local_map, exit, None,
                            )
                        },
                    )?;
                    function.instruction(&match kind {
                        LoadKind::I8S => Instruction::I64Load8S(memarg(0, 0)),
                        LoadKind::I16S => Instruction::I64Load16S(memarg(0, 0)),
                        LoadKind::I32S => Instruction::I64Load32S(memarg(0, 0)),
                        LoadKind::I64 => Instruction::I64Load(memarg(0, 0)),
                        LoadKind::I8U => Instruction::I64Load8U(memarg(0, 0)),
                        LoadKind::I16U => Instruction::I64Load16U(memarg(0, 0)),
                        LoadKind::I32U => Instruction::I64Load32U(memarg(0, 0)),
                    });
                }
            }
            Op::ExactFp {
                op,
                lhs,
                rhs,
                third,
                rm,
                fcsr,
                exit,
            } => {
                function.instruction(&Instruction::LocalGet(
                    local_map[rm.0].expect("cached rounding local"),
                ));
                function.instruction(&Instruction::I32Const(4));
                function.instruction(&Instruction::I32GtU);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_cached_side_exit(function, region, layout, state, local_map, exit, None)?;
                function.instruction(&Instruction::Return);
                function.instruction(&Instruction::End);
                emit_exact_fp_value(
                    function,
                    *op,
                    local_map[lhs.0].expect("cached helper lhs local"),
                    local_map[rhs.0].expect("cached helper rhs local"),
                    local_map[third.0].expect("cached helper third local"),
                    local_map[rm.0].expect("cached helper rounding local"),
                    local_map[fcsr.0].expect("cached helper fcsr local"),
                    output_local,
                    layout,
                    helpers,
                )?;
            }
            Op::Reservation { op, address } => {
                function.instruction(&Instruction::I32Const(match op {
                    ReservationOp::LoadReserved => 0,
                    ReservationOp::StoreConditional => 1,
                }));
                function.instruction(&Instruction::LocalGet(0));
                function.instruction(&Instruction::LocalGet(
                    local_map[address.0].expect("cached reservation address local"),
                ));
                function.instruction(&Instruction::Call(
                    helpers
                        .reservation_index()
                        .ok_or_else(|| EmitError("missing reservation helper import".into()))?,
                ));
            }
            _ => emit_value_body(
                function,
                region,
                layout,
                local_map,
                &all_defined,
                ValueId(position),
            )?,
        };
        function.instruction(&Instruction::LocalSet(output_local));
    }

    for &(reg, value) in &region.outputs {
        let value_local = local_map[value.0].expect("cached integer output local");
        if let Some(state_local) = state.x[reg as usize] {
            function.instruction(&Instruction::LocalGet(value_local));
            function.instruction(&Instruction::LocalSet(state_local));
            if let Some(valid) = state.valid_x {
                emit_mark_valid_i64(function, valid, reg as usize);
            }
        } else if is_materialized_x(state, reg) {
            emit_materialized_x_store(function, layout, reg, value_local);
        } else {
            return Err(EmitError(format!(
                "cached integer output x{reg} has no state representation"
            )));
        }
    }
    for &(reg, value) in &region.f_outputs {
        function.instruction(&Instruction::LocalGet(
            local_map[value.0].expect("cached FP output local"),
        ));
        function.instruction(&Instruction::LocalSet(
            state.f[reg as usize].expect("cached FP state local"),
        ));
        if let Some(valid) = state.valid_f {
            emit_mark_valid_i64(function, valid, reg as usize);
        }
    }
    if let Some(value) = region.fcsr_output {
        function.instruction(&Instruction::LocalGet(
            local_map[value.0].expect("cached fcsr output local"),
        ));
        function.instruction(&Instruction::LocalSet(
            state.fcsr.expect("cached fcsr state local"),
        ));
        if let Some(valid) = state.valid_fcsr {
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::LocalSet(valid));
        }
    }
    function.instruction(&Instruction::LocalGet(
        local_map[region.next_pc.0].expect("cached next-PC local"),
    ));
    function.instruction(&Instruction::LocalSet(state.pc));
    function.instruction(&Instruction::LocalGet(state.retired));
    function.instruction(&Instruction::I64Const(i64::from(region.retired)));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::LocalSet(state.retired));
    if loop_backedge.is_some() {
        // Re-enter only for the architectural backedge and while another
        // iteration is permitted by this invocation's cumulative budget. With
        // no fuel capability, fall through to the outer dispatcher so its
        // defensive hop cap still bounds malformed standalone embeddings.
        if let Some(fuel) = state.fuel {
            function.instruction(&Instruction::LocalGet(state.pc));
            emit_guest_pc(function, region.entry_pc, layout);
            function.instruction(&Instruction::I64Eq);
            function.instruction(&Instruction::LocalGet(state.retired));
            function.instruction(&Instruction::LocalGet(fuel));
            function.instruction(&Instruction::I64LtU);
            function.instruction(&Instruction::I32And);
            function.instruction(&Instruction::BrIf(0));
        }
        function.instruction(&Instruction::End);
    }
    if let (Some(plan), Some(temps), Some((source_local, destination_local, count_local, _))) =
        (bulk_plan, bulk_temps, bulk_x_locals)
    {
        function.instruction(&Instruction::Else);
        let subtract_pointers = plan.step < 0;
        for (reg, local, subtract) in [
            (plan.source_reg, source_local, subtract_pointers),
            (plan.destination_reg, destination_local, subtract_pointers),
            (plan.count_reg, count_local, true),
        ] {
            emit_add_or_sub_local(function, local, temps.result, subtract);
            if is_materialized_x(state, reg) {
                emit_materialized_x_store(function, layout, reg, local);
            }
        }
        if let Some(value_state) = state.x[plan.value_reg as usize] {
            function.instruction(&Instruction::I32Const(layout.x_base as i32));
            function.instruction(&Instruction::I64Load(memarg(
                3,
                u64::from(plan.value_reg) * 8,
            )));
            function.instruction(&Instruction::LocalSet(value_state));
        } else if !is_materialized_x(state, plan.value_reg) {
            return Err(EmitError(format!(
                "bulk-copy value x{} has no state representation",
                plan.value_reg
            )));
        }
        if let Some(valid) = state.valid_x {
            for reg in [
                plan.source_reg,
                plan.destination_reg,
                plan.count_reg,
                plan.value_reg,
            ] {
                if state.x[reg as usize].is_some() {
                    emit_mark_valid_i64(function, valid, reg as usize);
                }
            }
        }

        function.instruction(&Instruction::I64Const(plan.limit_value as i64));
        function.instruction(&Instruction::LocalGet(count_local));
        function.instruction(&Instruction::I64LtU);
        function.instruction(&Instruction::LocalSet(
            local_map[plan.condition.0].expect("bulk-copy condition local"),
        ));
        emit_guest_pc(function, region.entry_pc, layout);
        emit_guest_pc(function, plan.exit_pc, layout);
        function.instruction(&Instruction::LocalGet(
            local_map[plan.condition.0].expect("bulk-copy condition local"),
        ));
        function.instruction(&Instruction::Select);
        function.instruction(&Instruction::LocalTee(
            local_map[plan.next_pc.0].expect("bulk-copy next-PC local"),
        ));
        function.instruction(&Instruction::LocalSet(state.pc));
        emit_bulk_retired(
            function,
            state.retired,
            temps.result,
            plan.bytes_per_iteration,
            region.retired,
        );
        function.instruction(&Instruction::End);
    }
    Ok(())
}

fn emit_cached_side_exit(
    function: &mut Function,
    region: &Region,
    layout: JitLayout,
    state: &CachedStateLocals,
    local_map: &[Option<u32>],
    exit: &SideExit,
    dynamic_pc: Option<u32>,
) -> Result<(), EmitError> {
    emit_cached_side_exit_state(function, region, layout, state, local_map, exit, dynamic_pc)?;
    emit_cached_retirement(function, layout, state, exit.retired);
    Ok(())
}

fn emit_cached_side_exit_state(
    function: &mut Function,
    region: &Region,
    layout: JitLayout,
    state: &CachedStateLocals,
    local_map: &[Option<u32>],
    exit: &SideExit,
    dynamic_pc: Option<u32>,
) -> Result<(), EmitError> {
    // First synchronize values retained from earlier cached members. Values
    // dirtied by the current member are published directly from the precise
    // SideExit snapshot below. In particular, a value written only before an
    // opaque effect may have no function-wide cached-state representation at
    // all, because it is neither read nor live after that effect.
    for (reg, state_local) in state.x.iter().copied().enumerate() {
        let Some(state_local) = state_local else {
            continue;
        };
        if state.write_x & (1u32 << reg) == 0 {
            continue;
        }
        if exit.outputs.iter().any(|&(r, _)| r as usize == reg) {
            continue;
        }
        if let Some(valid) = state.valid_x {
            emit_valid_i64(function, valid, reg);
            function.instruction(&Instruction::If(BlockType::Empty));
        }
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        function.instruction(&Instruction::LocalGet(state_local));
        function.instruction(&Instruction::I64Store(memarg(3, reg as u64 * 8)));
        if state.valid_x.is_some() {
            function.instruction(&Instruction::End);
        }
    }
    // Publish every integer value dirtied by this member before the precise
    // boundary, independently of the cached-state residency policy. This is
    // the authoritative architectural snapshot consumed by helpers and T0.
    for &(reg, value) in &exit.outputs {
        let value_local = local_map[value.0]
            .ok_or_else(|| EmitError("missing cached integer exit value".into()))?;
        emit_materialized_x_store(function, layout, reg, value_local);
    }
    for (reg, state_local) in state.f.iter().copied().enumerate() {
        let Some(state_local) = state_local else {
            continue;
        };
        if state.write_f & (1u32 << reg) == 0 {
            continue;
        }
        if exit.f_outputs.iter().any(|&(r, _)| r as usize == reg) {
            continue;
        }
        if let Some(valid) = state.valid_f {
            emit_valid_i64(function, valid, reg);
            function.instruction(&Instruction::If(BlockType::Empty));
        }
        function.instruction(&Instruction::I32Const(layout.f_base as i32));
        function.instruction(&Instruction::LocalGet(state_local));
        function.instruction(&Instruction::I64Store(memarg(3, reg as u64 * 8)));
        if state.valid_f.is_some() {
            function.instruction(&Instruction::End);
        }
    }
    for &(reg, value) in &exit.f_outputs {
        let value_local =
            local_map[value.0].ok_or_else(|| EmitError("missing cached FP exit value".into()))?;
        function.instruction(&Instruction::I32Const(layout.f_base as i32));
        function.instruction(&Instruction::LocalGet(value_local));
        function.instruction(&Instruction::I64Store(memarg(3, u64::from(reg) * 8)));
    }
    if let Some(value) = exit.fcsr_output {
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        function.instruction(&Instruction::LocalGet(
            local_map[value.0].ok_or_else(|| EmitError("missing cached fcsr exit value".into()))?,
        ));
        function.instruction(&Instruction::I32Store(memarg(2, 0)));
    } else if let Some(state_local) = state.fcsr.filter(|_| state.write_fcsr) {
        if let Some(valid) = state.valid_fcsr {
            function.instruction(&Instruction::LocalGet(valid));
            function.instruction(&Instruction::If(BlockType::Empty));
        }
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        function.instruction(&Instruction::LocalGet(state_local));
        function.instruction(&Instruction::I32Store(memarg(2, 0)));
        if state.valid_fcsr.is_some() {
            function.instruction(&Instruction::End);
        }
    }
    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
    if let Some(local) = dynamic_pc {
        function.instruction(&Instruction::LocalGet(local));
    } else {
        emit_guest_pc(function, exit.guest_pc, layout);
    }
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
    // The region argument documents that the snapshot belongs to this body
    // and keeps the API symmetric with the ordinary emitter.
    let _ = region;
    Ok(())
}

fn emit_cached_retirement(
    function: &mut Function,
    layout: JitLayout,
    state: &CachedStateLocals,
    extra: u32,
) {
    function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
    function.instruction(&Instruction::LocalGet(state.retired));
    if extra != 0 {
        function.instruction(&Instruction::I64Const(i64::from(extra)));
        function.instruction(&Instruction::I64Add);
    }
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
}

fn validate_emission(region: &Region, layout: JitLayout) -> Result<(), EmitError> {
    region
        .validate()
        .map_err(|error| EmitError(error.to_string()))?;
    if layout.mem.is_some() && layout.sys.is_some() {
        return Err(EmitError(
            "a region cannot use flat and full-system memory capabilities together".into(),
        ));
    }
    if let Some(memory) = layout.sys {
        validate_system_memory(memory)?;
    }
    if region.has_vector_helper() {
        match layout.vector {
            Some(VectorCapability::User) if layout.mem.is_some() && layout.sys.is_none() => {}
            Some(VectorCapability::System) if layout.sys.is_some() && layout.mem.is_none() => {}
            Some(VectorCapability::User) => {
                return Err(EmitError(
                    "user vector capability requires flat user memory".into(),
                ));
            }
            Some(VectorCapability::System) => {
                return Err(EmitError(
                    "system vector capability requires full-system memory".into(),
                ));
            }
            None => return Err(EmitError("vector effect lacks a typed capability".into())),
        }
    }
    Ok(())
}

fn emit_function(
    region: &Region,
    layout: JitLayout,
    loop_backedge: Option<LoopBackedge>,
    helpers: HelperImports,
) -> Result<Function, EmitError> {
    let loop_backedge = (!region.has_vector_helper())
        .then_some(loop_backedge)
        .flatten();
    if region.has_effects() {
        return match loop_backedge {
            Some(loop_backedge) => emit_effectful_loop(region, layout, loop_backedge, helpers),
            None => emit_effectful(region, layout, helpers),
        };
    }

    if let Some(loop_backedge) = loop_backedge {
        return emit_single_latch_loop(region, layout, loop_backedge);
    }

    let uses = region.use_counts();
    // Values with multiple users are materialized once. Architectural reads
    // are always materialized before any state store, even with one user: a
    // JALR may write the same register it used to compute its target.
    let materialized: Vec<bool> = region
        .values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            uses[index] > 1 || matches!(value.op, Op::ReadX(_) | Op::ReadF(_) | Op::ReadFcsr)
        })
        .collect();
    let mut local_map = vec![None; region.values.len()];
    let mut local_types = Vec::new();
    for (index, value) in region.values.iter().enumerate() {
        if materialized[index] {
            // Local zero is the opaque state-pointer parameter.
            let local = 1 + local_types.len() as u32;
            local_map[index] = Some(local);
            local_types.push(val_type(value.ty));
        }
    }

    let mut function = Function::new_with_locals_types(local_types);
    let mut defined = vec![false; region.values.len()];
    for index in 0..region.values.len() {
        if let Some(local) = local_map[index] {
            emit_value_body(
                &mut function,
                region,
                layout,
                &local_map,
                &defined,
                ValueId(index),
            )?;
            function.instruction(&Instruction::LocalSet(local));
            defined[index] = true;
        }
    }

    // Commit only final dirty register values.
    for &(reg, value) in &region.outputs {
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        emit_value(&mut function, region, layout, &local_map, &defined, value)?;
        function.instruction(&Instruction::I64Store(memarg(3, u64::from(reg) * 8)));
    }
    emit_commit_f_outputs(
        &mut function,
        layout,
        &region.f_outputs,
        |function, value| emit_value(function, region, layout, &local_map, &defined, value),
    )?;
    if let Some(value) = region.fcsr_output {
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        emit_value(&mut function, region, layout, &local_map, &defined, value)?;
        function.instruction(&Instruction::I32Store(memarg(2, 0)));
    }

    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
    emit_value(
        &mut function,
        region,
        layout,
        &local_map,
        &defined,
        region.next_pc,
    )?;
    function.instruction(&Instruction::I64Store(memarg(3, 0)));

    // Retirement is cumulative across a host dispatch so future compiled edge
    // transfers can remain inside Wasm without losing accounting.
    function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
    function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Const(i64::from(region.retired)));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
    function.instruction(&Instruction::End);

    Ok(function)
}

/// Emit an ordered region with explicit guest-memory bounds checks.
///
/// A WebAssembly out-of-bounds trap is a host failure, not an RV64 exception,
/// so every access is checked against the flat guest-memory capability first.
/// Failure commits state as of immediately before the access and returns to
/// the architectural interpreter, which performs the instruction and reports
/// the exact guest exception when appropriate.
fn emit_effectful(
    region: &Region,
    layout: JitLayout,
    helpers: HelperImports,
) -> Result<Function, EmitError> {
    // Ordered effects make unrestricted stackification invalid across a load
    // or store. Locals also give side exits stable values to commit.
    let local_map: Vec<Option<u32>> = (0..region.values.len())
        .map(|index| Some(1 + index as u32))
        .collect();
    let mut local_types: Vec<ValType> = region
        .values
        .iter()
        .map(|value| val_type(value.ty))
        .collect();
    let memory_temps = allocate_memory_temps(&mut local_types, region, layout);
    let vector_status = region
        .has_vector_helper()
        .then(|| alloc_local(&mut local_types, ValType::I32));
    let vector_temps = allocate_vector_temps(
        &mut local_types,
        region_has_direct_vector(region, layout),
        region_has_unmasked_unit_stride(region, layout),
        memory_temps.map(|temps| temps.context),
        layout.sys.is_some(),
    );
    let mut function = Function::new_with_locals_types(local_types);
    emit_memory_context_init(&mut function, layout, memory_temps);
    emit_vector_memory_context_init(&mut function, layout, vector_temps, memory_temps);
    emit_vector_unit_lmul1_refresh(&mut function, layout, vector_temps);
    // Validation guarantees operands precede their users. Marking all locals
    // as addressable makes the shared pure-value emitter issue LocalGet rather
    // than recursively duplicating an earlier computation.
    let all_defined = vec![true; region.values.len()];
    let copy_plan = match (layout.sys, memory_temps.and_then(|temps| temps.copy)) {
        (Some(memory), Some(_)) => {
            dense_copy_plan(region, usize::from(memory.cache_min_accesses.max(1)))
        }
        _ => None,
    }
    .filter(|_| bulk_copy_loop_plan(region).is_none());
    let store_plan = match (layout.sys, memory_temps.and_then(|temps| temps.copy)) {
        (Some(memory), Some(_)) => {
            dense_store_plan(region, usize::from(memory.cache_min_accesses.max(1)))
        }
        _ => None,
    };
    for position in 0..=region.values.len() {
        for effect in &region.effects {
            match effect {
                Effect::Store {
                    position: store_position,
                    address,
                    value,
                    kind,
                    condition,
                    exit,
                } if *store_position == position => {
                    if let Some(condition) = condition {
                        function.instruction(&Instruction::LocalGet(1 + condition.0 as u32));
                        function.instruction(&Instruction::If(BlockType::Empty));
                    }
                    let copy_access = copy_plan
                        .as_ref()
                        .and_then(|plan| plan.store_access(*store_position, *address, *value));
                    if let (Some(access), Some(copy)) =
                        (copy_access, memory_temps.and_then(|temps| temps.copy))
                    {
                        if copy_plan
                            .as_ref()
                            .is_some_and(|plan| plan.setup_position == *store_position)
                        {
                            emit_dense_copy_setup(
                                &mut function,
                                copy_plan.as_ref().expect("copy plan"),
                                layout,
                                helpers,
                                memory_temps.expect("copy memory temporaries"),
                                &local_map,
                                |function| {
                                    emit_side_exit(
                                        function,
                                        region,
                                        layout,
                                        &local_map,
                                        &all_defined,
                                        exit,
                                    )
                                },
                            )?;
                        }
                        function.instruction(&Instruction::LocalGet(copy.destination_linear));
                        function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
                        function.instruction(&Instruction::I64Store(memarg(
                            3,
                            access.destination_offset,
                        )));
                    } else if let (Some(access), Some(range)) = (
                        store_plan
                            .as_ref()
                            .and_then(|plan| plan.store_access(*store_position, *address)),
                        memory_temps.and_then(|temps| temps.copy),
                    ) {
                        if store_plan
                            .as_ref()
                            .is_some_and(|plan| plan.setup_position == *store_position)
                        {
                            emit_dense_store_setup(
                                &mut function,
                                store_plan.as_ref().expect("store plan"),
                                layout,
                                helpers,
                                memory_temps.expect("store memory temporaries"),
                                &local_map,
                                |function| {
                                    emit_side_exit(
                                        function,
                                        region,
                                        layout,
                                        &local_map,
                                        &all_defined,
                                        exit,
                                    )
                                },
                            )?;
                        }
                        if let Some(fill_value) =
                            store_plan.as_ref().and_then(|plan| plan.fill_value)
                        {
                            if store_plan
                                .as_ref()
                                .is_some_and(|plan| plan.setup_position == *store_position)
                            {
                                emit_dense_fill(
                                    &mut function,
                                    store_plan.as_ref().expect("store plan"),
                                    range.destination_linear,
                                    1 + fill_value.0 as u32,
                                )?;
                            }
                        } else {
                            function.instruction(&Instruction::LocalGet(range.destination_linear));
                            function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
                            function.instruction(&Instruction::I64Store(memarg(
                                3,
                                access.destination_offset,
                            )));
                        }
                    } else {
                        emit_memory_address(
                            &mut function,
                            layout,
                            helpers,
                            memory_temps,
                            1 + address.0 as u32,
                            kind.bytes(),
                            true,
                            |function| {
                                emit_side_exit(
                                    function,
                                    region,
                                    layout,
                                    &local_map,
                                    &all_defined,
                                    exit,
                                )
                            },
                        )?;
                        function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
                        let instruction = match kind {
                            StoreKind::I8 => Instruction::I64Store8(memarg(0, 0)),
                            StoreKind::I16 => Instruction::I64Store16(memarg(0, 0)),
                            StoreKind::I32 => Instruction::I64Store32(memarg(0, 0)),
                            StoreKind::I64 => Instruction::I64Store(memarg(0, 0)),
                        };
                        function.instruction(&instruction);
                    }
                    if condition.is_some() {
                        function.instruction(&Instruction::End);
                        emit_reservation_clear(&mut function, helpers, 1 + address.0 as u32)?;
                    }
                }
                Effect::Guard {
                    position: guard_position,
                    condition,
                    exit,
                } if *guard_position == position => {
                    function.instruction(&Instruction::LocalGet(1 + condition.0 as u32));
                    function.instruction(&Instruction::If(BlockType::Empty));
                    emit_side_exit(
                        &mut function,
                        region,
                        layout,
                        &local_map,
                        &all_defined,
                        exit,
                    )?;
                    function.instruction(&Instruction::Return);
                    function.instruction(&Instruction::End);
                }
                Effect::GuardTarget {
                    position: guard_position,
                    target,
                    expected,
                    exit,
                } if *guard_position == position => {
                    function.instruction(&Instruction::LocalGet(1 + target.0 as u32));
                    emit_guest_pc(&mut function, *expected, layout);
                    function.instruction(&Instruction::I64Ne);
                    function.instruction(&Instruction::If(BlockType::Empty));
                    emit_side_exit(
                        &mut function,
                        region,
                        layout,
                        &local_map,
                        &all_defined,
                        exit,
                    )?;
                    emit_pc_from_local(&mut function, layout, 1 + target.0 as u32);
                    emit_ic_guard_miss(&mut function, region, layout, 1 + target.0 as u32);
                    function.instruction(&Instruction::Return);
                    function.instruction(&Instruction::End);
                }
                Effect::FpState {
                    position: fp_position,
                    dirty,
                    exit,
                } if *fp_position == position => {
                    emit_fp_state(&mut function, layout, *dirty, |function| {
                        emit_side_exit(function, region, layout, &local_map, &all_defined, exit)
                    })?;
                }
                Effect::VectorState {
                    position: vector_position,
                    exit,
                } if *vector_position == position => {
                    emit_vector_state(&mut function, layout, |function| {
                        emit_side_exit(function, region, layout, &local_map, &all_defined, exit)
                    })?;
                }
                Effect::Vector {
                    position: vector_position,
                    insn,
                    direct,
                    fallthrough,
                    exit,
                } if *vector_position == position => {
                    let status = vector_status.expect("vector effect status local");
                    if vector_config_instruction(*insn) {
                        emit_vector_unit_lmul1_invalidate(&mut function, vector_temps);
                    }
                    emit_side_exit_state(
                        &mut function,
                        region,
                        layout,
                        &local_map,
                        &all_defined,
                        exit,
                    )?;
                    let mut prepare_fallback = |_function: &mut Function| Ok(());
                    emit_vector_execution(
                        &mut function,
                        layout,
                        helpers,
                        *insn,
                        *direct,
                        vector_temps,
                        status,
                        &mut prepare_fallback,
                        false,
                    )?;

                    // Zero means the architectural instruction did not
                    // complete. Leave it unretired at its original PC so T0
                    // can raise the exact trap (or restart from vstart).
                    function.instruction(&Instruction::LocalGet(status));
                    function.instruction(&Instruction::I32Eqz);
                    function.instruction(&Instruction::If(BlockType::Empty));
                    emit_retirement_const(&mut function, layout.retired_addr, exit.retired);
                    function.instruction(&Instruction::Return);
                    function.instruction(&Instruction::End);

                    // Status two is a completed system vector store that
                    // dirtied generated code. Stop after this instruction so
                    // the runtime invalidates that page before more code runs.
                    function.instruction(&Instruction::LocalGet(status));
                    function.instruction(&Instruction::I32Const(2));
                    function.instruction(&Instruction::I32Eq);
                    function.instruction(&Instruction::If(BlockType::Empty));
                    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
                    emit_guest_pc(&mut function, *fallthrough, layout);
                    function.instruction(&Instruction::I64Store(memarg(3, 0)));
                    emit_retirement_const(
                        &mut function,
                        layout.retired_addr,
                        exit.retired.saturating_add(1),
                    );
                    function.instruction(&Instruction::Return);
                    function.instruction(&Instruction::End);
                    if vector_config_instruction(*insn) {
                        emit_vector_unit_lmul1_refresh(&mut function, layout, vector_temps);
                    }
                }
                _ => {}
            }
        }

        let Some(data) = region.values.get(position) else {
            continue;
        };
        match &data.op {
            Op::Load {
                address,
                kind,
                exit,
            } => {
                let copy_access = copy_plan
                    .as_ref()
                    .and_then(|plan| plan.load_access(ValueId(position)));
                if let (Some(access), Some(copy)) =
                    (copy_access, memory_temps.and_then(|temps| temps.copy))
                {
                    if copy_plan
                        .as_ref()
                        .is_some_and(|plan| plan.setup_position == position)
                    {
                        emit_dense_copy_setup(
                            &mut function,
                            copy_plan.as_ref().expect("copy plan"),
                            layout,
                            helpers,
                            memory_temps.expect("copy memory temporaries"),
                            &local_map,
                            |function| {
                                emit_side_exit(
                                    function,
                                    region,
                                    layout,
                                    &local_map,
                                    &all_defined,
                                    exit,
                                )
                            },
                        )?;
                    }
                    function.instruction(&Instruction::LocalGet(copy.source_linear));
                    function.instruction(&Instruction::I64Load(memarg(3, access.source_offset)));
                } else {
                    emit_memory_address(
                        &mut function,
                        layout,
                        helpers,
                        memory_temps,
                        1 + address.0 as u32,
                        kind.bytes(),
                        false,
                        |function| {
                            emit_side_exit(function, region, layout, &local_map, &all_defined, exit)
                        },
                    )?;
                    let instruction = match kind {
                        LoadKind::I8S => Instruction::I64Load8S(memarg(0, 0)),
                        LoadKind::I16S => Instruction::I64Load16S(memarg(0, 0)),
                        LoadKind::I32S => Instruction::I64Load32S(memarg(0, 0)),
                        LoadKind::I64 => Instruction::I64Load(memarg(0, 0)),
                        LoadKind::I8U => Instruction::I64Load8U(memarg(0, 0)),
                        LoadKind::I16U => Instruction::I64Load16U(memarg(0, 0)),
                        LoadKind::I32U => Instruction::I64Load32U(memarg(0, 0)),
                    };
                    function.instruction(&instruction);
                }
            }
            Op::ExactFp {
                op,
                lhs,
                rhs,
                third,
                rm,
                fcsr,
                exit,
            } => {
                // Dynamic frm values 5..7 are reserved. Returning to T0 lets
                // the interpreter raise the architectural illegal instruction
                // instead of invoking the helper with an invented mode.
                function.instruction(&Instruction::LocalGet(1 + rm.0 as u32));
                function.instruction(&Instruction::I32Const(4));
                function.instruction(&Instruction::I32GtU);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_side_exit(
                    &mut function,
                    region,
                    layout,
                    &local_map,
                    &all_defined,
                    exit,
                )?;
                function.instruction(&Instruction::Return);
                function.instruction(&Instruction::End);

                emit_exact_fp_value(
                    &mut function,
                    *op,
                    1 + lhs.0 as u32,
                    1 + rhs.0 as u32,
                    1 + third.0 as u32,
                    1 + rm.0 as u32,
                    1 + fcsr.0 as u32,
                    1 + position as u32,
                    layout,
                    helpers,
                )?;
            }
            Op::Reservation { op, address } => {
                function.instruction(&Instruction::I32Const(match op {
                    ReservationOp::LoadReserved => 0,
                    ReservationOp::StoreConditional => 1,
                }));
                function.instruction(&Instruction::LocalGet(0));
                function.instruction(&Instruction::LocalGet(1 + address.0 as u32));
                function.instruction(&Instruction::Call(
                    helpers
                        .reservation_index()
                        .ok_or_else(|| EmitError("missing reservation helper import".into()))?,
                ));
            }
            _ => emit_value_body(
                &mut function,
                region,
                layout,
                &local_map,
                &all_defined,
                ValueId(position),
            )?,
        }
        function.instruction(&Instruction::LocalSet(1 + position as u32));
    }

    emit_commit_outputs(&mut function, layout, &region.outputs, |function, value| {
        function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
        Ok(())
    })?;
    emit_commit_f_outputs(
        &mut function,
        layout,
        &region.f_outputs,
        |function, value| {
            function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
            Ok(())
        },
    )?;
    if let Some(value) = region.fcsr_output {
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
        function.instruction(&Instruction::I32Store(memarg(2, 0)));
    }
    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
    function.instruction(&Instruction::LocalGet(1 + region.next_pc.0 as u32));
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
    emit_retirement_const(&mut function, layout.retired_addr, region.retired);
    function.instruction(&Instruction::End);

    Ok(function)
}

/// Memory-aware single-latch loop lowering. Dirty architectural registers get
/// dedicated carry locals so a memory fault at the beginning of a later
/// iteration can still materialize the state produced by earlier iterations.
fn emit_effectful_loop(
    region: &Region,
    layout: JitLayout,
    loop_backedge: LoopBackedge,
    helpers: HelperImports,
) -> Result<Function, EmitError> {
    let local_map: Vec<Option<u32>> = (0..region.values.len())
        .map(|index| Some(1 + index as u32))
        .collect();
    let retired_local = 1 + region.values.len() as u32;
    let next_pc_local = retired_local + 1;
    let mut carry_map = [None; 32];
    let mut f_carry_map = [None; 32];
    let mut fcsr_carry = None;
    let mut local_types: Vec<ValType> = region.values.iter().map(|v| val_type(v.ty)).collect();
    local_types.extend([ValType::I64, ValType::I64]);
    for &(reg, _) in &region.outputs {
        carry_map[reg as usize] = Some(1 + local_types.len() as u32);
        local_types.push(ValType::I64);
    }
    for &(reg, _) in &region.f_outputs {
        f_carry_map[reg as usize] = Some(1 + local_types.len() as u32);
        local_types.push(ValType::I64);
    }
    if region.fcsr_output.is_some() {
        fcsr_carry = Some(1 + local_types.len() as u32);
        local_types.push(ValType::I32);
    }
    let memory_temps = allocate_memory_temps(&mut local_types, region, layout);
    let mut function = Function::new_with_locals_types(local_types);
    emit_memory_context_init(&mut function, layout, memory_temps);
    let all_defined = vec![true; region.values.len()];
    let copy_plan = match (layout.sys, memory_temps.and_then(|temps| temps.copy)) {
        (Some(memory), Some(_)) => {
            dense_copy_plan(region, usize::from(memory.cache_min_accesses.max(1)))
        }
        _ => None,
    }
    .filter(|_| bulk_copy_loop_plan(region).is_none());
    let store_plan = match (layout.sys, memory_temps.and_then(|temps| temps.copy)) {
        (Some(memory), Some(_)) => {
            dense_store_plan(region, usize::from(memory.cache_min_accesses.max(1)))
        }
        _ => None,
    };
    let bulk_plan = bulk_copy_loop_plan(region).filter(|plan| {
        loop_backedge.condition == Some(plan.condition) && loop_backedge.exit_pc == plan.exit_pc
    });
    let bulk_temps = bulk_plan.and_then(|_| memory_temps.and_then(|temps| temps.bulk_copy));

    // Capture the complete dirty-register state at loop entry. This includes
    // registers written but not read by the static body.
    for &(reg, _) in &region.outputs {
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        function.instruction(&Instruction::I64Load(memarg(3, u64::from(reg) * 8)));
        function.instruction(&Instruction::LocalSet(
            carry_map[reg as usize].expect("dirty register has a carry local"),
        ));
    }
    for &(reg, _) in &region.f_outputs {
        function.instruction(&Instruction::I32Const(layout.f_base as i32));
        function.instruction(&Instruction::I64Load(memarg(3, u64::from(reg) * 8)));
        function.instruction(&Instruction::LocalSet(
            f_carry_map[reg as usize].expect("dirty FP register has a carry local"),
        ));
    }
    if let Some(carry) = fcsr_carry {
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        function.instruction(&Instruction::I32Load(memarg(2, 0)));
        function.instruction(&Instruction::LocalSet(carry));
    }
    for (index, value) in region.values.iter().enumerate() {
        match value.op {
            Op::ReadX(reg) => {
                if let Some(carry) = carry_map[reg as usize] {
                    function.instruction(&Instruction::LocalGet(carry));
                } else {
                    function.instruction(&Instruction::I32Const(layout.x_base as i32));
                    function.instruction(&Instruction::I64Load(memarg(3, u64::from(reg) * 8)));
                }
            }
            Op::ReadF(reg) => {
                if let Some(carry) = f_carry_map[reg as usize] {
                    function.instruction(&Instruction::LocalGet(carry));
                } else {
                    function.instruction(&Instruction::I32Const(layout.f_base as i32));
                    function.instruction(&Instruction::I64Load(memarg(3, u64::from(reg) * 8)));
                }
            }
            Op::ReadFcsr => {
                if let Some(carry) = fcsr_carry {
                    function.instruction(&Instruction::LocalGet(carry));
                } else {
                    function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
                    function.instruction(&Instruction::I32Load(memarg(2, 0)));
                }
            }
            _ => continue,
        }
        function.instruction(&Instruction::LocalSet(1 + index as u32));
    }
    function.instruction(&Instruction::I64Const(0));
    function.instruction(&Instruction::LocalSet(retired_local));

    if let (Some(plan), Some(temps)) = (bulk_plan, bulk_temps) {
        let source = read_x_value(region, plan.source_reg)
            .ok_or_else(|| EmitError("bulk-copy source has no ReadX value".into()))?;
        let destination = read_x_value(region, plan.destination_reg)
            .ok_or_else(|| EmitError("bulk-copy destination has no ReadX value".into()))?;
        let count = read_x_value(region, plan.count_reg)
            .ok_or_else(|| EmitError("bulk-copy count has no ReadX value".into()))?;
        let limit = read_x_value(region, plan.limit_reg)
            .ok_or_else(|| EmitError("bulk-copy limit has no ReadX value".into()))?;
        emit_bulk_copy_call(
            &mut function,
            plan,
            region.retired,
            helpers,
            temps,
            1 + source.0 as u32,
            1 + destination.0 as u32,
            1 + count.0 as u32,
            1 + limit.0 as u32,
            retired_local,
            None,
            layout.fuel_addr,
        )?;
        function.instruction(&Instruction::LocalGet(temps.result));
        function.instruction(&Instruction::I64Const(0));
        function.instruction(&Instruction::I64Ne);
        function.instruction(&Instruction::If(BlockType::Empty));
        let subtract_pointers = plan.step < 0;
        emit_add_or_sub_local(
            &mut function,
            carry_map[plan.source_reg as usize].expect("bulk-copy source carry"),
            temps.result,
            subtract_pointers,
        );
        emit_add_or_sub_local(
            &mut function,
            carry_map[plan.destination_reg as usize].expect("bulk-copy destination carry"),
            temps.result,
            subtract_pointers,
        );
        emit_add_or_sub_local(
            &mut function,
            carry_map[plan.count_reg as usize].expect("bulk-copy count carry"),
            temps.result,
            true,
        );
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        function.instruction(&Instruction::I64Load(memarg(
            3,
            u64::from(plan.value_reg) * 8,
        )));
        function.instruction(&Instruction::LocalSet(
            carry_map[plan.value_reg as usize].expect("bulk-copy value carry"),
        ));
        function.instruction(&Instruction::I64Const(plan.limit_value as i64));
        function.instruction(&Instruction::LocalGet(
            carry_map[plan.count_reg as usize].expect("bulk-copy count carry"),
        ));
        function.instruction(&Instruction::I64LtU);
        function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
        emit_guest_pc(&mut function, region.entry_pc, layout);
        function.instruction(&Instruction::Else);
        emit_guest_pc(&mut function, plan.exit_pc, layout);
        function.instruction(&Instruction::End);
        function.instruction(&Instruction::LocalSet(next_pc_local));
        emit_bulk_retired(
            &mut function,
            retired_local,
            temps.result,
            plan.bytes_per_iteration,
            region.retired,
        );
        for &(reg, _) in &region.outputs {
            function.instruction(&Instruction::I32Const(layout.x_base as i32));
            function.instruction(&Instruction::LocalGet(
                carry_map[reg as usize].expect("bulk-copy output carry"),
            ));
            function.instruction(&Instruction::I64Store(memarg(3, u64::from(reg) * 8)));
        }
        function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
        function.instruction(&Instruction::LocalGet(next_pc_local));
        function.instruction(&Instruction::I64Store(memarg(3, 0)));
        emit_retirement_local(&mut function, layout.retired_addr, retired_local, 0);
        function.instruction(&Instruction::Return);
        function.instruction(&Instruction::End);
    }

    function.instruction(&Instruction::Block(BlockType::Empty));
    function.instruction(&Instruction::Loop(BlockType::Empty));
    for position in 0..=region.values.len() {
        for effect in &region.effects {
            match effect {
                Effect::Store {
                    position: store_position,
                    address,
                    value,
                    kind,
                    condition,
                    exit,
                } if *store_position == position => {
                    if let Some(condition) = condition {
                        function.instruction(&Instruction::LocalGet(1 + condition.0 as u32));
                        function.instruction(&Instruction::If(BlockType::Empty));
                    }
                    let copy_access = copy_plan
                        .as_ref()
                        .and_then(|plan| plan.store_access(*store_position, *address, *value));
                    if let (Some(access), Some(copy)) =
                        (copy_access, memory_temps.and_then(|temps| temps.copy))
                    {
                        if copy_plan
                            .as_ref()
                            .is_some_and(|plan| plan.setup_position == *store_position)
                        {
                            emit_dense_copy_setup(
                                &mut function,
                                copy_plan.as_ref().expect("copy plan"),
                                layout,
                                helpers,
                                memory_temps.expect("copy memory temporaries"),
                                &local_map,
                                |function| {
                                    emit_loop_side_exit(
                                        function,
                                        region,
                                        layout,
                                        exit,
                                        retired_local,
                                        &carry_map,
                                        &f_carry_map,
                                        fcsr_carry,
                                    )
                                },
                            )?;
                        }
                        function.instruction(&Instruction::LocalGet(copy.destination_linear));
                        function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
                        function.instruction(&Instruction::I64Store(memarg(
                            3,
                            access.destination_offset,
                        )));
                    } else if let (Some(access), Some(range)) = (
                        store_plan
                            .as_ref()
                            .and_then(|plan| plan.store_access(*store_position, *address)),
                        memory_temps.and_then(|temps| temps.copy),
                    ) {
                        if store_plan
                            .as_ref()
                            .is_some_and(|plan| plan.setup_position == *store_position)
                        {
                            emit_dense_store_setup(
                                &mut function,
                                store_plan.as_ref().expect("store plan"),
                                layout,
                                helpers,
                                memory_temps.expect("store memory temporaries"),
                                &local_map,
                                |function| {
                                    emit_loop_side_exit(
                                        function,
                                        region,
                                        layout,
                                        exit,
                                        retired_local,
                                        &carry_map,
                                        &f_carry_map,
                                        fcsr_carry,
                                    )
                                },
                            )?;
                        }
                        if let Some(fill_value) =
                            store_plan.as_ref().and_then(|plan| plan.fill_value)
                        {
                            if store_plan
                                .as_ref()
                                .is_some_and(|plan| plan.setup_position == *store_position)
                            {
                                emit_dense_fill(
                                    &mut function,
                                    store_plan.as_ref().expect("store plan"),
                                    range.destination_linear,
                                    1 + fill_value.0 as u32,
                                )?;
                            }
                        } else {
                            function.instruction(&Instruction::LocalGet(range.destination_linear));
                            function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
                            function.instruction(&Instruction::I64Store(memarg(
                                3,
                                access.destination_offset,
                            )));
                        }
                    } else {
                        emit_memory_address(
                            &mut function,
                            layout,
                            helpers,
                            memory_temps,
                            1 + address.0 as u32,
                            kind.bytes(),
                            true,
                            |function| {
                                emit_loop_side_exit(
                                    function,
                                    region,
                                    layout,
                                    exit,
                                    retired_local,
                                    &carry_map,
                                    &f_carry_map,
                                    fcsr_carry,
                                )
                            },
                        )?;
                        function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
                        let instruction = match kind {
                            StoreKind::I8 => Instruction::I64Store8(memarg(0, 0)),
                            StoreKind::I16 => Instruction::I64Store16(memarg(0, 0)),
                            StoreKind::I32 => Instruction::I64Store32(memarg(0, 0)),
                            StoreKind::I64 => Instruction::I64Store(memarg(0, 0)),
                        };
                        function.instruction(&instruction);
                    }
                    if condition.is_some() {
                        function.instruction(&Instruction::End);
                        emit_reservation_clear(&mut function, helpers, 1 + address.0 as u32)?;
                    }
                }
                Effect::Guard {
                    position: guard_position,
                    condition,
                    exit,
                } if *guard_position == position => {
                    function.instruction(&Instruction::LocalGet(1 + condition.0 as u32));
                    function.instruction(&Instruction::If(BlockType::Empty));
                    emit_loop_side_exit(
                        &mut function,
                        region,
                        layout,
                        exit,
                        retired_local,
                        &carry_map,
                        &f_carry_map,
                        fcsr_carry,
                    )?;
                    function.instruction(&Instruction::Return);
                    function.instruction(&Instruction::End);
                }
                Effect::GuardTarget {
                    position: guard_position,
                    target,
                    expected,
                    exit,
                } if *guard_position == position => {
                    function.instruction(&Instruction::LocalGet(1 + target.0 as u32));
                    emit_guest_pc(&mut function, *expected, layout);
                    function.instruction(&Instruction::I64Ne);
                    function.instruction(&Instruction::If(BlockType::Empty));
                    emit_loop_side_exit(
                        &mut function,
                        region,
                        layout,
                        exit,
                        retired_local,
                        &carry_map,
                        &f_carry_map,
                        fcsr_carry,
                    )?;
                    emit_pc_from_local(&mut function, layout, 1 + target.0 as u32);
                    emit_ic_guard_miss(&mut function, region, layout, 1 + target.0 as u32);
                    function.instruction(&Instruction::Return);
                    function.instruction(&Instruction::End);
                }
                Effect::FpState {
                    position: fp_position,
                    dirty,
                    exit,
                } if *fp_position == position => {
                    emit_fp_state(&mut function, layout, *dirty, |function| {
                        emit_loop_side_exit(
                            function,
                            region,
                            layout,
                            exit,
                            retired_local,
                            &carry_map,
                            &f_carry_map,
                            fcsr_carry,
                        )
                    })?;
                }
                Effect::VectorState {
                    position: vector_position,
                    exit,
                } if *vector_position == position => {
                    emit_vector_state(&mut function, layout, |function| {
                        emit_loop_side_exit(
                            function,
                            region,
                            layout,
                            exit,
                            retired_local,
                            &carry_map,
                            &f_carry_map,
                            fcsr_carry,
                        )
                    })?;
                }
                _ => {}
            }
        }

        let Some(data) = region.values.get(position) else {
            continue;
        };
        if matches!(data.op, Op::ReadX(_) | Op::ReadF(_) | Op::ReadFcsr) {
            continue;
        }
        match &data.op {
            Op::Load {
                address,
                kind,
                exit,
            } => {
                let copy_access = copy_plan
                    .as_ref()
                    .and_then(|plan| plan.load_access(ValueId(position)));
                if let (Some(access), Some(copy)) =
                    (copy_access, memory_temps.and_then(|temps| temps.copy))
                {
                    if copy_plan
                        .as_ref()
                        .is_some_and(|plan| plan.setup_position == position)
                    {
                        emit_dense_copy_setup(
                            &mut function,
                            copy_plan.as_ref().expect("copy plan"),
                            layout,
                            helpers,
                            memory_temps.expect("copy memory temporaries"),
                            &local_map,
                            |function| {
                                emit_loop_side_exit(
                                    function,
                                    region,
                                    layout,
                                    exit,
                                    retired_local,
                                    &carry_map,
                                    &f_carry_map,
                                    fcsr_carry,
                                )
                            },
                        )?;
                    }
                    function.instruction(&Instruction::LocalGet(copy.source_linear));
                    function.instruction(&Instruction::I64Load(memarg(3, access.source_offset)));
                } else {
                    emit_memory_address(
                        &mut function,
                        layout,
                        helpers,
                        memory_temps,
                        1 + address.0 as u32,
                        kind.bytes(),
                        false,
                        |function| {
                            emit_loop_side_exit(
                                function,
                                region,
                                layout,
                                exit,
                                retired_local,
                                &carry_map,
                                &f_carry_map,
                                fcsr_carry,
                            )
                        },
                    )?;
                    let instruction = match kind {
                        LoadKind::I8S => Instruction::I64Load8S(memarg(0, 0)),
                        LoadKind::I16S => Instruction::I64Load16S(memarg(0, 0)),
                        LoadKind::I32S => Instruction::I64Load32S(memarg(0, 0)),
                        LoadKind::I64 => Instruction::I64Load(memarg(0, 0)),
                        LoadKind::I8U => Instruction::I64Load8U(memarg(0, 0)),
                        LoadKind::I16U => Instruction::I64Load16U(memarg(0, 0)),
                        LoadKind::I32U => Instruction::I64Load32U(memarg(0, 0)),
                    };
                    function.instruction(&instruction);
                }
            }
            Op::ExactFp {
                op,
                lhs,
                rhs,
                third,
                rm,
                fcsr,
                exit,
            } => {
                function.instruction(&Instruction::LocalGet(1 + rm.0 as u32));
                function.instruction(&Instruction::I32Const(4));
                function.instruction(&Instruction::I32GtU);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_loop_side_exit(
                    &mut function,
                    region,
                    layout,
                    exit,
                    retired_local,
                    &carry_map,
                    &f_carry_map,
                    fcsr_carry,
                )?;
                function.instruction(&Instruction::Return);
                function.instruction(&Instruction::End);
                emit_exact_fp_value(
                    &mut function,
                    *op,
                    1 + lhs.0 as u32,
                    1 + rhs.0 as u32,
                    1 + third.0 as u32,
                    1 + rm.0 as u32,
                    1 + fcsr.0 as u32,
                    1 + position as u32,
                    layout,
                    helpers,
                )?;
            }
            Op::Reservation { op, address } => {
                function.instruction(&Instruction::I32Const(match op {
                    ReservationOp::LoadReserved => 0,
                    ReservationOp::StoreConditional => 1,
                }));
                function.instruction(&Instruction::LocalGet(0));
                function.instruction(&Instruction::LocalGet(1 + address.0 as u32));
                function.instruction(&Instruction::Call(
                    helpers
                        .reservation_index()
                        .ok_or_else(|| EmitError("missing reservation helper import".into()))?,
                ));
            }
            _ => emit_value_body(
                &mut function,
                region,
                layout,
                &local_map,
                &all_defined,
                ValueId(position),
            )?,
        }
        function.instruction(&Instruction::LocalSet(1 + position as u32));
    }

    function.instruction(&Instruction::LocalGet(retired_local));
    function.instruction(&Instruction::I64Const(i64::from(region.retired)));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::LocalSet(retired_local));
    if let Some(condition) = loop_backedge.condition {
        function.instruction(&Instruction::LocalGet(1 + condition.0 as u32));
    } else {
        function.instruction(&Instruction::I32Const(1));
    }
    if layout.fuel_addr != 0 {
        function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::LocalGet(retired_local));
        function.instruction(&Instruction::I64Add);
        function.instruction(&Instruction::I32Const(layout.fuel_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64LtU);
        function.instruction(&Instruction::I32And);
    } else {
        function.instruction(&Instruction::I32Const(0));
        function.instruction(&Instruction::I32And);
    }
    function.instruction(&Instruction::If(BlockType::Empty));

    // Parallel-copy-safe backedge: all output values remain in their own SSA
    // locals while carry and ReadX locals are updated.
    for &(reg, output) in &region.outputs {
        function.instruction(&Instruction::LocalGet(1 + output.0 as u32));
        function.instruction(&Instruction::LocalSet(
            carry_map[reg as usize].expect("dirty register has a carry local"),
        ));
    }
    for &(reg, output) in &region.f_outputs {
        function.instruction(&Instruction::LocalGet(1 + output.0 as u32));
        function.instruction(&Instruction::LocalSet(
            f_carry_map[reg as usize].expect("dirty FP register has a carry local"),
        ));
    }
    if let (Some(output), Some(carry)) = (region.fcsr_output, fcsr_carry) {
        function.instruction(&Instruction::LocalGet(1 + output.0 as u32));
        function.instruction(&Instruction::LocalSet(carry));
    }
    for (index, value) in region.values.iter().enumerate() {
        let output = match value.op {
            Op::ReadX(reg) => region.outputs.iter().find(|&&(r, _)| r == reg).copied(),
            Op::ReadF(reg) => region.f_outputs.iter().find(|&&(r, _)| r == reg).copied(),
            Op::ReadFcsr => region.fcsr_output.map(|output| (0, output)),
            _ => None,
        };
        if let Some((_, output)) = output {
            function.instruction(&Instruction::LocalGet(1 + output.0 as u32));
            function.instruction(&Instruction::LocalSet(1 + index as u32));
        }
    }
    function.instruction(&Instruction::Br(1));
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::LocalGet(1 + region.next_pc.0 as u32));
    function.instruction(&Instruction::LocalSet(next_pc_local));
    function.instruction(&Instruction::Br(1));
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);

    emit_commit_outputs(&mut function, layout, &region.outputs, |function, value| {
        function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
        Ok(())
    })?;
    emit_commit_f_outputs(
        &mut function,
        layout,
        &region.f_outputs,
        |function, value| {
            function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
            Ok(())
        },
    )?;
    if let Some(value) = region.fcsr_output {
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
        function.instruction(&Instruction::I32Store(memarg(2, 0)));
    }
    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
    function.instruction(&Instruction::LocalGet(next_pc_local));
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
    emit_retirement_local(&mut function, layout.retired_addr, retired_local, 0);
    function.instruction(&Instruction::End);
    Ok(function)
}

#[allow(clippy::too_many_arguments)]
fn emit_loop_side_exit(
    function: &mut Function,
    region: &Region,
    layout: JitLayout,
    exit: &SideExit,
    retired_local: u32,
    carry_map: &[Option<u32>; 32],
    f_carry_map: &[Option<u32>; 32],
    fcsr_carry: Option<u32>,
) -> Result<(), EmitError> {
    for &(reg, final_output) in &region.outputs {
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        if let Some(&(_, current)) = exit.outputs.iter().find(|&&(r, _)| r == reg) {
            function.instruction(&Instruction::LocalGet(1 + current.0 as u32));
        } else {
            let carry = carry_map[reg as usize].ok_or_else(|| {
                EmitError(format!(
                    "missing loop carry for side-exit output {final_output:?}"
                ))
            })?;
            function.instruction(&Instruction::LocalGet(carry));
        }
        function.instruction(&Instruction::I64Store(memarg(3, u64::from(reg) * 8)));
    }
    for &(reg, final_output) in &region.f_outputs {
        function.instruction(&Instruction::I32Const(layout.f_base as i32));
        if let Some(&(_, current)) = exit.f_outputs.iter().find(|&&(r, _)| r == reg) {
            function.instruction(&Instruction::LocalGet(1 + current.0 as u32));
        } else {
            let carry = f_carry_map[reg as usize].ok_or_else(|| {
                EmitError(format!(
                    "missing FP loop carry for side-exit output {final_output:?}"
                ))
            })?;
            function.instruction(&Instruction::LocalGet(carry));
        }
        function.instruction(&Instruction::I64Store(memarg(3, u64::from(reg) * 8)));
    }
    if region.fcsr_output.is_some() {
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        if let Some(current) = exit.fcsr_output {
            function.instruction(&Instruction::LocalGet(1 + current.0 as u32));
        } else {
            function
                .instruction(&Instruction::LocalGet(fcsr_carry.ok_or_else(|| {
                    EmitError("missing fcsr loop carry for side exit".into())
                })?));
        }
        function.instruction(&Instruction::I32Store(memarg(2, 0)));
    }
    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
    emit_guest_pc(function, exit.guest_pc, layout);
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
    emit_retirement_local(function, layout.retired_addr, retired_local, exit.retired);
    Ok(())
}

fn emit_linear_address(function: &mut Function, memory_base: u32, address_local: u32) {
    function.instruction(&Instruction::I32Const(memory_base as i32));
    function.instruction(&Instruction::LocalGet(address_local));
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::I32Add);
}

fn validate_system_memory(memory: SystemMemory) -> Result<(), EmitError> {
    if !(3..=31).contains(&memory.page_shift) {
        return Err(EmitError(format!(
            "system-memory page shift {} is outside the supported memory32 range",
            memory.page_shift
        )));
    }
    let slots = memory
        .index_mask
        .checked_add(1)
        .ok_or_else(|| EmitError("system-memory translation mask cannot be u32::MAX".into()))?;
    if !slots.is_power_of_two() {
        return Err(EmitError(
            "system-memory translation mask must be power-of-two minus one".into(),
        ));
    }
    for (name, row) in [("load", memory.load), ("store", memory.store)] {
        if row.tags & 7 != 0 || row.offsets & 7 != 0 {
            return Err(EmitError(format!(
                "system-memory {name} row bases must be eight-byte aligned"
            )));
        }
    }
    if memory.context_addr == 0 || memory.context_addr & 7 != 0 {
        return Err(EmitError(
            "system-memory context address must be non-zero and eight-byte aligned".into(),
        ));
    }
    Ok(())
}

fn emit_translation_probe(
    function: &mut Function,
    row: TranslationRow,
    temps: MemoryTemps,
    address_local: u32,
    page_shift: u8,
) {
    function.instruction(&Instruction::I32Const(row.tags as i32));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(3));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalGet(address_local));
    let page_mask = !((1u64 << page_shift) - 1);
    function.instruction(&Instruction::I64Const(page_mask as i64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::LocalGet(temps.context));
    function.instruction(&Instruction::I64Or);
    function.instruction(&Instruction::I64Eq);
}

fn emit_translation_offset(function: &mut Function, row: TranslationRow, temps: MemoryTemps) {
    function.instruction(&Instruction::I32Const(row.offsets as i32));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(3));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
}

fn emit_fp_state(
    function: &mut Function,
    layout: JitLayout,
    dirty: bool,
    mut side_exit: impl FnMut(&mut Function) -> Result<(), EmitError>,
) -> Result<(), EmitError> {
    if layout.mstatus_addr == 0 {
        return Err(EmitError(
            "full-system FP effect requires an mstatus capability".into(),
        ));
    }
    const FS_MASK: i64 = 3 << 13;
    function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Const(FS_MASK));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::If(BlockType::Empty));
    side_exit(function)?;
    function.instruction(&Instruction::Return);
    function.instruction(&Instruction::End);

    if dirty {
        function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
        function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64Const(FS_MASK));
        function.instruction(&Instruction::I64Or);
        function.instruction(&Instruction::I64Store(memarg(3, 0)));
    }
    Ok(())
}

fn emit_vector_state(
    function: &mut Function,
    layout: JitLayout,
    mut side_exit: impl FnMut(&mut Function) -> Result<(), EmitError>,
) -> Result<(), EmitError> {
    match layout.vector {
        Some(VectorCapability::User) => return Ok(()),
        Some(VectorCapability::System) => {}
        None => {
            return Err(EmitError(
                "vector-state effect requires a vector capability".into(),
            ));
        }
    }
    if layout.mstatus_addr == 0 {
        return Err(EmitError(
            "full-system vector-state effect requires an mstatus capability".into(),
        ));
    }

    const VS_MASK: i64 = 3 << 9;
    function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Const(VS_MASK));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::If(BlockType::Empty));
    side_exit(function)?;
    function.instruction(&Instruction::Return);
    function.instruction(&Instruction::End);
    Ok(())
}

/// Leave whether the raw f64 bits in `local` encode a finite value.  The
/// generated native-arithmetic path deliberately mirrors rv64-wasm's proven
/// `fast64` predicate: NaNs and infinities always use the exact soft-float
/// helper, so Wasm NaN canonicalisation can never become architectural state.
fn emit_f64_finite(function: &mut Function, local: u32) {
    function.instruction(&Instruction::LocalGet(local));
    function.instruction(&Instruction::I64Const(52));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Const(0x7ff));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(0x7ff));
    function.instruction(&Instruction::I64Ne);
}

fn emit_f64_zero(function: &mut Function, local: u32) {
    function.instruction(&Instruction::LocalGet(local));
    function.instruction(&Instruction::I64Const(1));
    function.instruction(&Instruction::I64Shl);
    function.instruction(&Instruction::I64Eqz);
}

fn emit_f64_normal(function: &mut Function, local: u32) {
    function.instruction(&Instruction::LocalGet(local));
    function.instruction(&Instruction::I64Const(52));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Const(0x7ff));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(0));
    function.instruction(&Instruction::I64GtU);
    function.instruction(&Instruction::LocalGet(local));
    function.instruction(&Instruction::I64Const(52));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Const(0x7ff));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(0x7ff));
    function.instruction(&Instruction::I64LtU);
    function.instruction(&Instruction::I32And);
}

fn emit_exact_fp_helper_call(
    function: &mut Function,
    op: ExactFpOp,
    lhs_local: u32,
    rhs_local: u32,
    third_local: u32,
    rm_local: u32,
    fcsr_addr: u32,
    helpers: HelperImports,
) -> Result<(), EmitError> {
    function.instruction(&Instruction::I32Const(op.helper_code()));
    for local in [lhs_local, rhs_local, third_local] {
        function.instruction(&Instruction::LocalGet(local));
    }
    function.instruction(&Instruction::LocalGet(rm_local));
    function.instruction(&Instruction::I32Const(fcsr_addr as i32));
    function.instruction(&Instruction::Call(
        helpers
            .fp_index()
            .ok_or_else(|| EmitError("missing exact-FP helper import".into()))?,
    ));
    Ok(())
}

/// Emit one exact FP result, inlining the subset that is already proven safe
/// by the runtime's randomized soft-float differential:
///
/// * round-to-nearest/even;
/// * NX is already sticky, so losing another NX event is unobservable;
/// * finite operands; and
/// * a result for which no flag other than NX can arise.
///
/// Everything else takes the existing Wasm-to-Wasm exact helper.  Keeping the
/// predicate in generated code removes a cross-instance call from ordinary
/// libm arithmetic while retaining bit/flag exactness at every boundary.
fn emit_exact_fp_value(
    function: &mut Function,
    op: ExactFpOp,
    lhs_local: u32,
    rhs_local: u32,
    third_local: u32,
    rm_local: u32,
    fcsr_local: u32,
    output_local: u32,
    layout: JitLayout,
    helpers: HelperImports,
) -> Result<(), EmitError> {
    // ReloadFcsr follows every ExactFp node.  Publish forwarded state even on
    // the inline arm so that load observes exactly the same value it did when
    // every operation unconditionally called the helper.
    function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
    function.instruction(&Instruction::LocalGet(fcsr_local));
    function.instruction(&Instruction::I32Store(memarg(2, 0)));

    if matches!(op, ExactFpOp::Eq64 | ExactFpOp::Lt64 | ExactFpOp::Le64) {
        // Ordered comparisons of finite operands are exact and cannot accrue
        // flags. NaNs retain the helper's quiet/signalling distinction.
        emit_f64_finite(function, lhs_local);
        emit_f64_finite(function, rhs_local);
        function.instruction(&Instruction::I32And);
        function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
        function.instruction(&Instruction::LocalGet(lhs_local));
        function.instruction(&Instruction::F64ReinterpretI64);
        function.instruction(&Instruction::LocalGet(rhs_local));
        function.instruction(&Instruction::F64ReinterpretI64);
        function.instruction(&match op {
            ExactFpOp::Eq64 => Instruction::F64Eq,
            ExactFpOp::Lt64 => Instruction::F64Lt,
            ExactFpOp::Le64 => Instruction::F64Le,
            _ => unreachable!(),
        });
        function.instruction(&Instruction::I64ExtendI32U);
        function.instruction(&Instruction::Else);
        emit_exact_fp_helper_call(
            function,
            op,
            lhs_local,
            rhs_local,
            third_local,
            rm_local,
            layout.fcsr_addr,
            helpers,
        )?;
        function.instruction(&Instruction::End);
        return Ok(());
    }

    let inline_fma = op == ExactFpOp::Fma64 && crate::hardware_fma_enabled();
    if !matches!(
        op,
        ExactFpOp::Add64 | ExactFpOp::Sub64 | ExactFpOp::Mul64 | ExactFpOp::Div64
    ) && !inline_fma
    {
        return emit_exact_fp_helper_call(
            function,
            op,
            lhs_local,
            rhs_local,
            third_local,
            rm_local,
            layout.fcsr_addr,
            helpers,
        );
    }

    // rm == RNE && (fcsr & NX) != 0 && finite arithmetic operands.
    function.instruction(&Instruction::LocalGet(rm_local));
    function.instruction(&Instruction::I32Eqz);
    function.instruction(&Instruction::LocalGet(fcsr_local));
    function.instruction(&Instruction::I32Const(1)); // FFLAG_INEXACT
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::I32Eqz);
    function.instruction(&Instruction::I32Eqz);
    function.instruction(&Instruction::I32And);
    emit_f64_finite(function, lhs_local);
    function.instruction(&Instruction::I32And);
    emit_f64_finite(function, rhs_local);
    function.instruction(&Instruction::I32And);
    if inline_fma {
        emit_f64_finite(function, third_local);
        function.instruction(&Instruction::I32And);
    }
    function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));

    if inline_fma {
        for local in [lhs_local, rhs_local, third_local] {
            function.instruction(&Instruction::LocalGet(local));
            function.instruction(&Instruction::F64ReinterpretI64);
            function.instruction(&Instruction::F64x2Splat);
        }
        function.instruction(&Instruction::F64x2RelaxedMadd);
        function.instruction(&Instruction::F64x2ExtractLane(0));
    } else {
        function.instruction(&Instruction::LocalGet(lhs_local));
        function.instruction(&Instruction::F64ReinterpretI64);
        function.instruction(&Instruction::LocalGet(rhs_local));
        function.instruction(&Instruction::F64ReinterpretI64);
        function.instruction(&match op {
            ExactFpOp::Add64 => Instruction::F64Add,
            ExactFpOp::Sub64 => Instruction::F64Sub,
            ExactFpOp::Mul64 => Instruction::F64Mul,
            ExactFpOp::Div64 => Instruction::F64Div,
            _ => unreachable!(),
        });
    }
    function.instruction(&Instruction::I64ReinterpretF64);
    function.instruction(&Instruction::LocalSet(output_local));

    match op {
        // Add/sub cannot produce an inexact subnormal result.  Any finite
        // result therefore has no newly observable flag beyond sticky NX.
        ExactFpOp::Add64 | ExactFpOp::Sub64 => {
            function.instruction(&Instruction::LocalGet(output_local));
            function.instruction(&Instruction::I64Const(52));
            function.instruction(&Instruction::I64ShrU);
            function.instruction(&Instruction::I64Const(0x7ff));
            function.instruction(&Instruction::I64And);
            function.instruction(&Instruction::I64Const(0x7ff));
            function.instruction(&Instruction::I64Ne);
        }
        // A normal result is safe.  Exact zero is additionally safe when
        // multiplication was forced by a zero operand, or division had a
        // zero numerator; other zero/subnormal results may carry UF.
        ExactFpOp::Mul64 | ExactFpOp::Div64 => {
            emit_f64_normal(function, output_local);
            emit_f64_zero(function, output_local);
            emit_f64_zero(function, lhs_local);
            if op == ExactFpOp::Mul64 {
                emit_f64_zero(function, rhs_local);
                function.instruction(&Instruction::I32Or);
            }
            function.instruction(&Instruction::I32And);
            function.instruction(&Instruction::I32Or);
        }
        // For finite fused inputs, a normal result cannot accrue any flag
        // beyond NX. Zero/subnormal/overflow results conservatively fall back.
        ExactFpOp::Fma64 => emit_f64_normal(function, output_local),
        _ => unreachable!(),
    }
    function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
    function.instruction(&Instruction::LocalGet(output_local));
    function.instruction(&Instruction::Else);
    emit_exact_fp_helper_call(
        function,
        op,
        lhs_local,
        rhs_local,
        third_local,
        rm_local,
        layout.fcsr_addr,
        helpers,
    )?;
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::Else);
    emit_exact_fp_helper_call(
        function,
        op,
        lhs_local,
        rhs_local,
        third_local,
        rm_local,
        layout.fcsr_addr,
        helpers,
    )?;
    function.instruction(&Instruction::End);
    Ok(())
}

/// Leave one validated memory32 linear address on the Wasm stack. Flat-user
/// memory uses an unsigned length check. Full-system memory requires a
/// same-page fused-row hit; a typed refill may publish the row on a miss, but
/// the generated code always re-probes it and never trusts a helper sentinel.
fn emit_memory_address(
    function: &mut Function,
    layout: JitLayout,
    helpers: HelperImports,
    temps: Option<MemoryTemps>,
    address_local: u32,
    access_bytes: u64,
    store: bool,
    mut side_exit: impl FnMut(&mut Function) -> Result<(), EmitError>,
) -> Result<(), EmitError> {
    if let Some((memory_base, memory_len)) = layout.mem {
        if let Some(last_valid) = memory_len.checked_sub(access_bytes) {
            function.instruction(&Instruction::LocalGet(address_local));
            function.instruction(&Instruction::I64Const(last_valid as i64));
            function.instruction(&Instruction::I64GtU);
        } else {
            function.instruction(&Instruction::I32Const(1));
        }
        function.instruction(&Instruction::If(BlockType::Empty));
        side_exit(function)?;
        function.instruction(&Instruction::Return);
        function.instruction(&Instruction::End);
        emit_linear_address(function, memory_base, address_local);
        return Ok(());
    }

    let memory = layout.sys.ok_or_else(|| {
        EmitError("memory operation requires a flat or full-system capability".into())
    })?;
    validate_system_memory(memory)?;
    let temps = temps.ok_or_else(|| EmitError("missing full-system memory temporaries".into()))?;
    let page_bytes = 1u64 << memory.page_shift;
    let last_same_page = page_bytes.checked_sub(access_bytes).ok_or_else(|| {
        EmitError("memory access is wider than the configured system page".into())
    })?;

    // Cross-page accesses require two independently faultable translations.
    // T0 already implements those bytewise semantics and remains the precise
    // slow path until a typed split-access IR effect is introduced.
    function.instruction(&Instruction::LocalGet(address_local));
    function.instruction(&Instruction::I64Const((page_bytes - 1) as i64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(last_same_page as i64));
    function.instruction(&Instruction::I64GtU);
    function.instruction(&Instruction::If(BlockType::Empty));
    side_exit(function)?;
    function.instruction(&Instruction::Return);
    function.instruction(&Instruction::End);

    let row = if store { memory.store } else { memory.load };
    let cache = if store {
        temps.store_cache
    } else {
        temps.load_cache
    };
    if let (Some(cache), Some(page_local)) = (cache, temps.page) {
        function.instruction(&Instruction::LocalGet(address_local));
        function.instruction(&Instruction::I64Const(i64::from(memory.page_shift)));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::LocalSet(page_local));

        function.instruction(&Instruction::LocalGet(page_local));
        function.instruction(&Instruction::LocalGet(cache.page));
        function.instruction(&Instruction::I64Eq);
        function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
        function.instruction(&Instruction::LocalGet(cache.offset));
        function.instruction(&Instruction::Else);
        emit_system_translation_offset(
            function,
            memory,
            row,
            temps,
            helpers,
            address_local,
            store,
            &mut side_exit,
        )?;
        function.instruction(&Instruction::LocalSet(temps.offset));
        function.instruction(&Instruction::LocalGet(page_local));
        function.instruction(&Instruction::LocalSet(cache.page));
        function.instruction(&Instruction::LocalGet(temps.offset));
        function.instruction(&Instruction::LocalSet(cache.offset));
        function.instruction(&Instruction::LocalGet(temps.offset));
        function.instruction(&Instruction::End);
    } else {
        // `temps.page` is a scratch local initialized only by the cached path
        // above. Selective caching can leave (for example) a store cache live
        // while a singleton load deliberately bypasses caching; that load
        // must derive its page from its own address rather than consume the
        // store path's stale scratch value.
        let mut uncached_temps = temps;
        uncached_temps.page = None;
        emit_system_translation_offset(
            function,
            memory,
            row,
            uncached_temps,
            helpers,
            address_local,
            store,
            &mut side_exit,
        )?;
    }
    function.instruction(&Instruction::LocalSet(temps.offset));

    function.instruction(&Instruction::LocalGet(address_local));
    function.instruction(&Instruction::LocalGet(temps.offset));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::I32WrapI64);
    Ok(())
}

/// Leave one proven signed linear-memory offset on the stack. A refill helper
/// may mutate the direct-mapped architectural row, so the row is always
/// re-probed before its offset is consumed. Invocation-local translation
/// caching happens outside this primitive only after that proof succeeds.
#[allow(clippy::too_many_arguments)]
fn emit_system_translation_offset(
    function: &mut Function,
    memory: SystemMemory,
    row: TranslationRow,
    temps: MemoryTemps,
    helpers: HelperImports,
    address_local: u32,
    store: bool,
    side_exit: &mut impl FnMut(&mut Function) -> Result<(), EmitError>,
) -> Result<(), EmitError> {
    emit_translation_index(function, memory, temps, address_local);

    emit_translation_probe(function, row, temps, address_local, memory.page_shift);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
    emit_translation_offset(function, row, temps);
    function.instruction(&Instruction::Else);
    if memory.miss == TlbMissPolicy::Refill {
        function.instruction(&Instruction::LocalGet(address_local));
        function.instruction(&Instruction::I32Const(i32::from(store)));
        function.instruction(&Instruction::Call(
            helpers
                .tlb_fill_index()
                .ok_or_else(|| EmitError("missing full-system TLB refill import".into()))?,
        ));
        function.instruction(&Instruction::Drop);

        emit_translation_probe(function, row, temps, address_local, memory.page_shift);
        function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
        emit_translation_offset(function, row, temps);
        function.instruction(&Instruction::Else);
        side_exit(function)?;
        function.instruction(&Instruction::Return);
        function.instruction(&Instruction::End);
    } else {
        side_exit(function)?;
        function.instruction(&Instruction::Return);
    }
    function.instruction(&Instruction::End);
    Ok(())
}

fn emit_translation_index(
    function: &mut Function,
    memory: SystemMemory,
    temps: MemoryTemps,
    address_local: u32,
) {
    if let Some(page_local) = temps.page {
        function.instruction(&Instruction::LocalGet(page_local));
    } else {
        function.instruction(&Instruction::LocalGet(address_local));
        function.instruction(&Instruction::I64Const(i64::from(memory.page_shift)));
        function.instruction(&Instruction::I64ShrU);
    }
    if memory.index_hash_shift != 0 {
        // Preserve the page in the otherwise-free offset temporary while
        // folding its upper VPN bits into the direct-map index. Tags still
        // carry the complete VA/context proof, so hashing changes collision
        // behavior only—not hit validity.
        function.instruction(&Instruction::LocalTee(temps.offset));
        function.instruction(&Instruction::LocalGet(temps.offset));
        function.instruction(&Instruction::I64Const(i64::from(memory.index_hash_shift)));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Xor);
    }
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::I32Const(memory.index_mask as i32));
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::LocalSet(temps.index));
}

fn emit_side_exit(
    function: &mut Function,
    region: &Region,
    layout: JitLayout,
    local_map: &[Option<u32>],
    defined: &[bool],
    exit: &SideExit,
) -> Result<(), EmitError> {
    emit_side_exit_state(function, region, layout, local_map, defined, exit)?;
    emit_retirement_const(function, layout.retired_addr, exit.retired);
    Ok(())
}

/// Publish every architectural value needed by a precise side exit, excluding
/// retirement. Vector helpers need this state before they run, but retirement
/// is committed only on a failed helper call; a successful vector instruction
/// remains part of the enclosing region's ordinary retirement total.
fn emit_side_exit_state(
    function: &mut Function,
    region: &Region,
    layout: JitLayout,
    local_map: &[Option<u32>],
    defined: &[bool],
    exit: &SideExit,
) -> Result<(), EmitError> {
    emit_commit_outputs(function, layout, &exit.outputs, |function, value| {
        emit_value(function, region, layout, local_map, defined, value)
    })?;
    emit_commit_f_outputs(function, layout, &exit.f_outputs, |function, value| {
        emit_value(function, region, layout, local_map, defined, value)
    })?;
    if let Some(value) = exit.fcsr_output {
        function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
        emit_value(function, region, layout, local_map, defined, value)?;
        function.instruction(&Instruction::I32Store(memarg(2, 0)));
    }
    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
    emit_guest_pc(function, exit.guest_pc, layout);
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
    Ok(())
}

fn emit_vector_call(
    function: &mut Function,
    helpers: HelperImports,
    insn: u32,
    prepare_fallback: &mut dyn FnMut(&mut Function) -> Result<(), EmitError>,
) -> Result<(), EmitError> {
    prepare_fallback(function)?;
    function.instruction(&Instruction::LocalGet(0));
    function.instruction(&Instruction::I32Const(insn as i32));
    function.instruction(&Instruction::Call(
        helpers
            .vector_index()
            .ok_or_else(|| EmitError("missing vector helper import".into()))?,
    ));
    Ok(())
}

fn vector_direct_available(layout: JitLayout, direct: Option<VectorDirect>) -> bool {
    layout.vector.is_some()
        && layout.vector_state.is_some()
        && match direct {
            Some(VectorDirect::ConfigImmediate { .. } | VectorDirect::ConfigRetainFull { .. }) => {
                true
            }
            Some(
                VectorDirect::Lane { .. }
                | VectorDirect::SlideImmediate { up: false, .. }
                | VectorDirect::SlideOne { up: false, .. }
                | VectorDirect::Index { .. }
                | VectorDirect::Reduction { .. }
                | VectorDirect::MaskLogic { .. }
                | VectorDirect::FloatSign { .. }
                | VectorDirect::FloatBroadcast { .. }
                | VectorDirect::FloatScalarInsert { .. }
                | VectorDirect::FloatScalarExtract { .. }
                | VectorDirect::FloatSlideOne { up: false, .. }
                | VectorDirect::ScalarExtract { .. }
                | VectorDirect::ScalarInsert { .. },
            ) => true,
            Some(VectorDirect::WidenAddSub {
                wide_left,
                destination,
                source2,
                operand,
                ..
            }) => match operand {
                VectorOperand::Vector(source1) => {
                    destination != source1
                        && source2 != source1
                        && (wide_left || destination != source2)
                }
                VectorOperand::ScalarX(_) => wide_left || destination != source2,
                VectorOperand::Immediate(_) => false,
            },
            Some(VectorDirect::WidenMultiplyAccumulate {
                destination,
                source2,
                operand,
                ..
            }) => {
                destination != source2
                    && !matches!(operand, VectorOperand::Vector(source1) if destination == source1)
                    && !matches!(operand, VectorOperand::Immediate(_))
            }
            Some(
                VectorDirect::SlideImmediate {
                    up: true,
                    destination,
                    source,
                    ..
                }
                | VectorDirect::SlideOne {
                    up: true,
                    destination,
                    source,
                    ..
                }
                | VectorDirect::FloatSlideOne {
                    up: true,
                    destination,
                    source,
                    ..
                },
            ) => destination != source,
            Some(VectorDirect::GatherImmediate {
                destination,
                source,
                ..
            }) => destination != source,
            Some(VectorDirect::GatherVector {
                destination,
                source,
                indices,
            }) => destination != source && destination != indices,
            Some(VectorDirect::Compare {
                destination,
                source2,
                operand,
                ..
            }) => {
                destination != source2
                    && !matches!(operand, VectorOperand::Vector(source) if destination == source)
            }
            Some(VectorDirect::WholeRegisterMove {
                destination,
                source,
                registers,
            }) => {
                destination % registers == 0
                    && source % registers == 0
                    && destination.saturating_add(registers) <= 32
                    && source.saturating_add(registers) <= 32
            }
            Some(VectorDirect::WholeRegisterMemory {
                register,
                registers,
                ..
            }) if register % registers != 0 || register.saturating_add(registers) > 32 => false,
            Some(
                VectorDirect::UnitStride { .. }
                | VectorDirect::Strided { .. }
                | VectorDirect::WholeRegisterMemory { .. },
            ) => match layout.vector {
                Some(VectorCapability::User) => layout.mem.is_some(),
                Some(VectorCapability::System) => layout.sys.is_some(),
                None => false,
            },
            None => false,
        }
        && (layout.vector != Some(VectorCapability::System) || layout.mstatus_addr != 0)
}

fn vector_partial_vl_available(direct: VectorDirect) -> bool {
    matches!(
        direct,
        VectorDirect::Lane { masked: false, .. }
            | VectorDirect::SlideImmediate { .. }
            | VectorDirect::GatherImmediate { .. }
            | VectorDirect::GatherVector { .. }
            | VectorDirect::Index { .. }
    )
}

fn vector_config_instruction(insn: u32) -> bool {
    insn & 0x7f == 0x57 && (insn >> 12) & 7 == 7
}

/// Resolve the ratified `vsetvli x0,x0,vtype` retain-vl form from a preceding
/// exact configuration. The instruction is legal only when VLMAX is
/// unchanged; in that case both the retained vl and the new vtype are exact.
fn known_vector_retaining_config_transition(
    previous: KnownVectorConfig,
    insn: u32,
) -> Option<KnownVectorConfig> {
    if !vector_config_instruction(insn)
        || insn >> 31 != 0
        || (insn >> 7) & 0x1f != 0
        || (insn >> 15) & 0x1f != 0
    {
        return None;
    }
    let vtype = u64::from((insn >> 20) & 0x7ff);
    let next = KnownVectorConfig::decode(vtype, previous.vl)?;
    (next.vlmax == previous.vlmax).then_some(next)
}

fn known_vector_register_aligned(register: u8, span: u8) -> bool {
    register & (span - 1) == 0
}

/// Return whether a statically configured direct instruction needs the
/// partial-vl tail merge. None means the existing guarded/helper lowering is
/// still required. This mirrors `emit_vector_direct_guard` in ordinary Rust
/// predicates so the generated hot path contains no repeated state decoder.
fn known_vector_direct_tail_merge(config: KnownVectorConfig, direct: VectorDirect) -> Option<bool> {
    let full_vl = config.vl == config.vlmax;
    if !full_vl && (config.vl == 0 || config.span != 1 || !vector_partial_vl_available(direct)) {
        return None;
    }
    let aligned = |register| known_vector_register_aligned(register, config.span);
    let supported = match direct {
        VectorDirect::Lane {
            op,
            masked,
            destination,
            source2,
            operand,
        } => {
            aligned(destination)
                && source2.is_none_or(aligned)
                && !matches!(operand, VectorOperand::Vector(register) if !aligned(register))
                && !(op == VectorLaneOp::Multiply && config.vsew == 0)
                && !(matches!(
                    op,
                    VectorLaneOp::MinUnsigned
                        | VectorLaneOp::MinSigned
                        | VectorLaneOp::MaxUnsigned
                        | VectorLaneOp::MaxSigned
                ) && config.vsew == 3)
                && (!masked || (full_vl && destination != 0))
        }
        VectorDirect::SlideImmediate {
            destination,
            source,
            ..
        }
        | VectorDirect::GatherImmediate {
            destination,
            source,
            ..
        } => config.span == 1 && aligned(destination) && aligned(source),
        VectorDirect::SlideOne {
            destination,
            source,
            ..
        } => full_vl && config.span == 1 && aligned(destination) && aligned(source),
        VectorDirect::GatherVector {
            destination,
            source,
            indices,
        } => config.span == 1 && aligned(destination) && aligned(source) && aligned(indices),
        VectorDirect::Index { destination } => {
            aligned(destination) && (full_vl || config.span == 1)
        }
        VectorDirect::Reduction { source, .. } => full_vl && aligned(source),
        VectorDirect::MaskLogic { .. } => full_vl,
        VectorDirect::WidenAddSub { .. } | VectorDirect::WidenMultiplyAccumulate { .. } => {
            full_vl && config.fractional_lmul && config.vsew < 3
        }
        VectorDirect::Compare {
            op,
            destination,
            source2,
            operand,
            ..
        } => {
            let disjoint_from_mask = |source: u8| {
                destination < source || destination >= source.saturating_add(config.span)
            };
            full_vl
                && aligned(source2)
                && disjoint_from_mask(source2)
                && !matches!(operand, VectorOperand::Vector(register) if !aligned(register))
                && !matches!(operand, VectorOperand::Vector(register) if !disjoint_from_mask(register))
                && !(config.vsew == 3
                    && matches!(
                        op,
                        VectorCompareOp::LessUnsigned
                            | VectorCompareOp::LessEqualUnsigned
                            | VectorCompareOp::GreaterUnsigned
                    ))
        }
        VectorDirect::UnitStride {
            masked,
            width,
            register,
            ..
        }
        | VectorDirect::Strided {
            masked,
            width,
            register,
            ..
        } => {
            let memory = config.memory_config(width)?;
            full_vl
                && known_vector_register_aligned(register, memory.span)
                && register.saturating_add(memory.span) <= 32
                // v0 supplies the predicate throughout a masked transfer.
                && (!masked || register != 0)
        }
        VectorDirect::ConfigImmediate { .. }
        | VectorDirect::ConfigRetainFull { .. }
        | VectorDirect::WholeRegisterMemory { .. }
        | VectorDirect::WholeRegisterMove { .. }
        | VectorDirect::ScalarExtract { .. }
        | VectorDirect::ScalarInsert { .. }
        | VectorDirect::FloatSign { .. }
        | VectorDirect::FloatBroadcast { .. }
        | VectorDirect::FloatScalarInsert { .. }
        | VectorDirect::FloatScalarExtract { .. }
        | VectorDirect::FloatSlideOne { .. } => false,
    };
    supported.then_some(!full_vl)
}

/// Integer RVV lowering has an exact, statically known scalar-state footprint.
/// Cached emitters can therefore publish only its scalar inputs on the hot
/// direct arm and defer the complete precise snapshot to the cold helper arm.
fn vector_cached_direct_deferable(direct: Option<VectorDirect>) -> bool {
    direct.is_some_and(|direct| {
        !matches!(
            direct,
            VectorDirect::ConfigImmediate {
                destination: 1..,
                ..
            } | VectorDirect::ScalarExtract { .. }
                | VectorDirect::FloatSign { .. }
                | VectorDirect::FloatBroadcast { .. }
                | VectorDirect::FloatScalarInsert { .. }
                | VectorDirect::FloatScalarExtract { .. }
                | VectorDirect::FloatSlideOne { .. }
        )
    })
}

fn vector_direct_x_inputs(direct: VectorDirect) -> [Option<u8>; 2] {
    match direct {
        VectorDirect::Lane {
            operand: VectorOperand::ScalarX(register),
            ..
        }
        | VectorDirect::ScalarInsert {
            source: register, ..
        }
        | VectorDirect::SlideOne {
            scalar: register, ..
        }
        | VectorDirect::WidenAddSub {
            operand: VectorOperand::ScalarX(register),
            ..
        }
        | VectorDirect::WidenMultiplyAccumulate {
            operand: VectorOperand::ScalarX(register),
            ..
        }
        | VectorDirect::Compare {
            operand: VectorOperand::ScalarX(register),
            ..
        } => [Some(register), None],
        VectorDirect::UnitStride { base, .. } | VectorDirect::WholeRegisterMemory { base, .. } => {
            [Some(base), None]
        }
        VectorDirect::Strided { base, stride, .. } => [Some(base), Some(stride)],
        _ => [None, None],
    }
}

fn emit_vector_group_alignment(function: &mut Function, register: u8, temps: VectorTemps) {
    function.instruction(&Instruction::I32Const(i32::from(register)));
    function.instruction(&Instruction::LocalGet(temps.span));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Sub);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::I32Eqz);
    function.instruction(&Instruction::I32And);
}

fn emit_vector_memory_guard(
    function: &mut Function,
    layout: JitLayout,
    load: bool,
    width: Option<u8>,
    register: u8,
    base: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
    constant_bytes: Option<u32>,
    address_preloaded: bool,
    cache_load_translation: bool,
) {
    debug_assert!(!cache_load_translation || load);
    if let Some(width) = width {
        emit_vector_group_alignment(function, register, temps);

        let vsew = match width {
            8 => 0,
            16 => 1,
            32 => 2,
            64 => 3,
            _ => unreachable!("decoded unit-stride width"),
        };
        function.instruction(&Instruction::LocalGet(temps.vtype));
        function.instruction(&Instruction::I64Const(3));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Const(7));
        function.instruction(&Instruction::I64And);
        function.instruction(&Instruction::I64Const(vsew));
        function.instruction(&Instruction::I64Eq);
        function.instruction(&Instruction::I32And);
    }

    if !address_preloaded {
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        function.instruction(&Instruction::I64Load(memarg(3, u64::from(base) * 8)));
        function.instruction(&Instruction::LocalSet(temps.address));
    }

    if let Some((memory_base, memory_len)) = layout.mem {
        // With EEW==SEW and vl==VLMAX, group_bytes is the exact active memory
        // extent for both integer and fractional LMUL. Prove the complete
        // range before any access so no partial architectural fault is hidden.
        function.instruction(&Instruction::I64Const(memory_len as i64));
        emit_vector_memory_bytes_i64(function, temps, fractional_lmul, constant_bytes);
        function.instruction(&Instruction::I64GeU);
        function.instruction(&Instruction::I32And);

        function.instruction(&Instruction::LocalGet(temps.address));
        function.instruction(&Instruction::I64Const(memory_len as i64));
        emit_vector_memory_bytes_i64(function, temps, fractional_lmul, constant_bytes);
        function.instruction(&Instruction::I64Sub);
        function.instruction(&Instruction::I64LeU);
        function.instruction(&Instruction::I32And);

        function.instruction(&Instruction::I32Const(memory_base as i32));
        function.instruction(&Instruction::LocalGet(temps.address));
        function.instruction(&Instruction::I32WrapI64);
        function.instruction(&Instruction::I32Add);
        function.instruction(&Instruction::LocalSet(temps.linear));
        return;
    }

    let memory = layout.sys.expect("checked system vector memory capability");
    let page_bytes = 1u64 << memory.page_shift;
    let page_mask = !(page_bytes - 1);

    // A single fused row proves permission and ordinary RAM only within one
    // page. Cross-page, missing-row, MMIO, and generated-code stores retain
    // the helper, which owns exact vstart and dirty-code status semantics.
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I64Const((page_bytes - 1) as i64));
    function.instruction(&Instruction::I64And);
    if let Some(bytes) = constant_bytes {
        if let Some(last_start) = page_bytes.checked_sub(u64::from(bytes)) {
            function.instruction(&Instruction::I64Const(last_start as i64));
            function.instruction(&Instruction::I64LeU);
        } else {
            function.instruction(&Instruction::Drop);
            function.instruction(&Instruction::I32Const(0));
        }
    } else {
        emit_vector_memory_bytes_i64(function, temps, fractional_lmul, None);
        function.instruction(&Instruction::I64Add);
        function.instruction(&Instruction::I64Const(page_bytes as i64));
        function.instruction(&Instruction::I64LeU);
    }
    function.instruction(&Instruction::I32And);

    if cache_load_translation {
        // A load translation remains valid for the lifetime of this generated
        // invocation: mapping/context changes exit generated code, and code
        // installation invalidates only store capabilities. Cache the proven
        // virtual page and offset in otherwise-unused unit-stride scratches.
        function.instruction(&Instruction::LocalGet(temps.address));
        function.instruction(&Instruction::I64Const(i64::from(memory.page_shift)));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::LocalSet(temps.element));

        function.instruction(&Instruction::LocalGet(temps.element));
        function.instruction(&Instruction::LocalGet(temps.last));
        function.instruction(&Instruction::I64Eq);
        function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        function.instruction(&Instruction::LocalGet(temps.address));
        function.instruction(&Instruction::LocalGet(temps.stride));
        function.instruction(&Instruction::I64Add);
        function.instruction(&Instruction::I32WrapI64);
        function.instruction(&Instruction::LocalSet(temps.linear));
        function.instruction(&Instruction::I32Const(1));
        function.instruction(&Instruction::Else);

        // Invalidate before replacing the cached offset. A failed row probe
        // must never leave the preceding page paired with the new row's
        // unrelated offset.
        function.instruction(&Instruction::I64Const(-1));
        function.instruction(&Instruction::LocalSet(temps.last));

        function.instruction(&Instruction::LocalGet(temps.element));
        if memory.index_hash_shift != 0 {
            function.instruction(&Instruction::LocalGet(temps.element));
            function.instruction(&Instruction::I64Const(i64::from(memory.index_hash_shift)));
            function.instruction(&Instruction::I64ShrU);
            function.instruction(&Instruction::I64Xor);
        }
        function.instruction(&Instruction::I32WrapI64);
        function.instruction(&Instruction::I32Const(memory.index_mask as i32));
        function.instruction(&Instruction::I32And);
        function.instruction(&Instruction::LocalSet(temps.index));

        let row = memory.load;
        function.instruction(&Instruction::I32Const(row.tags as i32));
        function.instruction(&Instruction::LocalGet(temps.index));
        function.instruction(&Instruction::I32Const(3));
        function.instruction(&Instruction::I32Shl);
        function.instruction(&Instruction::I32Add);
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::LocalGet(temps.address));
        function.instruction(&Instruction::I64Const(page_mask as i64));
        function.instruction(&Instruction::I64And);
        if let Some(context) = temps.context {
            function.instruction(&Instruction::LocalGet(context));
        } else {
            function.instruction(&Instruction::I32Const(memory.context_addr as i32));
            function.instruction(&Instruction::I64Load(memarg(3, 0)));
        }
        function.instruction(&Instruction::I64Or);
        function.instruction(&Instruction::I64Eq);
        function.instruction(&Instruction::LocalSet(temps.chunk));

        function.instruction(&Instruction::I32Const(row.offsets as i32));
        function.instruction(&Instruction::LocalGet(temps.index));
        function.instruction(&Instruction::I32Const(3));
        function.instruction(&Instruction::I32Shl);
        function.instruction(&Instruction::I32Add);
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::LocalSet(temps.stride));
        function.instruction(&Instruction::LocalGet(temps.chunk));
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::LocalGet(temps.element));
        function.instruction(&Instruction::LocalSet(temps.last));
        function.instruction(&Instruction::End);
        function.instruction(&Instruction::LocalGet(temps.address));
        function.instruction(&Instruction::LocalGet(temps.stride));
        function.instruction(&Instruction::I64Add);
        function.instruction(&Instruction::I32WrapI64);
        function.instruction(&Instruction::LocalSet(temps.linear));
        function.instruction(&Instruction::LocalGet(temps.chunk));
        function.instruction(&Instruction::End);
        function.instruction(&Instruction::I32And);
        return;
    }

    // Direct-map index from the complete virtual page, including the optional
    // architectural high-VPN hash used by the production fused JTLB.
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I64Const(i64::from(memory.page_shift)));
    function.instruction(&Instruction::I64ShrU);
    if memory.index_hash_shift != 0 {
        function.instruction(&Instruction::LocalGet(temps.address));
        function.instruction(&Instruction::I64Const(i64::from(memory.page_shift)));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Const(i64::from(memory.index_hash_shift)));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Xor);
    }
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::I32Const(memory.index_mask as i32));
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::LocalSet(temps.index));

    let row = if load { memory.load } else { memory.store };
    function.instruction(&Instruction::I32Const(row.tags as i32));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(3));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I64Const(page_mask as i64));
    function.instruction(&Instruction::I64And);
    if let Some(context) = temps.context {
        function.instruction(&Instruction::LocalGet(context));
    } else {
        function.instruction(&Instruction::I32Const(memory.context_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
    }
    function.instruction(&Instruction::I64Or);
    function.instruction(&Instruction::I64Eq);
    function.instruction(&Instruction::I32And);

    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I32Const(row.offsets as i32));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(3));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::LocalSet(temps.linear));
}

#[allow(clippy::too_many_arguments)]
fn emit_vector_strided_memory_guard(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    load: bool,
    width: u8,
    register: u8,
    base: u8,
    stride: u8,
    temps: VectorTemps,
    dynamic_config_checks: bool,
) {
    if dynamic_config_checks {
        emit_vector_group_alignment(function, register, temps);
        let vsew = match width {
            8 => 0,
            16 => 1,
            32 => 2,
            64 => 3,
            _ => unreachable!("decoded strided width"),
        };
        function.instruction(&Instruction::LocalGet(temps.vtype));
        function.instruction(&Instruction::I64Const(3));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Const(7));
        function.instruction(&Instruction::I64And);
        function.instruction(&Instruction::I64Const(vsew));
        function.instruction(&Instruction::I64Eq);
        function.instruction(&Instruction::I32And);
    }

    function.instruction(&Instruction::I32Const(layout.x_base as i32));
    function.instruction(&Instruction::I64Load(memarg(3, u64::from(base) * 8)));
    function.instruction(&Instruction::LocalSet(temps.address));
    function.instruction(&Instruction::I32Const(layout.x_base as i32));
    function.instruction(&Instruction::I64Load(memarg(3, u64::from(stride) * 8)));
    function.instruction(&Instruction::LocalTee(temps.stride));
    function.instruction(&Instruction::I64Const(0));
    function.instruction(&Instruction::I64GeS);
    function.instruction(&Instruction::I32And);

    // Keep stride * (vl - 1) below one unsigned wrap. The later last>=base
    // test then proves that adding the bounded product to the base did not
    // wrap either. Larger valid strides retain the precise helper path.
    let stride_bound = if let Some((_, memory_len)) = layout.mem {
        memory_len.min(u64::MAX / 128)
    } else {
        let page_shift = layout
            .sys
            .expect("checked system vector memory capability")
            .page_shift;
        (1u64 << page_shift).min(u64::MAX / 128)
    };
    function.instruction(&Instruction::LocalGet(temps.stride));
    function.instruction(&Instruction::I64Const(stride_bound as i64));
    function.instruction(&Instruction::I64LeU);
    function.instruction(&Instruction::I32And);

    // Prove the unsigned last-element address did not wrap. The direct path
    // currently covers non-negative strides; negative and wrapping walks keep
    // the helper's element-by-element fault semantics.
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::LocalGet(temps.stride));
    function.instruction(&Instruction::I32Const(state.vl_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Const(1));
    function.instruction(&Instruction::I64Sub);
    function.instruction(&Instruction::I64Mul);
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::LocalTee(temps.last));
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I64GeU);
    function.instruction(&Instruction::I32And);

    let element_bytes = u64::from(width / 8);
    if let Some((memory_base, memory_len)) = layout.mem {
        let Some(limit) = (memory_len as u64).checked_sub(element_bytes) else {
            function.instruction(&Instruction::I32Const(0));
            function.instruction(&Instruction::I32And);
            return;
        };
        function.instruction(&Instruction::LocalGet(temps.address));
        function.instruction(&Instruction::I64Const(limit as i64));
        function.instruction(&Instruction::I64LeU);
        function.instruction(&Instruction::I32And);
        function.instruction(&Instruction::LocalGet(temps.last));
        function.instruction(&Instruction::I64Const(limit as i64));
        function.instruction(&Instruction::I64LeU);
        function.instruction(&Instruction::I32And);
        function.instruction(&Instruction::I32Const(memory_base as i32));
        function.instruction(&Instruction::LocalGet(temps.address));
        function.instruction(&Instruction::I32WrapI64);
        function.instruction(&Instruction::I32Add);
        function.instruction(&Instruction::LocalSet(temps.linear));
        function.instruction(&Instruction::I64Const(-1));
        function.instruction(&Instruction::LocalSet(temps.last));
        return;
    }

    let memory = layout.sys.expect("checked system vector memory capability");
    let page_bytes = 1u64 << memory.page_shift;
    let page_mask = !(page_bytes - 1);
    // At most two consecutive pages are handled directly. Both translations
    // are proved before any element access, preserving all-or-helper fault
    // semantics while covering common long positive-stride walks.
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Const(i64::from(memory.page_shift)));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I64Const(i64::from(memory.page_shift)));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Sub);
    function.instruction(&Instruction::I64Const(1));
    function.instruction(&Instruction::I64LeU);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Const((page_bytes - 1) as i64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(element_bytes as i64));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::I64Const(page_bytes as i64));
    function.instruction(&Instruction::I64LeU);
    function.instruction(&Instruction::I32And);

    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I64Const(i64::from(memory.page_shift)));
    function.instruction(&Instruction::I64ShrU);
    if memory.index_hash_shift != 0 {
        function.instruction(&Instruction::LocalGet(temps.address));
        function.instruction(&Instruction::I64Const(i64::from(memory.page_shift)));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Const(i64::from(memory.index_hash_shift)));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Xor);
    }
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::I32Const(memory.index_mask as i32));
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::LocalSet(temps.index));

    let row = if load { memory.load } else { memory.store };
    function.instruction(&Instruction::I32Const(row.tags as i32));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(3));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I64Const(page_mask as i64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I32Const(memory.context_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Or);
    function.instruction(&Instruction::I64Eq);
    function.instruction(&Instruction::I32And);

    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I32Const(row.offsets as i32));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(3));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::LocalSet(temps.linear));

    // Probe the last page as well (the same row when the range stays within
    // one page). Save its translated page base for per-element selection.
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Const(i64::from(memory.page_shift)));
    function.instruction(&Instruction::I64ShrU);
    if memory.index_hash_shift != 0 {
        function.instruction(&Instruction::LocalGet(temps.last));
        function.instruction(&Instruction::I64Const(i64::from(memory.page_shift)));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Const(i64::from(memory.index_hash_shift)));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Xor);
    }
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::I32Const(memory.index_mask as i32));
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::LocalSet(temps.index));

    function.instruction(&Instruction::I32Const(row.tags as i32));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(3));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Const(page_mask as i64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I32Const(memory.context_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Or);
    function.instruction(&Instruction::I64Eq);
    function.instruction(&Instruction::I32And);

    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Const(page_mask as i64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I32Const(row.offsets as i32));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(3));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::LocalSet(temps.linear2));

    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Const(page_mask as i64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::LocalSet(temps.last));
}

/// Leave the conservative direct-SIMD predicate on the stack. Policy bits are
/// irrelevant because this path requires every element in the logical register
/// group to be active and preserves bytes outside a fractional group. Reserved
/// vtype/LMUL, partial vl, restart, masking, and disabled privileged vector
/// state reject the path and retain the architectural helper.
fn emit_vector_direct_state_load(
    function: &mut Function,
    state: VectorStateLayout,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(state.vtype_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalSet(temps.vtype));
}

fn emit_vector_group_bytes_i64(function: &mut Function, temps: VectorTemps, fractional_lmul: bool) {
    if fractional_lmul {
        function.instruction(&Instruction::LocalGet(temps.group_bytes));
        function.instruction(&Instruction::I64ExtendI32U);
    } else {
        function.instruction(&Instruction::I64Const(16));
        function.instruction(&Instruction::LocalGet(temps.span));
        function.instruction(&Instruction::I64ExtendI32U);
        function.instruction(&Instruction::I64Mul);
    }
}

fn emit_vector_memory_bytes_i64(
    function: &mut Function,
    temps: VectorTemps,
    fractional_lmul: bool,
    constant_bytes: Option<u32>,
) {
    if let Some(bytes) = constant_bytes {
        function.instruction(&Instruction::I64Const(i64::from(bytes)));
    } else {
        emit_vector_group_bytes_i64(function, temps, fractional_lmul);
    }
}

fn emit_vector_direct_guard(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    direct: VectorDirect,
    temps: VectorTemps,
    fractional_lmul: bool,
    full_vl: bool,
    address_preloaded: bool,
) {
    debug_assert!(!matches!(direct, VectorDirect::ConfigImmediate { .. }));
    // Decode the architectural group width in bytes. Integer LMUL encodings
    // 0..3 produce 16/32/64/128 bytes; fractional encodings 5..7 produce
    // 2/4/8 bytes. Encoding 4 is reserved and rejected below.
    if fractional_lmul {
        function.instruction(&Instruction::I32Const(1));
        function.instruction(&Instruction::LocalSet(temps.span));
        function.instruction(&Instruction::I32Const(16));
        function.instruction(&Instruction::I32Const(8));
        function.instruction(&Instruction::LocalGet(temps.index));
        function.instruction(&Instruction::I32Sub);
        function.instruction(&Instruction::I32ShrU);
        function.instruction(&Instruction::LocalSet(temps.group_bytes));
    } else {
        function.instruction(&Instruction::I32Const(1));
        function.instruction(&Instruction::LocalGet(temps.vtype));
        function.instruction(&Instruction::I32WrapI64);
        function.instruction(&Instruction::I32Const(7));
        function.instruction(&Instruction::I32And);
        function.instruction(&Instruction::LocalTee(temps.index));
        function.instruction(&Instruction::I32Shl);
        function.instruction(&Instruction::LocalSet(temps.span));
    }

    // No VILL/reserved high bits.
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(!0xffi64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Eqz);

    // Select the requested LMUL class and reject the reserved 100 encoding.
    if fractional_lmul {
        function.instruction(&Instruction::LocalGet(temps.index));
        function.instruction(&Instruction::I32Const(4));
        function.instruction(&Instruction::I32GtU);
        function.instruction(&Instruction::I32And);
    } else {
        function.instruction(&Instruction::LocalGet(temps.index));
        function.instruction(&Instruction::I32Const(3));
        function.instruction(&Instruction::I32LeU);
        function.instruction(&Instruction::I32And);
    }

    // ELEN=64: e8/e16/e32/e64 only.
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(3));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Const(7));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(3));
    function.instruction(&Instruction::I64LeU);
    function.instruction(&Instruction::I32And);

    function.instruction(&Instruction::I32Const(state.vstart_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::I32And);

    // The primary SIMD path requires vl == VLMAX. A secondary, explicitly
    // tail-preserving path may accept vl <= VLMAX for one-register groups.
    function.instruction(&Instruction::I32Const(state.vl_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    emit_vector_group_bytes_i64(function, temps, fractional_lmul);
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(3));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Const(7));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&if full_vl {
        Instruction::I64Eq
    } else {
        Instruction::I64LeU
    });
    function.instruction(&Instruction::I32And);

    match direct {
        VectorDirect::Lane {
            op,
            masked,
            destination,
            source2,
            operand,
        } => {
            function.instruction(&Instruction::I32Const(i32::from(!masked)));
            function.instruction(&Instruction::I32And);
            emit_vector_group_alignment(function, destination, temps);
            if let Some(source2) = source2 {
                emit_vector_group_alignment(function, source2, temps);
            }
            if let VectorOperand::Vector(source1) = operand {
                emit_vector_group_alignment(function, source1, temps);
            }

            // SIMD128 has no byte multiply or 64-bit lane min/max primitive.
            if op == VectorLaneOp::Multiply {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(0x38));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Eqz);
                function.instruction(&Instruction::I32Eqz);
                function.instruction(&Instruction::I32And);
            } else if matches!(
                op,
                VectorLaneOp::MinUnsigned
                    | VectorLaneOp::MinSigned
                    | VectorLaneOp::MaxUnsigned
                    | VectorLaneOp::MaxSigned
            ) {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(0x38));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(0x18));
                function.instruction(&Instruction::I64Ne);
                function.instruction(&Instruction::I32And);
            }
        }
        VectorDirect::UnitStride {
            load,
            masked,
            width,
            register,
            base,
        } => {
            // Masked transfers are selected only by the exact known-config
            // path, which supplies a statically proven EMUL and a scalar
            // predicate loop. Keep the generic dynamic decoder on its helper.
            function.instruction(&Instruction::I32Const(i32::from(!masked)));
            function.instruction(&Instruction::I32And);
            emit_vector_memory_guard(
                function,
                layout,
                load,
                Some(width),
                register,
                base,
                temps,
                fractional_lmul,
                None,
                address_preloaded,
                false,
            );
        }
        VectorDirect::Strided {
            load,
            masked,
            width,
            register,
            base,
            stride,
        } => {
            function.instruction(&Instruction::I32Const(i32::from(!masked)));
            function.instruction(&Instruction::I32And);
            emit_vector_strided_memory_guard(
                function, layout, state, load, width, register, base, stride, temps, true,
            );
        }
        VectorDirect::SlideImmediate {
            destination,
            source,
            ..
        }
        | VectorDirect::SlideOne {
            destination,
            source,
            ..
        }
        | VectorDirect::GatherImmediate {
            destination,
            source,
            ..
        } => {
            emit_vector_group_alignment(function, destination, temps);
            emit_vector_group_alignment(function, source, temps);

            // A single Wasm SIMD value can implement every legal fractional
            // group and LMUL=1 exactly. Wider register groups need cross-vreg
            // shuffles and deliberately retain the helper for now.
            function.instruction(&Instruction::LocalGet(temps.span));
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::I32Eq);
            function.instruction(&Instruction::I32And);
        }
        VectorDirect::GatherVector {
            destination,
            source,
            indices,
        } => {
            emit_vector_group_alignment(function, destination, temps);
            emit_vector_group_alignment(function, source, temps);
            emit_vector_group_alignment(function, indices, temps);
            function.instruction(&Instruction::LocalGet(temps.span));
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::I32Eq);
            function.instruction(&Instruction::I32And);
        }
        VectorDirect::Index { destination } => {
            emit_vector_group_alignment(function, destination, temps);

            // One Wasm SIMD value covers every fractional group and LMUL=1.
            // Wider groups need a per-chunk index bias and retain the helper.
            function.instruction(&Instruction::LocalGet(temps.span));
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::I32Eq);
            function.instruction(&Instruction::I32And);
        }
        VectorDirect::Reduction { source, .. } => {
            emit_vector_group_alignment(function, source, temps);
        }
        VectorDirect::MaskLogic { .. } => {}
        VectorDirect::WidenAddSub { .. } | VectorDirect::WidenMultiplyAccumulate { .. } => {
            // A fractional narrow group (2/4/8 bytes) and its doubled-width
            // destination both fit in one architectural 16-byte register.
            // Wider configurations retain the helper until cross-register
            // widening is lowered as a unit.
            function.instruction(&Instruction::I32Const(i32::from(fractional_lmul)));
            function.instruction(&Instruction::I32And);
            function.instruction(&Instruction::LocalGet(temps.vtype));
            function.instruction(&Instruction::I64Const(3));
            function.instruction(&Instruction::I64ShrU);
            function.instruction(&Instruction::I64Const(7));
            function.instruction(&Instruction::I64And);
            function.instruction(&Instruction::I64Const(3));
            function.instruction(&Instruction::I64LtU);
            function.instruction(&Instruction::I32And);
        }
        VectorDirect::FloatSign {
            destination,
            source2,
            source1,
            ..
        } => {
            emit_vector_group_alignment(function, destination, temps);
            emit_vector_group_alignment(function, source2, temps);
            emit_vector_group_alignment(function, source1, temps);

            // G supplies vector f32/f64 only; no Zvfh lowering is implied.
            function.instruction(&Instruction::LocalGet(temps.vtype));
            function.instruction(&Instruction::I64Const(3));
            function.instruction(&Instruction::I64ShrU);
            function.instruction(&Instruction::I64Const(7));
            function.instruction(&Instruction::I64And);
            function.instruction(&Instruction::I64Const(2));
            function.instruction(&Instruction::I64GeU);
            function.instruction(&Instruction::I32And);

            if layout.vector == Some(VectorCapability::System) {
                function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
                function.instruction(&Instruction::I64Load(memarg(3, 0)));
                function.instruction(&Instruction::I64Const(3 << 13));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Eqz);
                function.instruction(&Instruction::I32Eqz);
                function.instruction(&Instruction::I32And);
            }
        }
        VectorDirect::FloatBroadcast { destination, .. } => {
            emit_vector_group_alignment(function, destination, temps);
            emit_vector_float_width_guard(function, layout, temps);
        }
        VectorDirect::FloatSlideOne {
            destination,
            source,
            ..
        } => {
            emit_vector_group_alignment(function, destination, temps);
            emit_vector_group_alignment(function, source, temps);
            function.instruction(&Instruction::LocalGet(temps.span));
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::I32Eq);
            function.instruction(&Instruction::I32And);
            emit_vector_float_width_guard(function, layout, temps);
        }
        VectorDirect::Compare {
            op,
            source2,
            operand,
            ..
        } => {
            emit_vector_group_alignment(function, source2, temps);
            if let VectorOperand::Vector(source1) = operand {
                emit_vector_group_alignment(function, source1, temps);
            }
            function.instruction(&Instruction::LocalGet(temps.span));
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::I32Eq);
            function.instruction(&Instruction::I32And);
            if matches!(
                op,
                VectorCompareOp::LessUnsigned
                    | VectorCompareOp::LessEqualUnsigned
                    | VectorCompareOp::GreaterUnsigned
            ) {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64Ne);
                function.instruction(&Instruction::I32And);
            }
        }
        VectorDirect::ConfigImmediate { destination, .. } => {
            // The destination write to x[rd] only happens when rd != 0; rd=0
            // discards vl (architecturally still computed). Validate the
            // register alignment when a real destination exists.
            if destination != 0 {
                emit_vector_group_alignment(function, destination, temps);
            }
            // No extra checks: vector_direct_available already enforces legal
            // vtype + the right SEW/LMUL pair for this machine.
        }
        VectorDirect::ConfigRetainFull { vlmax, .. } => {
            // vsetvli x0, x0, vtype must leave vtype and vl untouched. The
            // upstream guard checks the vtype bits are legal AND that the new
            // vtype shares VLMAX with the current one. We re-verify here so
            // the host SIMD path stays inside a single guard contract.
            emit_vector_retain_full_guard(function, layout, state, temps, vlmax);
            function.instruction(&Instruction::I32And);
        }
        VectorDirect::WholeRegisterMove { destination, source, .. } => {
            // Whole-register moves (vmv1r.v … vmv8r.v) copy 16-byte chunks.
            // Both groups must stay within v0..v31; vector_direct_available
            // already enforces that, so only the alignment check is needed.
            emit_vector_group_alignment(function, destination, temps);
            emit_vector_group_alignment(function, source, temps);
        }
        VectorDirect::WholeRegisterMemory { register, base, .. } => {
            // vlr.v / vsr.v whole-register loads + stores share the unit-stride
            // memory guard once we know the group shape.
            emit_vector_group_alignment(function, register, temps);
            emit_vector_group_alignment(function, base, temps);
            emit_vector_memory_guard(
                function,
                layout,
                // load bit is irrelevant for the alignment guard itself; the
                // body routes through emit_vector_whole_memory_body which
                // checks the direction internally.
                true,
                None,
                register,
                base,
                temps,
                fractional_lmul,
                None,
                address_preloaded,
                false,
            );
        }
        VectorDirect::ScalarInsert { .. } => {
            // destination is a vector register (no extra alignment check:
            // vector_direct_available requires the group fit), source is an
            // x register (no vector alignment applies).
        }
        VectorDirect::ScalarExtract { .. } => {
            // destination is an x register (always scalar, no alignment);
            // source is a vector register (vector_direct_available ensures
            // the group fits).
        }
        VectorDirect::FloatScalarInsert { .. } => {
            emit_vector_float_width_guard(function, layout, temps);
        }
        VectorDirect::FloatScalarExtract { .. } => {
            emit_vector_float_width_guard(function, layout, temps);
        }
    }

    if layout.vector == Some(VectorCapability::System) {
        function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64Const(3 << 9));
        function.instruction(&Instruction::I64And);
        function.instruction(&Instruction::I64Eqz);
        function.instruction(&Instruction::I32Eqz);
        function.instruction(&Instruction::I32And);
    }
}

fn emit_vector_float_width_guard(function: &mut Function, layout: JitLayout, temps: VectorTemps) {
    // G supplies vector f32/f64 only; no Zvfh lowering is implied.
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(3));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Const(7));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(2));
    function.instruction(&Instruction::I64GeU);
    function.instruction(&Instruction::I32And);
    emit_vector_system_fp_enabled_guard(function, layout);
}

fn emit_vector_register_address(
    function: &mut Function,
    state: VectorStateLayout,
    register: u8,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(
        state.regs_base.wrapping_add(u32::from(register) * 16) as i32,
    ));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(4));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Add);
}

fn emit_vector_splat(
    function: &mut Function,
    layout: JitLayout,
    operand: VectorOperand,
    sew: u8,
    temps: VectorTemps,
) {
    match operand {
        VectorOperand::ScalarX(register) => {
            function.instruction(&Instruction::I32Const(layout.x_base as i32));
            function.instruction(&Instruction::I64Load(memarg(3, u64::from(register) * 8)));
            if sew != 64 {
                function.instruction(&Instruction::I32WrapI64);
            }
        }
        VectorOperand::Immediate(value) => {
            if sew == 64 {
                function.instruction(&Instruction::I64Const(i64::from(value)));
            } else {
                function.instruction(&Instruction::I32Const(i32::from(value)));
            }
        }
        VectorOperand::Vector(_) => unreachable!("vector operand is loaded per register chunk"),
    }
    function.instruction(&match sew {
        8 => Instruction::I8x16Splat,
        16 => Instruction::I16x8Splat,
        32 => Instruction::I32x4Splat,
        64 => Instruction::I64x2Splat,
        _ => unreachable!("validated vector SEW"),
    });
    function.instruction(&Instruction::LocalSet(temps.splat));
}

fn emit_vector_operand(
    function: &mut Function,
    state: VectorStateLayout,
    operand: VectorOperand,
    temps: VectorTemps,
) {
    match operand {
        VectorOperand::Vector(register) => {
            emit_vector_register_address(function, state, register, temps);
            function.instruction(&Instruction::V128Load(memarg(4, 0)));
        }
        VectorOperand::ScalarX(_) | VectorOperand::Immediate(_) => {
            function.instruction(&Instruction::LocalGet(temps.splat));
        }
    }
}

fn emit_vector_shift_amount(function: &mut Function, layout: JitLayout, operand: VectorOperand) {
    match operand {
        VectorOperand::ScalarX(register) => {
            function.instruction(&Instruction::I32Const(layout.x_base as i32));
            function.instruction(&Instruction::I64Load(memarg(3, u64::from(register) * 8)));
            function.instruction(&Instruction::I32WrapI64);
        }
        VectorOperand::Immediate(value) => {
            function.instruction(&Instruction::I32Const(i32::from(value)));
        }
        VectorOperand::Vector(_) => unreachable!("per-lane vector shifts keep helper lowering"),
    }
}

fn emit_vector_lane_operator(function: &mut Function, op: VectorLaneOp, sew: u8) {
    let instruction = match (op, sew) {
        (VectorLaneOp::Add, 8) => Instruction::I8x16Add,
        (VectorLaneOp::Add, 16) => Instruction::I16x8Add,
        (VectorLaneOp::Add, 32) => Instruction::I32x4Add,
        (VectorLaneOp::Add, 64) => Instruction::I64x2Add,
        (VectorLaneOp::Sub | VectorLaneOp::ReverseSub, 8) => Instruction::I8x16Sub,
        (VectorLaneOp::Sub | VectorLaneOp::ReverseSub, 16) => Instruction::I16x8Sub,
        (VectorLaneOp::Sub | VectorLaneOp::ReverseSub, 32) => Instruction::I32x4Sub,
        (VectorLaneOp::Sub | VectorLaneOp::ReverseSub, 64) => Instruction::I64x2Sub,
        (VectorLaneOp::MinUnsigned, 8) => Instruction::I8x16MinU,
        (VectorLaneOp::MinUnsigned, 16) => Instruction::I16x8MinU,
        (VectorLaneOp::MinUnsigned, 32) => Instruction::I32x4MinU,
        (VectorLaneOp::MinSigned, 8) => Instruction::I8x16MinS,
        (VectorLaneOp::MinSigned, 16) => Instruction::I16x8MinS,
        (VectorLaneOp::MinSigned, 32) => Instruction::I32x4MinS,
        (VectorLaneOp::MaxUnsigned, 8) => Instruction::I8x16MaxU,
        (VectorLaneOp::MaxUnsigned, 16) => Instruction::I16x8MaxU,
        (VectorLaneOp::MaxUnsigned, 32) => Instruction::I32x4MaxU,
        (VectorLaneOp::MaxSigned, 8) => Instruction::I8x16MaxS,
        (VectorLaneOp::MaxSigned, 16) => Instruction::I16x8MaxS,
        (VectorLaneOp::MaxSigned, 32) => Instruction::I32x4MaxS,
        (VectorLaneOp::And, _) => Instruction::V128And,
        (VectorLaneOp::Or, _) => Instruction::V128Or,
        (VectorLaneOp::Xor, _) => Instruction::V128Xor,
        (VectorLaneOp::ShiftLeft, 8) => Instruction::I8x16Shl,
        (VectorLaneOp::ShiftLeft, 16) => Instruction::I16x8Shl,
        (VectorLaneOp::ShiftLeft, 32) => Instruction::I32x4Shl,
        (VectorLaneOp::ShiftLeft, 64) => Instruction::I64x2Shl,
        (VectorLaneOp::ShiftRightUnsigned, 8) => Instruction::I8x16ShrU,
        (VectorLaneOp::ShiftRightUnsigned, 16) => Instruction::I16x8ShrU,
        (VectorLaneOp::ShiftRightUnsigned, 32) => Instruction::I32x4ShrU,
        (VectorLaneOp::ShiftRightUnsigned, 64) => Instruction::I64x2ShrU,
        (VectorLaneOp::ShiftRightSigned, 8) => Instruction::I8x16ShrS,
        (VectorLaneOp::ShiftRightSigned, 16) => Instruction::I16x8ShrS,
        (VectorLaneOp::ShiftRightSigned, 32) => Instruction::I32x4ShrS,
        (VectorLaneOp::ShiftRightSigned, 64) => Instruction::I64x2ShrS,
        (VectorLaneOp::Multiply, 16) => Instruction::I16x8Mul,
        (VectorLaneOp::Multiply, 32) => Instruction::I32x4Mul,
        (VectorLaneOp::Multiply, 64) => Instruction::I64x2Mul,
        // The direct guard makes these dynamically unreachable. Keeping an
        // explicit trap here also makes any future guard regression fail hard
        // instead of silently producing a wrong architectural result.
        (VectorLaneOp::Multiply, 8)
        | (
            VectorLaneOp::MinUnsigned
            | VectorLaneOp::MinSigned
            | VectorLaneOp::MaxUnsigned
            | VectorLaneOp::MaxSigned,
            64,
        ) => Instruction::Unreachable,
        (VectorLaneOp::Move, _) => return,
        _ => unreachable!("complete direct vector lane operator matrix"),
    };
    function.instruction(&instruction);
}

fn emit_vector_lane_result_store(
    function: &mut Function,
    state: VectorStateLayout,
    destination: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
    tail_merge: bool,
) {
    if tail_merge {
        function.instruction(&Instruction::LocalGet(temps.result));
        emit_vector_register_address(function, state, destination, temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
        function.instruction(&Instruction::V128Const(i128::from_le_bytes([
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
        ])));
        function.instruction(&Instruction::I32Const(state.vl_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64Const(1));
        function.instruction(&Instruction::LocalGet(temps.vtype));
        function.instruction(&Instruction::I64Const(3));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Const(7));
        function.instruction(&Instruction::I64And);
        function.instruction(&Instruction::I64Shl);
        function.instruction(&Instruction::I64Mul);
        function.instruction(&Instruction::I32WrapI64);
        function.instruction(&Instruction::I8x16Splat);
        function.instruction(&Instruction::I8x16LtU);
        function.instruction(&Instruction::V128Bitselect);
        function.instruction(&Instruction::LocalSet(temps.result));
        emit_vector_register_address(function, state, destination, temps);
        function.instruction(&Instruction::LocalGet(temps.result));
        function.instruction(&Instruction::V128Store(memarg(4, 0)));
        return;
    }
    if !fractional_lmul {
        emit_vector_register_address(function, state, destination, temps);
        function.instruction(&Instruction::LocalGet(temps.result));
        function.instruction(&Instruction::V128Store(memarg(4, 0)));
        return;
    }

    function.instruction(&Instruction::LocalGet(temps.group_bytes));
    function.instruction(&Instruction::I32Const(8));
    function.instruction(&Instruction::I32Eq);
    function.instruction(&Instruction::If(BlockType::Empty));
    emit_vector_register_address(function, state, destination, temps);
    function.instruction(&Instruction::LocalGet(temps.result));
    function.instruction(&Instruction::V128Store64Lane {
        memarg: memarg(3, 0),
        lane: 0,
    });
    function.instruction(&Instruction::Else);

    function.instruction(&Instruction::LocalGet(temps.group_bytes));
    function.instruction(&Instruction::I32Const(4));
    function.instruction(&Instruction::I32Eq);
    function.instruction(&Instruction::If(BlockType::Empty));
    emit_vector_register_address(function, state, destination, temps);
    function.instruction(&Instruction::LocalGet(temps.result));
    function.instruction(&Instruction::V128Store32Lane {
        memarg: memarg(2, 0),
        lane: 0,
    });
    function.instruction(&Instruction::Else);
    emit_vector_register_address(function, state, destination, temps);
    function.instruction(&Instruction::LocalGet(temps.result));
    function.instruction(&Instruction::V128Store16Lane {
        memarg: memarg(1, 0),
        lane: 0,
    });
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);
}

/// Merge a computed SIMD chunk with its old destination according to packed
/// v0 predicate bits. Known-config selection currently uses this for SEW
/// 16/32/64, whose per-chunk bit selectors fit in the corresponding Wasm lane.
fn emit_vector_lane_mask_merge(
    function: &mut Function,
    state: VectorStateLayout,
    destination: u8,
    sew: u8,
    temps: VectorTemps,
) {
    let lane_bytes = usize::from(sew / 8);
    let lanes = 16 / lane_bytes;

    if sew == 8 {
        // Each 128-bit data chunk consumes two packed bytes from v0. Expand
        // the low and high predicate bytes independently into byte lanes.
        function.instruction(&Instruction::I32Const(state.regs_base as i32));
        function.instruction(&Instruction::LocalGet(temps.chunk));
        function.instruction(&Instruction::I32Const(1));
        function.instruction(&Instruction::I32Shl);
        function.instruction(&Instruction::I32Add);
        function.instruction(&Instruction::I32Load16U(memarg(1, 0)));
        function.instruction(&Instruction::LocalSet(temps.index));

        let mut low_selectors = [0u8; 16];
        let mut high_selectors = [0u8; 16];
        for lane in 0..8 {
            low_selectors[lane] = 1 << lane;
            high_selectors[8 + lane] = 1 << lane;
        }
        function.instruction(&Instruction::LocalGet(temps.index));
        function.instruction(&Instruction::I8x16Splat);
        function.instruction(&Instruction::V128Const(i128::from_le_bytes(low_selectors)));
        function.instruction(&Instruction::V128And);
        function.instruction(&Instruction::V128Const(0));
        function.instruction(&Instruction::I8x16Ne);

        function.instruction(&Instruction::LocalGet(temps.index));
        function.instruction(&Instruction::I32Const(8));
        function.instruction(&Instruction::I32ShrU);
        function.instruction(&Instruction::I8x16Splat);
        function.instruction(&Instruction::V128Const(i128::from_le_bytes(high_selectors)));
        function.instruction(&Instruction::V128And);
        function.instruction(&Instruction::V128Const(0));
        function.instruction(&Instruction::I8x16Ne);
        function.instruction(&Instruction::V128Or);
        function.instruction(&Instruction::LocalSet(temps.mask));
    } else {
        let mut selector_bytes = [0u8; 16];
        for lane in 0..lanes {
            let selector = (1u64 << lane).to_le_bytes();
            let offset = lane * lane_bytes;
            selector_bytes[offset..offset + lane_bytes].copy_from_slice(&selector[..lane_bytes]);
        }

        function.instruction(&Instruction::I32Const(state.regs_base as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::LocalGet(temps.chunk));
        function.instruction(&Instruction::I64ExtendI32U);
        function.instruction(&Instruction::I64Const(lanes as i64));
        function.instruction(&Instruction::I64Mul);
        function.instruction(&Instruction::I64ShrU);
        if sew == 64 {
            function.instruction(&Instruction::I64x2Splat);
        } else {
            function.instruction(&Instruction::I32WrapI64);
            function.instruction(&match sew {
                16 => Instruction::I16x8Splat,
                32 => Instruction::I32x4Splat,
                _ => unreachable!("known masked SIMD lane width"),
            });
        }
        function.instruction(&Instruction::V128Const(i128::from_le_bytes(selector_bytes)));
        function.instruction(&Instruction::V128And);
        function.instruction(&Instruction::V128Const(0));
        function.instruction(&match sew {
            16 => Instruction::I16x8Ne,
            32 => Instruction::I32x4Ne,
            64 => Instruction::I64x2Ne,
            _ => unreachable!("known masked SIMD lane width"),
        });
        function.instruction(&Instruction::LocalSet(temps.mask));
    }

    function.instruction(&Instruction::LocalGet(temps.result));
    emit_vector_register_address(function, state, destination, temps);
    function.instruction(&Instruction::V128Load(memarg(4, 0)));
    function.instruction(&Instruction::LocalGet(temps.mask));
    function.instruction(&Instruction::V128Bitselect);
    function.instruction(&Instruction::LocalSet(temps.result));
}

fn emit_vector_lane_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    op: VectorLaneOp,
    masked: bool,
    destination: u8,
    source2: Option<u8>,
    operand: VectorOperand,
    sew: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
    tail_merge: bool,
) {
    let shift = matches!(
        op,
        VectorLaneOp::ShiftLeft | VectorLaneOp::ShiftRightUnsigned | VectorLaneOp::ShiftRightSigned
    );
    if !shift && !matches!(operand, VectorOperand::Vector(_)) {
        emit_vector_splat(function, layout, operand, sew, temps);
    }

    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    function.instruction(&Instruction::Loop(BlockType::Empty));

    if op == VectorLaneOp::Move {
        emit_vector_operand(function, state, operand, temps);
    } else if op == VectorLaneOp::ReverseSub {
        emit_vector_operand(function, state, operand, temps);
        emit_vector_register_address(function, state, source2.expect("binary source2"), temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
        emit_vector_lane_operator(function, op, sew);
    } else {
        emit_vector_register_address(function, state, source2.expect("binary source2"), temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
        if shift {
            emit_vector_shift_amount(function, layout, operand);
        } else {
            emit_vector_operand(function, state, operand, temps);
        }
        emit_vector_lane_operator(function, op, sew);
    }
    function.instruction(&Instruction::LocalSet(temps.result));
    if masked {
        emit_vector_lane_mask_merge(function, state, destination, sew, temps);
    }
    emit_vector_lane_result_store(
        function,
        state,
        destination,
        temps,
        fractional_lmul,
        tail_merge,
    );

    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.chunk));
    function.instruction(&Instruction::LocalGet(temps.span));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);
}

fn emit_vector_fractional_low_bytes_mask(function: &mut Function, temps: VectorTemps) {
    for bytes in [2u32, 4, 8] {
        function.instruction(&Instruction::LocalGet(temps.group_bytes));
        function.instruction(&Instruction::I32Const(bytes as i32));
        function.instruction(&Instruction::I32Eq);
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::LocalGet(temps.result));
        let mask = (1i128 << (bytes * 8)) - 1;
        function.instruction(&Instruction::V128Const(mask));
        function.instruction(&Instruction::V128And);
        function.instruction(&Instruction::LocalSet(temps.result));
        function.instruction(&Instruction::End);
    }
}

fn emit_vector_slide_immediate_body(
    function: &mut Function,
    state: VectorStateLayout,
    up: bool,
    destination: u8,
    source: u8,
    offset: u8,
    sew: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
    tail_merge: bool,
) {
    let byte_offset = usize::from(offset) * usize::from(sew / 8);
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));

    if byte_offset >= 16 {
        // slideup leaves every active element undisturbed; slidedown writes
        // zero when every selected source element is beyond VLMAX.
        if !up {
            function.instruction(&Instruction::V128Const(0));
            function.instruction(&Instruction::LocalSet(temps.result));
            emit_vector_lane_result_store(
                function,
                state,
                destination,
                temps,
                fractional_lmul,
                tail_merge,
            );
        }
        return;
    }

    let mut lanes = [0u8; 16];
    if up {
        emit_vector_register_address(function, state, destination, temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
        emit_vector_register_address(function, state, source, temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
        for (index, lane) in lanes.iter_mut().enumerate() {
            *lane = if index < byte_offset {
                index as u8
            } else {
                (16 + index - byte_offset) as u8
            };
        }
    } else {
        emit_vector_register_address(function, state, source, temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
        function.instruction(&Instruction::LocalSet(temps.result));
        if fractional_lmul {
            // Bytes above a fractional register group are not source tail
            // elements. Clear them before the shuffle so out-of-range active
            // lanes receive the architectural zero value.
            emit_vector_fractional_low_bytes_mask(function, temps);
        }
        function.instruction(&Instruction::LocalGet(temps.result));
        function.instruction(&Instruction::V128Const(0));
        for (index, lane) in lanes.iter_mut().enumerate() {
            *lane = if index + byte_offset < 16 {
                (index + byte_offset) as u8
            } else {
                16
            };
        }
    }
    function.instruction(&Instruction::I8x16Shuffle(lanes));
    function.instruction(&Instruction::LocalSet(temps.result));
    emit_vector_lane_result_store(
        function,
        state,
        destination,
        temps,
        fractional_lmul,
        tail_merge,
    );
}

fn emit_vector_slide_one_scalar_store(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    destination: u8,
    scalar: u8,
    sew: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
) {
    function.instruction(&Instruction::I32Const(
        state.regs_base.wrapping_add(u32::from(destination) * 16) as i32,
    ));
    if fractional_lmul {
        function.instruction(&Instruction::LocalGet(temps.group_bytes));
        function.instruction(&Instruction::I32Add);
        function.instruction(&Instruction::I32Const(i32::from(sew / 8)));
        function.instruction(&Instruction::I32Sub);
    } else {
        function.instruction(&Instruction::I32Const(16 - i32::from(sew / 8)));
        function.instruction(&Instruction::I32Add);
    }
    function.instruction(&Instruction::I32Const(layout.x_base as i32));
    function.instruction(&Instruction::I64Load(memarg(3, u64::from(scalar) * 8)));
    function.instruction(&match sew {
        8 => Instruction::I64Store8(memarg(0, 0)),
        16 => Instruction::I64Store16(memarg(1, 0)),
        32 => Instruction::I64Store32(memarg(2, 0)),
        64 => Instruction::I64Store(memarg(3, 0)),
        _ => unreachable!("validated vector SEW"),
    });
}

#[allow(clippy::too_many_arguments)]
fn emit_vector_slide_one_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    up: bool,
    destination: u8,
    source: u8,
    scalar: u8,
    sew: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
) {
    let width = usize::from(sew / 8);
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    let mut lanes = [0u8; 16];
    if up {
        emit_vector_splat(function, layout, VectorOperand::ScalarX(scalar), sew, temps);
        function.instruction(&Instruction::LocalGet(temps.splat));
        emit_vector_register_address(function, state, source, temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
        for (index, lane) in lanes.iter_mut().enumerate() {
            *lane = if index < width {
                index as u8
            } else {
                (16 + index - width) as u8
            };
        }
    } else {
        emit_vector_register_address(function, state, source, temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
        function.instruction(&Instruction::V128Const(0));
        for (index, lane) in lanes.iter_mut().enumerate() {
            *lane = if index + width < 16 {
                (index + width) as u8
            } else {
                16
            };
        }
    }
    function.instruction(&Instruction::I8x16Shuffle(lanes));
    function.instruction(&Instruction::LocalSet(temps.result));
    emit_vector_lane_result_store(function, state, destination, temps, fractional_lmul, false);
    if !up {
        emit_vector_slide_one_scalar_store(
            function,
            layout,
            state,
            destination,
            scalar,
            sew,
            temps,
            fractional_lmul,
        );
    }
}

fn emit_vector_gather_immediate_value(
    function: &mut Function,
    state: VectorStateLayout,
    source: u8,
    index: u8,
    sew: u8,
) {
    function.instruction(&Instruction::I32Const(
        state
            .regs_base
            .wrapping_add(u32::from(source) * 16)
            .wrapping_add(u32::from(index) * u32::from(sew / 8)) as i32,
    ));
    function.instruction(&match sew {
        8 => Instruction::I32Load8U(memarg(0, 0)),
        16 => Instruction::I32Load16U(memarg(1, 0)),
        32 => Instruction::I32Load(memarg(2, 0)),
        64 => Instruction::I64Load(memarg(3, 0)),
        _ => unreachable!("validated vector SEW"),
    });
    function.instruction(&match sew {
        8 => Instruction::I8x16Splat,
        16 => Instruction::I16x8Splat,
        32 => Instruction::I32x4Splat,
        64 => Instruction::I64x2Splat,
        _ => unreachable!("validated vector SEW"),
    });
}

fn emit_vector_gather_immediate_body(
    function: &mut Function,
    state: VectorStateLayout,
    destination: u8,
    source: u8,
    index: u8,
    sew: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
    tail_merge: bool,
) {
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    let byte_offset = u32::from(index) * u32::from(sew / 8);
    if fractional_lmul {
        function.instruction(&Instruction::LocalGet(temps.group_bytes));
        function.instruction(&Instruction::I32Const(byte_offset as i32));
        function.instruction(&Instruction::I32GtU);
        function.instruction(&Instruction::If(BlockType::Result(ValType::V128)));
        emit_vector_gather_immediate_value(function, state, source, index, sew);
        function.instruction(&Instruction::Else);
        function.instruction(&Instruction::V128Const(0));
        function.instruction(&Instruction::End);
    } else if byte_offset < 16 {
        emit_vector_gather_immediate_value(function, state, source, index, sew);
    } else {
        function.instruction(&Instruction::V128Const(0));
    }
    function.instruction(&Instruction::LocalSet(temps.result));
    emit_vector_lane_result_store(
        function,
        state,
        destination,
        temps,
        fractional_lmul,
        tail_merge,
    );
}

fn emit_vector_gather_vector_body(
    function: &mut Function,
    state: VectorStateLayout,
    destination: u8,
    source: u8,
    indices: u8,
    sew: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
    partial_vl: bool,
) {
    let width = i32::from(sew / 8);
    let width_shift = i64::from((sew / 8).trailing_zeros());
    if !fractional_lmul {
        function.instruction(&Instruction::I32Const(16));
        function.instruction(&Instruction::LocalSet(temps.group_bytes));
    }
    if partial_vl {
        function.instruction(&Instruction::I32Const(state.vl_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I32WrapI64);
        function.instruction(&Instruction::I32Const(width));
        function.instruction(&Instruction::I32Mul);
    } else {
        function.instruction(&Instruction::LocalGet(temps.group_bytes));
    }
    function.instruction(&Instruction::LocalSet(temps.linear));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.index));
    function.instruction(&Instruction::Loop(BlockType::Empty));

    function.instruction(&Instruction::I32Const(
        state.regs_base.wrapping_add(u32::from(indices) * 16) as i32,
    ));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Add);
    function.instruction(&match sew {
        8 => Instruction::I64Load8U(memarg(0, 0)),
        16 => Instruction::I64Load16U(memarg(1, 0)),
        32 => Instruction::I64Load32U(memarg(2, 0)),
        64 => Instruction::I64Load(memarg(3, 0)),
        _ => unreachable!("validated vector SEW"),
    });
    function.instruction(&Instruction::LocalSet(temps.last));

    function.instruction(&Instruction::I32Const(
        state.regs_base.wrapping_add(u32::from(destination) * 16) as i32,
    ));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::LocalGet(temps.group_bytes));
    function.instruction(&Instruction::I64ExtendI32U);
    function.instruction(&Instruction::I64Const(width_shift));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64LtU);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
    function.instruction(&Instruction::I32Const(
        state.regs_base.wrapping_add(u32::from(source) * 16) as i32,
    ));
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Const(width_shift));
    function.instruction(&Instruction::I64Shl);
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::I32Add);
    function.instruction(&match sew {
        8 => Instruction::I64Load8U(memarg(0, 0)),
        16 => Instruction::I64Load16U(memarg(1, 0)),
        32 => Instruction::I64Load32U(memarg(2, 0)),
        64 => Instruction::I64Load(memarg(3, 0)),
        _ => unreachable!("validated vector SEW"),
    });
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::I64Const(0));
    function.instruction(&Instruction::End);
    function.instruction(&match sew {
        8 => Instruction::I64Store8(memarg(0, 0)),
        16 => Instruction::I64Store16(memarg(1, 0)),
        32 => Instruction::I64Store32(memarg(2, 0)),
        64 => Instruction::I64Store(memarg(3, 0)),
        _ => unreachable!("validated vector SEW"),
    });

    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(width));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.index));
    function.instruction(&Instruction::LocalGet(temps.linear));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);
}

fn emit_vector_index_body(
    function: &mut Function,
    state: VectorStateLayout,
    destination: u8,
    sew: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
    tail_merge: bool,
) {
    let mut bytes = [0u8; 16];
    let width = usize::from(sew / 8);
    for lane in 0..(16 / width) {
        let value = (lane as u64).to_le_bytes();
        bytes[lane * width..(lane + 1) * width].copy_from_slice(&value[..width]);
    }
    function.instruction(&Instruction::V128Const(i128::from_le_bytes(bytes)));
    function.instruction(&Instruction::LocalSet(temps.result));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    function.instruction(&Instruction::LocalGet(temps.span));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Eq);
    function.instruction(&Instruction::If(BlockType::Empty));
    emit_vector_lane_result_store(
        function,
        state,
        destination,
        temps,
        fractional_lmul,
        tail_merge,
    );
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::LocalGet(temps.result));
    function.instruction(&Instruction::LocalSet(temps.splat));
    function.instruction(&Instruction::Loop(BlockType::Empty));
    function.instruction(&Instruction::LocalGet(temps.splat));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const((128 / i32::from(sew)) as i32));
    function.instruction(&Instruction::I32Mul);
    if sew == 64 {
        function.instruction(&Instruction::I64ExtendI32U);
        function.instruction(&Instruction::I64x2Splat);
    } else {
        function.instruction(&match sew {
            8 => Instruction::I8x16Splat,
            16 => Instruction::I16x8Splat,
            32 => Instruction::I32x4Splat,
            _ => unreachable!("validated vector SEW"),
        });
    }
    function.instruction(&match sew {
        8 => Instruction::I8x16Add,
        16 => Instruction::I16x8Add,
        32 => Instruction::I32x4Add,
        64 => Instruction::I64x2Add,
        _ => unreachable!("validated vector SEW"),
    });
    function.instruction(&Instruction::LocalSet(temps.result));
    emit_vector_lane_result_store(function, state, destination, temps, false, false);
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.chunk));
    function.instruction(&Instruction::LocalGet(temps.span));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);
}

fn emit_vector_reduction_simd_operator(function: &mut Function, op: VectorReductionOp) {
    function.instruction(&match op {
        VectorReductionOp::And => Instruction::V128And,
        VectorReductionOp::Or => Instruction::V128Or,
        VectorReductionOp::Xor => Instruction::V128Xor,
    });
}

fn emit_vector_reduction_scalar_operator(function: &mut Function, op: VectorReductionOp) {
    function.instruction(&match op {
        VectorReductionOp::And => Instruction::I64And,
        VectorReductionOp::Or => Instruction::I64Or,
        VectorReductionOp::Xor => Instruction::I64Xor,
    });
}

fn emit_vector_reduction_fractional_identity(
    function: &mut Function,
    op: VectorReductionOp,
    temps: VectorTemps,
) {
    for bytes in [2u32, 4, 8] {
        function.instruction(&Instruction::LocalGet(temps.group_bytes));
        function.instruction(&Instruction::I32Const(bytes as i32));
        function.instruction(&Instruction::I32Eq);
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::LocalGet(temps.result));
        let low_mask = (1i128 << (bytes * 8)) - 1;
        function.instruction(&Instruction::V128Const(low_mask));
        function.instruction(&Instruction::V128And);
        if op == VectorReductionOp::And {
            function.instruction(&Instruction::V128Const(!low_mask));
            function.instruction(&Instruction::V128Or);
        }
        function.instruction(&Instruction::LocalSet(temps.result));
        function.instruction(&Instruction::End);
    }
}

fn emit_vector_reduction_body(
    function: &mut Function,
    state: VectorStateLayout,
    op: VectorReductionOp,
    destination: u8,
    source: u8,
    seed: u8,
    sew: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
) {
    let identity = if op == VectorReductionOp::And { -1 } else { 0 };

    function.instruction(&Instruction::I32Const(
        state.regs_base.wrapping_add(u32::from(seed) * 16) as i32,
    ));
    function.instruction(&match sew {
        8 => Instruction::I64Load8U(memarg(0, 0)),
        16 => Instruction::I64Load16U(memarg(1, 0)),
        32 => Instruction::I64Load32U(memarg(2, 0)),
        64 => Instruction::I64Load(memarg(3, 0)),
        _ => unreachable!("validated vector SEW"),
    });
    function.instruction(&Instruction::LocalSet(temps.last));

    function.instruction(&Instruction::V128Const(identity));
    function.instruction(&Instruction::LocalSet(temps.result));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    function.instruction(&Instruction::Loop(BlockType::Empty));
    function.instruction(&Instruction::LocalGet(temps.result));
    emit_vector_register_address(function, state, source, temps);
    function.instruction(&Instruction::V128Load(memarg(4, 0)));
    emit_vector_reduction_simd_operator(function, op);
    function.instruction(&Instruction::LocalSet(temps.result));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.chunk));
    function.instruction(&Instruction::LocalGet(temps.span));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);

    if fractional_lmul {
        emit_vector_reduction_fractional_identity(function, op, temps);
    }

    for byte_shift in match sew {
        8 => &[8usize, 4, 2, 1][..],
        16 => &[8usize, 4, 2][..],
        32 => &[8usize, 4][..],
        64 => &[8usize][..],
        _ => unreachable!("validated vector SEW"),
    } {
        let mut lanes = [16u8; 16];
        for (index, lane) in lanes.iter_mut().enumerate().take(16 - byte_shift) {
            *lane = (index + byte_shift) as u8;
        }
        function.instruction(&Instruction::LocalGet(temps.result));
        function.instruction(&Instruction::LocalGet(temps.result));
        function.instruction(&Instruction::V128Const(identity));
        function.instruction(&Instruction::I8x16Shuffle(lanes));
        emit_vector_reduction_simd_operator(function, op);
        function.instruction(&Instruction::LocalSet(temps.result));
    }

    function.instruction(&Instruction::I32Const(
        state.regs_base.wrapping_add(u32::from(destination) * 16) as i32,
    ));
    function.instruction(&Instruction::LocalGet(temps.result));
    function.instruction(&match sew {
        8 => Instruction::I8x16ExtractLaneU(0),
        16 => Instruction::I16x8ExtractLaneU(0),
        32 => Instruction::I32x4ExtractLane(0),
        64 => Instruction::I64x2ExtractLane(0),
        _ => unreachable!("validated vector SEW"),
    });
    if sew != 64 {
        function.instruction(&Instruction::I64ExtendI32U);
    }
    function.instruction(&Instruction::LocalGet(temps.last));
    emit_vector_reduction_scalar_operator(function, op);
    function.instruction(&match sew {
        8 => Instruction::I64Store8(memarg(0, 0)),
        16 => Instruction::I64Store16(memarg(1, 0)),
        32 => Instruction::I64Store32(memarg(2, 0)),
        64 => Instruction::I64Store(memarg(3, 0)),
        _ => unreachable!("validated vector SEW"),
    });
}

fn emit_vector_widen_element_address(
    function: &mut Function,
    state: VectorStateLayout,
    register: u8,
    temps: VectorTemps,
    wide: bool,
) {
    function.instruction(&Instruction::I32Const(
        state.regs_base.wrapping_add(u32::from(register) * 16) as i32,
    ));
    function.instruction(&Instruction::LocalGet(temps.index));
    if wide {
        function.instruction(&Instruction::I32Const(1));
        function.instruction(&Instruction::I32Shl);
    }
    function.instruction(&Instruction::I32Add);
}

fn emit_vector_widen_load(function: &mut Function, width: u8, signed: bool) {
    function.instruction(&match (width, signed) {
        (1, false) => Instruction::I64Load8U(memarg(0, 0)),
        (1, true) => Instruction::I64Load8S(memarg(0, 0)),
        (2, false) => Instruction::I64Load16U(memarg(1, 0)),
        (2, true) => Instruction::I64Load16S(memarg(1, 0)),
        (4, false) => Instruction::I64Load32U(memarg(2, 0)),
        (4, true) => Instruction::I64Load32S(memarg(2, 0)),
        (8, _) => Instruction::I64Load(memarg(3, 0)),
        _ => unreachable!("validated widening width"),
    });
}

fn emit_vector_widen_store(function: &mut Function, width: u8) {
    function.instruction(&match width {
        2 => Instruction::I64Store16(memarg(1, 0)),
        4 => Instruction::I64Store32(memarg(2, 0)),
        8 => Instruction::I64Store(memarg(3, 0)),
        _ => unreachable!("validated widened width"),
    });
}

fn emit_vector_widen_scalar_operand(
    function: &mut Function,
    layout: JitLayout,
    register: u8,
    sew: u8,
    signed: bool,
) {
    function.instruction(&Instruction::I32Const(layout.x_base as i32));
    function.instruction(&Instruction::I64Load(memarg(3, u64::from(register) * 8)));
    if signed {
        function.instruction(&match sew {
            8 => Instruction::I64Extend8S,
            16 => Instruction::I64Extend16S,
            32 => Instruction::I64Extend32S,
            _ => unreachable!("narrow widening SEW"),
        });
    } else {
        let mask = (1u64 << sew) - 1;
        function.instruction(&Instruction::I64Const(mask as i64));
        function.instruction(&Instruction::I64And);
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_vector_widen_add_sub_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    signed: bool,
    subtract: bool,
    wide_left: bool,
    destination: u8,
    source2: u8,
    operand: VectorOperand,
    sew: u8,
    temps: VectorTemps,
) {
    let narrow_width = sew / 8;
    let wide_width = narrow_width * 2;
    if let VectorOperand::ScalarX(register) = operand {
        emit_vector_widen_scalar_operand(function, layout, register, sew, signed);
        function.instruction(&Instruction::LocalSet(temps.last));
    }
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.index));
    function.instruction(&Instruction::Loop(BlockType::Empty));

    emit_vector_widen_element_address(function, state, source2, temps, wide_left);
    emit_vector_widen_load(
        function,
        if wide_left { wide_width } else { narrow_width },
        signed,
    );
    function.instruction(&Instruction::LocalSet(temps.address));
    if let VectorOperand::Vector(register) = operand {
        emit_vector_widen_element_address(function, state, register, temps, false);
        emit_vector_widen_load(function, narrow_width, signed);
        function.instruction(&Instruction::LocalSet(temps.last));
    }

    emit_vector_widen_element_address(function, state, destination, temps, true);
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&if subtract {
        Instruction::I64Sub
    } else {
        Instruction::I64Add
    });
    emit_vector_widen_store(function, wide_width);

    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(i32::from(narrow_width)));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.index));
    function.instruction(&Instruction::LocalGet(temps.group_bytes));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);
}

#[allow(clippy::too_many_arguments)]
fn emit_vector_widen_madd_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    source2_signed: bool,
    operand_signed: bool,
    destination: u8,
    source2: u8,
    operand: VectorOperand,
    sew: u8,
    temps: VectorTemps,
) {
    let narrow_width = sew / 8;
    let wide_width = narrow_width * 2;
    if let VectorOperand::ScalarX(register) = operand {
        emit_vector_widen_scalar_operand(function, layout, register, sew, operand_signed);
        function.instruction(&Instruction::LocalSet(temps.stride));
    }
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.index));
    function.instruction(&Instruction::Loop(BlockType::Empty));

    emit_vector_widen_element_address(function, state, source2, temps, false);
    emit_vector_widen_load(function, narrow_width, source2_signed);
    function.instruction(&Instruction::LocalSet(temps.address));
    if let VectorOperand::Vector(register) = operand {
        emit_vector_widen_element_address(function, state, register, temps, false);
        emit_vector_widen_load(function, narrow_width, operand_signed);
        function.instruction(&Instruction::LocalSet(temps.stride));
    }
    emit_vector_widen_element_address(function, state, destination, temps, true);
    emit_vector_widen_load(function, wide_width, false);
    function.instruction(&Instruction::LocalSet(temps.last));

    emit_vector_widen_element_address(function, state, destination, temps, true);
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::LocalGet(temps.stride));
    function.instruction(&Instruction::I64Mul);
    function.instruction(&Instruction::I64Add);
    emit_vector_widen_store(function, wide_width);

    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(i32::from(narrow_width)));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.index));
    function.instruction(&Instruction::LocalGet(temps.group_bytes));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);
}

fn emit_vector_float_sign_body(
    function: &mut Function,
    state: VectorStateLayout,
    op: VectorFloatSignOp,
    destination: u8,
    source2: u8,
    source1: u8,
    sew: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
) {
    let sign = match sew {
        32 => 0x8000_0000_8000_0000_8000_0000_8000_0000u128 as i128,
        64 => 0x8000_0000_0000_0000_8000_0000_0000_0000u128 as i128,
        _ => unreachable!("guarded floating vector SEW"),
    };
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    function.instruction(&Instruction::Loop(BlockType::Empty));

    emit_vector_register_address(function, state, source2, temps);
    function.instruction(&Instruction::V128Load(memarg(4, 0)));
    match op {
        VectorFloatSignOp::Copy | VectorFloatSignOp::Negate => {
            function.instruction(&Instruction::V128Const(!sign));
            function.instruction(&Instruction::V128And);
            emit_vector_register_address(function, state, source1, temps);
            function.instruction(&Instruction::V128Load(memarg(4, 0)));
            if op == VectorFloatSignOp::Negate {
                function.instruction(&Instruction::V128Const(sign));
                function.instruction(&Instruction::V128Xor);
            }
            function.instruction(&Instruction::V128Const(sign));
            function.instruction(&Instruction::V128And);
            function.instruction(&Instruction::V128Or);
        }
        VectorFloatSignOp::Xor => {
            emit_vector_register_address(function, state, source1, temps);
            function.instruction(&Instruction::V128Load(memarg(4, 0)));
            function.instruction(&Instruction::V128Const(sign));
            function.instruction(&Instruction::V128And);
            function.instruction(&Instruction::V128Xor);
        }
    }
    function.instruction(&Instruction::LocalSet(temps.result));
    emit_vector_lane_result_store(function, state, destination, temps, fractional_lmul, false);

    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.chunk));
    function.instruction(&Instruction::LocalGet(temps.span));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);
}

fn emit_vector_float_scalar_value(
    function: &mut Function,
    layout: JitLayout,
    source: u8,
    sew: u8,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(layout.f_base as i32));
    function.instruction(&Instruction::I64Load(memarg(3, u64::from(source) * 8)));
    if sew == 64 {
        return;
    }
    function.instruction(&Instruction::LocalTee(temps.address));
    function.instruction(&Instruction::I64Const(32));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Const(0xffff_ffff));
    function.instruction(&Instruction::I64Eq);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::I32Const(0x7fc0_0000));
    function.instruction(&Instruction::End);
}

fn emit_vector_float_splat(
    function: &mut Function,
    layout: JitLayout,
    source: u8,
    sew: u8,
    temps: VectorTemps,
) {
    emit_vector_float_scalar_value(function, layout, source, sew, temps);
    function.instruction(&match sew {
        32 => Instruction::I32x4Splat,
        64 => Instruction::I64x2Splat,
        _ => unreachable!("guarded floating vector SEW"),
    });
}

fn emit_vector_float_broadcast_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    destination: u8,
    source: u8,
    sew: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
) {
    emit_vector_float_splat(function, layout, source, sew, temps);
    function.instruction(&Instruction::LocalSet(temps.result));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    function.instruction(&Instruction::Loop(BlockType::Empty));
    emit_vector_lane_result_store(function, state, destination, temps, fractional_lmul, false);
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.chunk));
    function.instruction(&Instruction::LocalGet(temps.span));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);
}

#[allow(clippy::too_many_arguments)]
fn emit_vector_float_slide_one_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    up: bool,
    destination: u8,
    source: u8,
    scalar: u8,
    sew: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
) {
    let width = usize::from(sew / 8);
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    let mut lanes = [0u8; 16];
    if up {
        emit_vector_float_splat(function, layout, scalar, sew, temps);
        function.instruction(&Instruction::LocalSet(temps.splat));
        function.instruction(&Instruction::LocalGet(temps.splat));
        emit_vector_register_address(function, state, source, temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
        for (index, lane) in lanes.iter_mut().enumerate() {
            *lane = if index < width {
                index as u8
            } else {
                (16 + index - width) as u8
            };
        }
    } else {
        emit_vector_register_address(function, state, source, temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
        function.instruction(&Instruction::V128Const(0));
        for (index, lane) in lanes.iter_mut().enumerate() {
            *lane = if index + width < 16 {
                (index + width) as u8
            } else {
                16
            };
        }
    }
    function.instruction(&Instruction::I8x16Shuffle(lanes));
    function.instruction(&Instruction::LocalSet(temps.result));
    emit_vector_lane_result_store(function, state, destination, temps, fractional_lmul, false);
    if !up {
        function.instruction(&Instruction::I32Const(
            state.regs_base.wrapping_add(u32::from(destination) * 16) as i32,
        ));
        if fractional_lmul {
            function.instruction(&Instruction::LocalGet(temps.group_bytes));
            function.instruction(&Instruction::I32Add);
            function.instruction(&Instruction::I32Const(i32::from(sew / 8)));
            function.instruction(&Instruction::I32Sub);
        } else {
            function.instruction(&Instruction::I32Const(16 - i32::from(sew / 8)));
            function.instruction(&Instruction::I32Add);
        }
        emit_vector_float_scalar_value(function, layout, scalar, sew, temps);
        function.instruction(&match sew {
            32 => Instruction::I32Store(memarg(2, 0)),
            64 => Instruction::I64Store(memarg(3, 0)),
            _ => unreachable!("guarded floating vector SEW"),
        });
    }
}

fn emit_vector_float_scalar_insert_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    destination: u8,
    source: u8,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(state.vl_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::I32Eqz);
    function.instruction(&Instruction::If(BlockType::Empty));
    for (vsew, sew) in [(2, 32), (3, 64)] {
        function.instruction(&Instruction::LocalGet(temps.vtype));
        function.instruction(&Instruction::I64Const(3));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Const(7));
        function.instruction(&Instruction::I64And);
        function.instruction(&Instruction::I64Const(vsew));
        function.instruction(&Instruction::I64Eq);
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::I32Const(
            state.regs_base.wrapping_add(u32::from(destination) * 16) as i32,
        ));
        emit_vector_float_scalar_value(function, layout, source, sew, temps);
        function.instruction(&match sew {
            32 => Instruction::I32Store(memarg(2, 0)),
            64 => Instruction::I64Store(memarg(3, 0)),
            _ => unreachable!(),
        });
        function.instruction(&Instruction::End);
    }
    function.instruction(&Instruction::End);
    emit_vector_direct_finish(function, layout, state);
}

fn emit_vector_float_scalar_extract_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    destination: u8,
    source: u8,
    temps: VectorTemps,
) {
    for (vsew, sew) in [(2, 32), (3, 64)] {
        function.instruction(&Instruction::LocalGet(temps.vtype));
        function.instruction(&Instruction::I64Const(3));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Const(7));
        function.instruction(&Instruction::I64And);
        function.instruction(&Instruction::I64Const(vsew));
        function.instruction(&Instruction::I64Eq);
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::I32Const(layout.f_base as i32));
        if sew == 32 {
            function.instruction(&Instruction::I64Const(-4_294_967_296));
            function.instruction(&Instruction::I32Const(
                state.regs_base.wrapping_add(u32::from(source) * 16) as i32,
            ));
            function.instruction(&Instruction::I64Load32U(memarg(2, 0)));
            function.instruction(&Instruction::I64Or);
        } else {
            function.instruction(&Instruction::I32Const(
                state.regs_base.wrapping_add(u32::from(source) * 16) as i32,
            ));
            function.instruction(&Instruction::I64Load(memarg(3, 0)));
        }
        function.instruction(&Instruction::I64Store(memarg(
            3,
            u64::from(destination) * 8,
        )));
        function.instruction(&Instruction::End);
    }
    emit_vector_system_dirty(function, layout);
    emit_vector_system_fp_dirty(function, layout);
    emit_profile_counter_add(function, state.simd_count_addr, 1);
}

fn emit_vector_compare_operator(function: &mut Function, op: VectorCompareOp, sew: u8) {
    let instruction = match (op, sew) {
        (VectorCompareOp::Equal, 8) => Instruction::I8x16Eq,
        (VectorCompareOp::Equal, 16) => Instruction::I16x8Eq,
        (VectorCompareOp::Equal, 32) => Instruction::I32x4Eq,
        (VectorCompareOp::Equal, 64) => Instruction::I64x2Eq,
        (VectorCompareOp::NotEqual, 8) => Instruction::I8x16Ne,
        (VectorCompareOp::NotEqual, 16) => Instruction::I16x8Ne,
        (VectorCompareOp::NotEqual, 32) => Instruction::I32x4Ne,
        (VectorCompareOp::NotEqual, 64) => Instruction::I64x2Ne,
        (VectorCompareOp::LessUnsigned, 8) => Instruction::I8x16LtU,
        (VectorCompareOp::LessUnsigned, 16) => Instruction::I16x8LtU,
        (VectorCompareOp::LessUnsigned, 32) => Instruction::I32x4LtU,
        (VectorCompareOp::LessSigned, 8) => Instruction::I8x16LtS,
        (VectorCompareOp::LessSigned, 16) => Instruction::I16x8LtS,
        (VectorCompareOp::LessSigned, 32) => Instruction::I32x4LtS,
        (VectorCompareOp::LessSigned, 64) => Instruction::I64x2LtS,
        (VectorCompareOp::LessEqualUnsigned, 8) => Instruction::I8x16LeU,
        (VectorCompareOp::LessEqualUnsigned, 16) => Instruction::I16x8LeU,
        (VectorCompareOp::LessEqualUnsigned, 32) => Instruction::I32x4LeU,
        (VectorCompareOp::LessEqualSigned, 8) => Instruction::I8x16LeS,
        (VectorCompareOp::LessEqualSigned, 16) => Instruction::I16x8LeS,
        (VectorCompareOp::LessEqualSigned, 32) => Instruction::I32x4LeS,
        (VectorCompareOp::LessEqualSigned, 64) => Instruction::I64x2LeS,
        (VectorCompareOp::GreaterUnsigned, 8) => Instruction::I8x16GtU,
        (VectorCompareOp::GreaterUnsigned, 16) => Instruction::I16x8GtU,
        (VectorCompareOp::GreaterUnsigned, 32) => Instruction::I32x4GtU,
        (VectorCompareOp::GreaterSigned, 8) => Instruction::I8x16GtS,
        (VectorCompareOp::GreaterSigned, 16) => Instruction::I16x8GtS,
        (VectorCompareOp::GreaterSigned, 32) => Instruction::I32x4GtS,
        (VectorCompareOp::GreaterSigned, 64) => Instruction::I64x2GtS,
        (
            VectorCompareOp::LessUnsigned
            | VectorCompareOp::LessEqualUnsigned
            | VectorCompareOp::GreaterUnsigned,
            64,
        ) => Instruction::Unreachable,
        _ => unreachable!("complete direct integer comparison matrix"),
    };
    function.instruction(&instruction);
}

fn emit_vector_compare_bitmask(function: &mut Function, sew: u8) {
    function.instruction(&match sew {
        8 => Instruction::I8x16Bitmask,
        16 => Instruction::I16x8Bitmask,
        32 => Instruction::I32x4Bitmask,
        64 => Instruction::I64x2Bitmask,
        _ => unreachable!("validated vector SEW"),
    });
}

#[allow(clippy::too_many_arguments)]
fn emit_vector_compare_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    op: VectorCompareOp,
    destination: u8,
    source2: u8,
    operand: VectorOperand,
    sew: u8,
    temps: VectorTemps,
) {
    if !matches!(operand, VectorOperand::Vector(_)) {
        emit_vector_splat(function, layout, operand, sew, temps);
    }
    function.instruction(&Instruction::LocalGet(temps.span));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Eq);
    function.instruction(&Instruction::If(BlockType::Empty));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    emit_vector_register_address(function, state, source2, temps);
    function.instruction(&Instruction::V128Load(memarg(4, 0)));
    emit_vector_operand(function, state, operand, temps);
    emit_vector_compare_operator(function, op, sew);
    emit_vector_compare_bitmask(function, sew);
    function.instruction(&Instruction::LocalSet(temps.index));

    // Comparisons write packed predicate bits. Preserve every tail bit above
    // vl (valid for both tail-undisturbed and tail-agnostic policy) while
    // replacing exactly the active low bits from the SIMD lane bitmask.
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Const(state.vl_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Sub);
    function.instruction(&Instruction::LocalSet(temps.chunk));

    let destination_address = state.regs_base.wrapping_add(u32::from(destination) * 16);
    function.instruction(&Instruction::I32Const(destination_address as i32));
    function.instruction(&Instruction::I32Const(destination_address as i32));
    function.instruction(&Instruction::I32Load16U(memarg(1, 0)));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(-1));
    function.instruction(&Instruction::I32Xor);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::I32Or);
    function.instruction(&Instruction::I32Store16(memarg(1, 0)));
    function.instruction(&Instruction::Else);
    emit_vector_compare_wide_body(
        function,
        state,
        op,
        destination,
        source2,
        operand,
        sew,
        temps,
    );
    function.instruction(&Instruction::End);
}

#[allow(clippy::too_many_arguments)]
fn emit_vector_compare_wide_body(
    function: &mut Function,
    state: VectorStateLayout,
    op: VectorCompareOp,
    destination: u8,
    source2: u8,
    operand: VectorOperand,
    sew: u8,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I64Const(0));
    function.instruction(&Instruction::LocalSet(temps.address));
    function.instruction(&Instruction::I64Const(0));
    function.instruction(&Instruction::LocalSet(temps.stride));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    function.instruction(&Instruction::Loop(BlockType::Empty));

    emit_vector_register_address(function, state, source2, temps);
    function.instruction(&Instruction::V128Load(memarg(4, 0)));
    emit_vector_operand(function, state, operand, temps);
    emit_vector_compare_operator(function, op, sew);
    emit_vector_compare_bitmask(function, sew);
    function.instruction(&Instruction::I64ExtendI32U);
    function.instruction(&Instruction::LocalSet(temps.last));

    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(128 / i32::from(sew)));
    function.instruction(&Instruction::I32Mul);
    function.instruction(&Instruction::LocalTee(temps.index));
    function.instruction(&Instruction::I32Const(64));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::If(BlockType::Empty));
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I64ExtendI32U);
    function.instruction(&Instruction::I64Shl);
    function.instruction(&Instruction::I64Or);
    function.instruction(&Instruction::LocalSet(temps.address));
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::LocalGet(temps.stride));
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32Const(64));
    function.instruction(&Instruction::I32Sub);
    function.instruction(&Instruction::I64ExtendI32U);
    function.instruction(&Instruction::I64Shl);
    function.instruction(&Instruction::I64Or);
    function.instruction(&Instruction::LocalSet(temps.stride));
    function.instruction(&Instruction::End);

    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.chunk));
    function.instruction(&Instruction::LocalGet(temps.span));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);

    function.instruction(&Instruction::V128Const(0));
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I64x2ReplaceLane(0));
    function.instruction(&Instruction::LocalGet(temps.stride));
    function.instruction(&Instruction::I64x2ReplaceLane(1));
    function.instruction(&Instruction::LocalSet(temps.result));
    emit_vector_active_bits_mask(function, state, temps);

    let destination_address = state.regs_base.wrapping_add(u32::from(destination) * 16);
    function.instruction(&Instruction::I32Const(destination_address as i32));
    function.instruction(&Instruction::LocalGet(temps.result));
    function.instruction(&Instruction::I32Const(destination_address as i32));
    function.instruction(&Instruction::V128Load(memarg(4, 0)));
    function.instruction(&Instruction::LocalGet(temps.splat));
    function.instruction(&Instruction::V128Bitselect);
    function.instruction(&Instruction::V128Store(memarg(4, 0)));
}

/// Construct a packed 128-bit mask with one bits for architectural element
/// indices below vl. The direct mask-logic path uses it to preserve all tail
/// predicate bits instead of relying on a particular tail-agnostic value.
fn emit_vector_active_bits_mask(
    function: &mut Function,
    state: VectorStateLayout,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(state.vl_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalTee(temps.last));
    function.instruction(&Instruction::I64Const(64));
    function.instruction(&Instruction::I64GeU);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
    function.instruction(&Instruction::I64Const(-1));
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::I64Const(1));
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Shl);
    function.instruction(&Instruction::I64Const(1));
    function.instruction(&Instruction::I64Sub);
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::LocalSet(temps.address));

    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Const(64));
    function.instruction(&Instruction::I64LeU);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
    function.instruction(&Instruction::I64Const(0));
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Const(128));
    function.instruction(&Instruction::I64GeU);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
    function.instruction(&Instruction::I64Const(-1));
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::I64Const(1));
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Const(64));
    function.instruction(&Instruction::I64Sub);
    function.instruction(&Instruction::I64Shl);
    function.instruction(&Instruction::I64Const(1));
    function.instruction(&Instruction::I64Sub);
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::LocalSet(temps.stride));

    function.instruction(&Instruction::V128Const(0));
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I64x2ReplaceLane(0));
    function.instruction(&Instruction::LocalGet(temps.stride));
    function.instruction(&Instruction::I64x2ReplaceLane(1));
    function.instruction(&Instruction::LocalSet(temps.splat));
}

fn emit_vector_mask_logic_body(
    function: &mut Function,
    state: VectorStateLayout,
    op: VectorMaskOp,
    destination: u8,
    source2: u8,
    source1: u8,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(
        state.regs_base.wrapping_add(u32::from(source2) * 16) as i32,
    ));
    function.instruction(&Instruction::V128Load(memarg(4, 0)));
    function.instruction(&Instruction::I32Const(
        state.regs_base.wrapping_add(u32::from(source1) * 16) as i32,
    ));
    function.instruction(&Instruction::V128Load(memarg(4, 0)));
    match op {
        VectorMaskOp::AndNot => {
            function.instruction(&Instruction::V128Not);
            function.instruction(&Instruction::V128And);
        }
        VectorMaskOp::And => {
            function.instruction(&Instruction::V128And);
        }
        VectorMaskOp::Or => {
            function.instruction(&Instruction::V128Or);
        }
        VectorMaskOp::Xor => {
            function.instruction(&Instruction::V128Xor);
        }
        VectorMaskOp::OrNot => {
            function.instruction(&Instruction::V128Not);
            function.instruction(&Instruction::V128Or);
        }
        VectorMaskOp::Nand => {
            function.instruction(&Instruction::V128And);
            function.instruction(&Instruction::V128Not);
        }
        VectorMaskOp::Nor => {
            function.instruction(&Instruction::V128Or);
            function.instruction(&Instruction::V128Not);
        }
        VectorMaskOp::Xnor => {
            function.instruction(&Instruction::V128Xor);
            function.instruction(&Instruction::V128Not);
        }
    }
    function.instruction(&Instruction::LocalSet(temps.result));
    emit_vector_active_bits_mask(function, state, temps);

    let destination_address = state.regs_base.wrapping_add(u32::from(destination) * 16);
    function.instruction(&Instruction::I32Const(destination_address as i32));
    function.instruction(&Instruction::LocalGet(temps.result));
    function.instruction(&Instruction::I32Const(destination_address as i32));
    function.instruction(&Instruction::V128Load(memarg(4, 0)));
    function.instruction(&Instruction::LocalGet(temps.splat));
    function.instruction(&Instruction::V128Bitselect);
    function.instruction(&Instruction::V128Store(memarg(4, 0)));
}

fn emit_vector_linear_address(function: &mut Function, temps: VectorTemps) {
    function.instruction(&Instruction::LocalGet(temps.linear));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(4));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Add);
}

fn emit_vector_fractional_unit_stride(
    function: &mut Function,
    state: VectorStateLayout,
    load: bool,
    register: u8,
    bytes: i32,
    temps: VectorTemps,
) {
    if load {
        emit_vector_register_address(function, state, register, temps);
        emit_vector_linear_address(function, temps);
    } else {
        emit_vector_linear_address(function, temps);
        emit_vector_register_address(function, state, register, temps);
    }
    match bytes {
        2 => {
            function.instruction(&Instruction::I32Load16U(memarg(1, 0)));
            function.instruction(&Instruction::I32Store16(memarg(1, 0)));
        }
        4 => {
            function.instruction(&Instruction::I32Load(memarg(2, 0)));
            function.instruction(&Instruction::I32Store(memarg(2, 0)));
        }
        8 => {
            function.instruction(&Instruction::I64Load(memarg(3, 0)));
            function.instruction(&Instruction::I64Store(memarg(3, 0)));
        }
        _ => unreachable!("ratified fractional LMUL byte width"),
    }
}

fn emit_vector_unit_stride_body(
    function: &mut Function,
    state: VectorStateLayout,
    load: bool,
    register: u8,
    temps: VectorTemps,
    fractional_lmul: bool,
) {
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));

    if !fractional_lmul {
        function.instruction(&Instruction::Loop(BlockType::Empty));
        if load {
            emit_vector_register_address(function, state, register, temps);
            emit_vector_linear_address(function, temps);
            function.instruction(&Instruction::V128Load(memarg(4, 0)));
        } else {
            emit_vector_linear_address(function, temps);
            emit_vector_register_address(function, state, register, temps);
            function.instruction(&Instruction::V128Load(memarg(4, 0)));
        }
        function.instruction(&Instruction::V128Store(memarg(4, 0)));

        function.instruction(&Instruction::LocalGet(temps.chunk));
        function.instruction(&Instruction::I32Const(1));
        function.instruction(&Instruction::I32Add);
        function.instruction(&Instruction::LocalTee(temps.chunk));
        function.instruction(&Instruction::LocalGet(temps.span));
        function.instruction(&Instruction::I32LtU);
        function.instruction(&Instruction::BrIf(0));
        function.instruction(&Instruction::End);
        return;
    }

    function.instruction(&Instruction::LocalGet(temps.group_bytes));
    function.instruction(&Instruction::I32Const(8));
    function.instruction(&Instruction::I32Eq);
    function.instruction(&Instruction::If(BlockType::Empty));
    emit_vector_fractional_unit_stride(function, state, load, register, 8, temps);
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::LocalGet(temps.group_bytes));
    function.instruction(&Instruction::I32Const(4));
    function.instruction(&Instruction::I32Eq);
    function.instruction(&Instruction::If(BlockType::Empty));
    emit_vector_fractional_unit_stride(function, state, load, register, 4, temps);
    function.instruction(&Instruction::Else);
    emit_vector_fractional_unit_stride(function, state, load, register, 2, temps);
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);
}

/// Emit a unit-stride transfer whose exact effective LMUL was proved by a
/// guarded vector-configuration fact. Static register chunks remove the
/// dynamic inner loop without changing architectural memory ordering.
fn emit_known_vector_unit_stride_body(
    function: &mut Function,
    state: VectorStateLayout,
    load: bool,
    register: u8,
    memory: KnownVectorMemoryConfig,
    temps: VectorTemps,
) {
    let register_address = |chunk: u8| {
        state
            .regs_base
            .wrapping_add(u32::from(register.saturating_add(chunk)) * 16)
    };
    let emit_linear_address = |function: &mut Function, offset: i32| {
        function.instruction(&Instruction::LocalGet(temps.linear));
        if offset != 0 {
            function.instruction(&Instruction::I32Const(offset));
            function.instruction(&Instruction::I32Add);
        }
    };

    if memory.fractional_lmul {
        if load {
            function.instruction(&Instruction::I32Const(register_address(0) as i32));
            emit_linear_address(function, 0);
        } else {
            emit_linear_address(function, 0);
            function.instruction(&Instruction::I32Const(register_address(0) as i32));
        }
        let (load_instruction, store_instruction) = match memory.group_bytes {
            2 => (
                Instruction::I32Load16U(memarg(1, 0)),
                Instruction::I32Store16(memarg(1, 0)),
            ),
            4 => (
                Instruction::I32Load(memarg(2, 0)),
                Instruction::I32Store(memarg(2, 0)),
            ),
            8 => (
                Instruction::I64Load(memarg(3, 0)),
                Instruction::I64Store(memarg(3, 0)),
            ),
            _ => unreachable!("ratified fractional LMUL byte width"),
        };
        function.instruction(&load_instruction);
        function.instruction(&store_instruction);
        return;
    }

    for chunk in 0..memory.span {
        let offset = i32::from(chunk) * 16;
        if load {
            function.instruction(&Instruction::I32Const(register_address(chunk) as i32));
            emit_linear_address(function, offset);
        } else {
            emit_linear_address(function, offset);
            function.instruction(&Instruction::I32Const(register_address(chunk) as i32));
        }
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
        function.instruction(&Instruction::V128Store(memarg(4, 0)));
    }
}

fn emit_vector_strided_linear_address(function: &mut Function, temps: VectorTemps) {
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::LocalGet(temps.stride));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I64ExtendI32U);
    function.instruction(&Instruction::I64Mul);
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::LocalTee(temps.element));
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64LtU);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    function.instruction(&Instruction::LocalGet(temps.linear));
    function.instruction(&Instruction::LocalGet(temps.element));
    function.instruction(&Instruction::LocalGet(temps.address));
    function.instruction(&Instruction::I64Sub);
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::LocalGet(temps.linear2));
    function.instruction(&Instruction::LocalGet(temps.element));
    function.instruction(&Instruction::LocalGet(temps.last));
    function.instruction(&Instruction::I64Sub);
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::End);
}

fn emit_vector_strided_register_address(
    function: &mut Function,
    state: VectorStateLayout,
    register: u8,
    width: u8,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(
        state.regs_base.wrapping_add(u32::from(register) * 16) as i32,
    ));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const((width / 8).trailing_zeros() as i32));
    function.instruction(&Instruction::I32Shl);
    function.instruction(&Instruction::I32Add);
}

fn emit_vector_strided_scalar_transfer(function: &mut Function, width: u8) {
    let (load, store) = match width {
        8 => (
            Instruction::I64Load8U(memarg(0, 0)),
            Instruction::I64Store8(memarg(0, 0)),
        ),
        16 => (
            Instruction::I64Load16U(memarg(1, 0)),
            Instruction::I64Store16(memarg(1, 0)),
        ),
        32 => (
            Instruction::I64Load32U(memarg(2, 0)),
            Instruction::I64Store32(memarg(2, 0)),
        ),
        64 => (
            Instruction::I64Load(memarg(3, 0)),
            Instruction::I64Store(memarg(3, 0)),
        ),
        _ => unreachable!("decoded strided width"),
    };
    function.instruction(&load);
    function.instruction(&store);
}

fn emit_vector_strided_body(
    function: &mut Function,
    state: VectorStateLayout,
    load: bool,
    width: u8,
    register: u8,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(state.vl_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::LocalSet(temps.index));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    function.instruction(&Instruction::Loop(BlockType::Empty));
    if load {
        emit_vector_strided_register_address(function, state, register, width, temps);
        emit_vector_strided_linear_address(function, temps);
    } else {
        emit_vector_strided_linear_address(function, temps);
        emit_vector_strided_register_address(function, state, register, width, temps);
    }
    emit_vector_strided_scalar_transfer(function, width);
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.chunk));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);
}

/// Execute an ordinary masked vector load/store element by element after the
/// caller has proved the complete address range and effective register group.
/// Inactive loads preserve their destination element (an allowed agnostic
/// result), while inactive stores perform no memory access at all.
#[allow(clippy::too_many_arguments)]
fn emit_vector_masked_memory_body(
    function: &mut Function,
    state: VectorStateLayout,
    load: bool,
    strided: bool,
    width: u8,
    register: u8,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(state.vl_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I32WrapI64);
    function.instruction(&Instruction::LocalSet(temps.index));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    function.instruction(&Instruction::Loop(BlockType::Empty));

    // Packed predicate bit v0[element]. The exact known configuration bounds
    // element below VLMAX, hence this byte read remains inside v0's 16 bytes.
    function.instruction(&Instruction::I32Const(state.regs_base as i32));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(3));
    function.instruction(&Instruction::I32ShrU);
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::I32Load8U(memarg(0, 0)));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(7));
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::I32ShrU);
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::If(BlockType::Empty));

    if load {
        emit_vector_strided_register_address(function, state, register, width, temps);
        if strided {
            emit_vector_strided_linear_address(function, temps);
        } else {
            function.instruction(&Instruction::LocalGet(temps.linear));
            function.instruction(&Instruction::LocalGet(temps.chunk));
            function.instruction(&Instruction::I32Const((width / 8).trailing_zeros() as i32));
            function.instruction(&Instruction::I32Shl);
            function.instruction(&Instruction::I32Add);
        }
    } else {
        if strided {
            emit_vector_strided_linear_address(function, temps);
        } else {
            function.instruction(&Instruction::LocalGet(temps.linear));
            function.instruction(&Instruction::LocalGet(temps.chunk));
            function.instruction(&Instruction::I32Const((width / 8).trailing_zeros() as i32));
            function.instruction(&Instruction::I32Shl);
            function.instruction(&Instruction::I32Add);
        }
        emit_vector_strided_register_address(function, state, register, width, temps);
    }
    emit_vector_strided_scalar_transfer(function, width);
    function.instruction(&Instruction::End);

    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.chunk));
    function.instruction(&Instruction::LocalGet(temps.index));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);
}

fn emit_vector_system_enabled_guard(function: &mut Function, layout: JitLayout) {
    if layout.vector == Some(VectorCapability::System) {
        function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64Const(3 << 9));
        function.instruction(&Instruction::I64And);
        function.instruction(&Instruction::I64Eqz);
        function.instruction(&Instruction::I32Eqz);
        function.instruction(&Instruction::I32And);
    }
}

fn emit_vector_whole_memory_guard(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    load: bool,
    register: u8,
    base: u8,
    registers: u8,
    temps: VectorTemps,
) {
    // Whole-register transfers deliberately work with VILL and ignore vl and
    // vtype. Restrict direct execution to a fresh instruction so faults can
    // retain precise element-index vstart in the helper.
    function.instruction(&Instruction::I32Const(state.vstart_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Eqz);
    emit_vector_memory_guard(
        function,
        layout,
        load,
        None,
        register,
        base,
        temps,
        false,
        Some(u32::from(registers) * 16),
        false,
        false,
    );
    emit_vector_system_enabled_guard(function, layout);
}

fn emit_vector_whole_move_guard(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    temps: VectorTemps,
) {
    // Whole-register moves ignore vl and LMUL grouping but still require a
    // valid vector configuration because vstart is expressed in SEW elements.
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(!0xffi64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(7));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(4));
    function.instruction(&Instruction::I64Ne);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(3));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Const(7));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(3));
    function.instruction(&Instruction::I64LeU);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::I32Const(state.vstart_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::I32And);
    emit_vector_system_enabled_guard(function, layout);
}

fn emit_vector_valid_config_guard(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(!0xffi64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(7));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(4));
    function.instruction(&Instruction::I64Ne);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(3));
    function.instruction(&Instruction::I64ShrU);
    function.instruction(&Instruction::I64Const(7));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Const(3));
    function.instruction(&Instruction::I64LeU);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::I32Const(state.vstart_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::I32And);
    emit_vector_system_enabled_guard(function, layout);
}

fn emit_vector_scalar_insert_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    destination: u8,
    source: u8,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(state.vl_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::I32Eqz);
    function.instruction(&Instruction::If(BlockType::Empty));
    for (vsew, store) in [
        (0, Instruction::I64Store8(memarg(0, 0))),
        (1, Instruction::I64Store16(memarg(1, 0))),
        (2, Instruction::I64Store32(memarg(2, 0))),
        (3, Instruction::I64Store(memarg(3, 0))),
    ] {
        function.instruction(&Instruction::LocalGet(temps.vtype));
        function.instruction(&Instruction::I64Const(3));
        function.instruction(&Instruction::I64ShrU);
        function.instruction(&Instruction::I64Const(7));
        function.instruction(&Instruction::I64And);
        function.instruction(&Instruction::I64Const(vsew));
        function.instruction(&Instruction::I64Eq);
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::I32Const(
            state.regs_base.wrapping_add(u32::from(destination) * 16) as i32,
        ));
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        function.instruction(&Instruction::I64Load(memarg(3, u64::from(source) * 8)));
        function.instruction(&store);
        function.instruction(&Instruction::End);
    }
    function.instruction(&Instruction::End);
    emit_vector_direct_finish(function, layout, state);
}

fn emit_vector_scalar_extract_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    destination: u8,
    source: u8,
    temps: VectorTemps,
) {
    if destination != 0 {
        for (vsew, load) in [
            (0, Instruction::I64Load8S(memarg(0, 0))),
            (1, Instruction::I64Load16S(memarg(1, 0))),
            (2, Instruction::I64Load32S(memarg(2, 0))),
            (3, Instruction::I64Load(memarg(3, 0))),
        ] {
            function.instruction(&Instruction::LocalGet(temps.vtype));
            function.instruction(&Instruction::I64Const(3));
            function.instruction(&Instruction::I64ShrU);
            function.instruction(&Instruction::I64Const(7));
            function.instruction(&Instruction::I64And);
            function.instruction(&Instruction::I64Const(vsew));
            function.instruction(&Instruction::I64Eq);
            function.instruction(&Instruction::If(BlockType::Empty));
            function.instruction(&Instruction::I32Const(layout.x_base as i32));
            function.instruction(&Instruction::I32Const(
                state.regs_base.wrapping_add(u32::from(source) * 16) as i32,
            ));
            function.instruction(&load);
            function.instruction(&Instruction::I64Store(memarg(
                3,
                u64::from(destination) * 8,
            )));
            function.instruction(&Instruction::End);
        }
    }
    emit_profile_counter_add(function, state.simd_count_addr, 1);
}

fn emit_vector_whole_memory_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    load: bool,
    register: u8,
    registers: u8,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    function.instruction(&Instruction::Loop(BlockType::Empty));
    if load {
        emit_vector_register_address(function, state, register, temps);
        emit_vector_linear_address(function, temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
    } else {
        emit_vector_linear_address(function, temps);
        emit_vector_register_address(function, state, register, temps);
        function.instruction(&Instruction::V128Load(memarg(4, 0)));
    }
    function.instruction(&Instruction::V128Store(memarg(4, 0)));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.chunk));
    function.instruction(&Instruction::I32Const(i32::from(registers)));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);
    emit_vector_direct_finish(function, layout, state);
}

fn emit_vector_whole_move_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    destination: u8,
    source: u8,
    registers: u8,
    temps: VectorTemps,
) {
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(temps.chunk));
    function.instruction(&Instruction::Loop(BlockType::Empty));
    emit_vector_register_address(function, state, destination, temps);
    emit_vector_register_address(function, state, source, temps);
    function.instruction(&Instruction::V128Load(memarg(4, 0)));
    function.instruction(&Instruction::V128Store(memarg(4, 0)));
    function.instruction(&Instruction::LocalGet(temps.chunk));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(temps.chunk));
    function.instruction(&Instruction::I32Const(i32::from(registers)));
    function.instruction(&Instruction::I32LtU);
    function.instruction(&Instruction::BrIf(0));
    function.instruction(&Instruction::End);
    emit_vector_direct_finish(function, layout, state);
}

fn emit_vector_direct_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    direct: VectorDirect,
    temps: VectorTemps,
    fractional_lmul: bool,
    tail_merge: bool,
) {
    match direct {
        VectorDirect::Lane {
            op,
            masked,
            destination,
            source2,
            operand,
        } => {
            for (vsew, sew) in [(0, 8), (1, 16), (2, 32), (3, 64)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_lane_body(
                    function,
                    layout,
                    state,
                    op,
                    masked,
                    destination,
                    source2,
                    operand,
                    sew,
                    temps,
                    fractional_lmul,
                    tail_merge,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::Reduction {
            op,
            destination,
            source,
            seed,
        } => {
            for (vsew, sew) in [(0, 8), (1, 16), (2, 32), (3, 64)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_reduction_body(
                    function,
                    state,
                    op,
                    destination,
                    source,
                    seed,
                    sew,
                    temps,
                    fractional_lmul,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::MaskLogic {
            op,
            destination,
            source2,
            source1,
        } => emit_vector_mask_logic_body(function, state, op, destination, source2, source1, temps),
        VectorDirect::WidenAddSub {
            signed,
            subtract,
            wide_left,
            destination,
            source2,
            operand,
        } => {
            for (vsew, sew) in [(0, 8), (1, 16), (2, 32)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_widen_add_sub_body(
                    function,
                    layout,
                    state,
                    signed,
                    subtract,
                    wide_left,
                    destination,
                    source2,
                    operand,
                    sew,
                    temps,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::WidenMultiplyAccumulate {
            source2_signed,
            operand_signed,
            destination,
            source2,
            operand,
        } => {
            for (vsew, sew) in [(0, 8), (1, 16), (2, 32)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_widen_madd_body(
                    function,
                    layout,
                    state,
                    source2_signed,
                    operand_signed,
                    destination,
                    source2,
                    operand,
                    sew,
                    temps,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::UnitStride {
            load,
            masked,
            width,
            register,
            ..
        } => {
            if masked {
                emit_vector_masked_memory_body(
                    function, state, load, false, width, register, temps,
                );
            } else {
                emit_vector_unit_stride_body(
                    function,
                    state,
                    load,
                    register,
                    temps,
                    fractional_lmul,
                );
            }
        }
        VectorDirect::Strided {
            load,
            masked,
            width,
            register,
            ..
        } => {
            if masked {
                emit_vector_masked_memory_body(function, state, load, true, width, register, temps);
            } else {
                emit_vector_strided_body(function, state, load, width, register, temps);
            }
        }
        VectorDirect::SlideImmediate {
            up,
            destination,
            source,
            offset,
        } => {
            for (vsew, sew) in [(0, 8), (1, 16), (2, 32), (3, 64)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_slide_immediate_body(
                    function,
                    state,
                    up,
                    destination,
                    source,
                    offset,
                    sew,
                    temps,
                    fractional_lmul,
                    tail_merge,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::SlideOne {
            up,
            destination,
            source,
            scalar,
        } => {
            for (vsew, sew) in [(0, 8), (1, 16), (2, 32), (3, 64)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_slide_one_body(
                    function,
                    layout,
                    state,
                    up,
                    destination,
                    source,
                    scalar,
                    sew,
                    temps,
                    fractional_lmul,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::GatherImmediate {
            destination,
            source,
            index,
        } => {
            for (vsew, sew) in [(0, 8), (1, 16), (2, 32), (3, 64)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_gather_immediate_body(
                    function,
                    state,
                    destination,
                    source,
                    index,
                    sew,
                    temps,
                    fractional_lmul,
                    tail_merge,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::GatherVector {
            destination,
            source,
            indices,
        } => {
            for (vsew, sew) in [(0, 8), (1, 16), (2, 32), (3, 64)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_gather_vector_body(
                    function,
                    state,
                    destination,
                    source,
                    indices,
                    sew,
                    temps,
                    fractional_lmul,
                    tail_merge,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::Index { destination } => {
            for (vsew, sew) in [(0, 8), (1, 16), (2, 32), (3, 64)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_index_body(
                    function,
                    state,
                    destination,
                    sew,
                    temps,
                    fractional_lmul,
                    tail_merge,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::FloatSign {
            op,
            destination,
            source2,
            source1,
        } => {
            for (vsew, sew) in [(2, 32), (3, 64)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_float_sign_body(
                    function,
                    state,
                    op,
                    destination,
                    source2,
                    source1,
                    sew,
                    temps,
                    fractional_lmul,
                );
                function.instruction(&Instruction::End);
            }
            emit_vector_system_fp_dirty(function, layout);
        }
        VectorDirect::FloatBroadcast {
            destination,
            source,
        } => {
            for (vsew, sew) in [(2, 32), (3, 64)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_float_broadcast_body(
                    function,
                    layout,
                    state,
                    destination,
                    source,
                    sew,
                    temps,
                    fractional_lmul,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::FloatSlideOne {
            up,
            destination,
            source,
            scalar,
        } => {
            for (vsew, sew) in [(2, 32), (3, 64)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_float_slide_one_body(
                    function,
                    layout,
                    state,
                    up,
                    destination,
                    source,
                    scalar,
                    sew,
                    temps,
                    fractional_lmul,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::Compare {
            op,
            destination,
            source2,
            operand,
        } => {
            for (vsew, sew) in [(0, 8), (1, 16), (2, 32), (3, 64)] {
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I64Const(3));
                function.instruction(&Instruction::I64ShrU);
                function.instruction(&Instruction::I64Const(7));
                function.instruction(&Instruction::I64And);
                function.instruction(&Instruction::I64Const(vsew));
                function.instruction(&Instruction::I64Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                emit_vector_compare_body(
                    function,
                    layout,
                    state,
                    op,
                    destination,
                    source2,
                    operand,
                    sew,
                    temps,
                );
                function.instruction(&Instruction::End);
            }
        }
        VectorDirect::ConfigImmediate {
            destination,
            vtype,
            vl,
        } => emit_vector_config_immediate_body(
            function, layout, state, destination, vtype, vl,
        ),
        VectorDirect::ConfigRetainFull { vtype, vlmax } => {
            // vsetvli x0, x0, vtype: nothing to do once the guard has proved
            // the current and new configs share VLMAX, since both vtype and
            // vl already equal the requested values. Re-write the same values
            // so emit_vector_direct_finish's dirty flags + counters stay
            // consistent with every other config-touching instruction.
            emit_vector_config_immediate_body(function, layout, state, 0, vtype, vlmax);
        }
        VectorDirect::WholeRegisterMemory {
            load,
            register,
            registers,
            ..
        } => emit_vector_whole_memory_body(
            function, layout, state, load, register, registers, temps,
        ),
        VectorDirect::WholeRegisterMove {
            destination,
            source,
            registers,
        } => emit_vector_whole_move_body(
            function, layout, state, destination, source, registers, temps,
        ),
        VectorDirect::ScalarInsert { destination, source } => {
            emit_vector_scalar_insert_body(function, layout, state, destination, source, temps);
        }
        VectorDirect::ScalarExtract { destination, source } => {
            emit_vector_scalar_extract_body(function, layout, state, destination, source, temps);
        }
        VectorDirect::FloatScalarInsert { destination, source } => {
            emit_vector_float_scalar_insert_body(
                function, layout, state, destination, source, temps,
            );
        }
        VectorDirect::FloatScalarExtract { destination, source } => {
            emit_vector_float_scalar_extract_body(
                function, layout, state, destination, source, temps,
            );
        }
    }

    emit_vector_direct_finish(function, layout, state);
}

fn emit_vector_direct_finish(function: &mut Function, layout: JitLayout, state: VectorStateLayout) {
    emit_vector_system_dirty(function, layout);
    emit_profile_counter_add(function, state.simd_count_addr, 1);
}

fn emit_vector_config_enabled_guard(function: &mut Function, layout: JitLayout) {
    match layout.vector {
        Some(VectorCapability::User) => {
            function.instruction(&Instruction::I32Const(1));
        }
        Some(VectorCapability::System) => {
            function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
            function.instruction(&Instruction::I64Load(memarg(3, 0)));
            function.instruction(&Instruction::I64Const(3 << 9));
            function.instruction(&Instruction::I64And);
            function.instruction(&Instruction::I64Eqz);
            function.instruction(&Instruction::I32Eqz);
        }
        None => {
            function.instruction(&Instruction::I32Const(0));
        }
    }
}

fn emit_vector_retain_full_guard(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    temps: VectorTemps,
    vlmax: u64,
) {
    emit_vector_config_enabled_guard(function, layout);
    function.instruction(&Instruction::I32Const(state.vtype_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalSet(temps.vtype));

    // Current vtype must be legal. Policy bits do not affect VLMAX, while
    // every high/reserved bit (including VILL) rejects the direct arm.
    function.instruction(&Instruction::LocalGet(temps.vtype));
    function.instruction(&Instruction::I64Const(!0xffi64));
    function.instruction(&Instruction::I64And);
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::I32And);

    // Enumerate the architectural SEW/LMUL encodings with the required
    // VLMAX. This is a fixed ISA table, not a guest-code or workload pattern.
    function.instruction(&Instruction::I32Const(0));
    for raw_vtype in 0u64..64 {
        if KnownVectorConfig::decode(raw_vtype, 0).is_some_and(|config| config.vlmax == vlmax) {
            function.instruction(&Instruction::LocalGet(temps.vtype));
            function.instruction(&Instruction::I64Const(0x3f));
            function.instruction(&Instruction::I64And);
            function.instruction(&Instruction::I64Const(raw_vtype as i64));
            function.instruction(&Instruction::I64Eq);
            function.instruction(&Instruction::I32Or);
        }
    }
    function.instruction(&Instruction::I32And);

    function.instruction(&Instruction::I32Const(state.vl_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Const(vlmax as i64));
    function.instruction(&Instruction::I64Eq);
    function.instruction(&Instruction::I32And);
}

fn emit_vector_config_immediate_body(
    function: &mut Function,
    layout: JitLayout,
    state: VectorStateLayout,
    destination: u8,
    vtype: u64,
    vl: u64,
) {
    for (address, value) in [
        (state.vtype_addr, vtype),
        (state.vl_addr, vl),
        (state.vstart_addr, 0),
    ] {
        function.instruction(&Instruction::I32Const(address as i32));
        function.instruction(&Instruction::I64Const(value as i64));
        function.instruction(&Instruction::I64Store(memarg(3, 0)));
    }
    if destination != 0 {
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        function.instruction(&Instruction::I64Const(vl as i64));
        function.instruction(&Instruction::I64Store(memarg(
            3,
            u64::from(destination) * 8,
        )));
    }
    emit_vector_direct_finish(function, layout, state);
}

fn emit_vector_system_dirty(function: &mut Function, layout: JitLayout) {
    if layout.vector == Some(VectorCapability::System) {
        function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
        function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64Const(3 << 9));
        function.instruction(&Instruction::I64Or);
        function.instruction(&Instruction::I64Store(memarg(3, 0)));
    }
}

fn emit_vector_system_fp_enabled_guard(function: &mut Function, layout: JitLayout) {
    if layout.vector == Some(VectorCapability::System) {
        function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64Const(3 << 13));
        function.instruction(&Instruction::I64And);
        function.instruction(&Instruction::I64Eqz);
        function.instruction(&Instruction::I32Eqz);
        function.instruction(&Instruction::I32And);
    }
}

fn emit_vector_system_fp_dirty(function: &mut Function, layout: JitLayout) {
    if layout.vector == Some(VectorCapability::System) {
        function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
        function.instruction(&Instruction::I32Const(layout.mstatus_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64Const(3 << 13));
        function.instruction(&Instruction::I64Or);
        function.instruction(&Instruction::I64Store(memarg(3, 0)));
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_vector_direct_class(
    function: &mut Function,
    layout: JitLayout,
    helpers: HelperImports,
    insn: u32,
    state: VectorStateLayout,
    direct: VectorDirect,
    temps: VectorTemps,
    fractional_lmul: bool,
    prepare_fallback: &mut dyn FnMut(&mut Function) -> Result<(), EmitError>,
    address_preloaded: bool,
) -> Result<(), EmitError> {
    emit_vector_direct_guard(
        function,
        layout,
        state,
        direct,
        temps,
        fractional_lmul,
        true,
        address_preloaded,
    );
    function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    emit_vector_direct_body(
        function,
        layout,
        state,
        direct,
        temps,
        fractional_lmul,
        false,
    );
    function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
    function.instruction(&Instruction::Else);
    emit_vector_partial_class(
        function,
        layout,
        helpers,
        insn,
        state,
        direct,
        temps,
        fractional_lmul,
        prepare_fallback,
        address_preloaded,
    )?;
    function.instruction(&Instruction::End);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_vector_partial_class(
    function: &mut Function,
    layout: JitLayout,
    helpers: HelperImports,
    insn: u32,
    state: VectorStateLayout,
    direct: VectorDirect,
    temps: VectorTemps,
    fractional_lmul: bool,
    prepare_fallback: &mut dyn FnMut(&mut Function) -> Result<(), EmitError>,
    address_preloaded: bool,
) -> Result<(), EmitError> {
    if !vector_partial_vl_available(direct) {
        emit_vector_call(function, helpers, insn, prepare_fallback)?;
        return Ok(());
    }
    emit_vector_direct_guard(
        function,
        layout,
        state,
        direct,
        temps,
        fractional_lmul,
        false,
        address_preloaded,
    );
    function.instruction(&Instruction::LocalGet(temps.span));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Eq);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::I32Const(state.vl_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Eqz);
    function.instruction(&Instruction::I32Eqz);
    function.instruction(&Instruction::I32And);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    emit_vector_direct_body(
        function,
        layout,
        state,
        direct,
        temps,
        fractional_lmul,
        true,
    );
    function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
    function.instruction(&Instruction::Else);
    emit_vector_call(function, helpers, insn, prepare_fallback)?;
    function.instruction(&Instruction::End);
    Ok(())
}

fn emit_known_vector_execution(
    function: &mut Function,
    layout: JitLayout,
    helpers: HelperImports,
    insn: u32,
    direct: VectorDirect,
    config: KnownVectorConfig,
    tail_merge: bool,
    temps: Option<VectorTemps>,
    status: u32,
    prepare_fallback: &mut dyn FnMut(&mut Function) -> Result<(), EmitError>,
    unit_shape_proven: bool,
    address_preloaded: bool,
) -> Result<(), EmitError> {
    let state = layout
        .vector_state
        .ok_or_else(|| EmitError("known direct vector effect lacks architectural state".into()))?;
    let temps = temps
        .ok_or_else(|| EmitError("known direct vector effect lacks shared temporaries".into()))?;
    if unit_shape_proven && !matches!(direct, VectorDirect::UnitStride { masked: false, .. }) {
        return Err(EmitError(
            "unit-shape proof applied to a non-unit vector effect".into(),
        ));
    }
    if !unit_shape_proven {
        function.instruction(&Instruction::I64Const(config.vtype as i64));
        function.instruction(&Instruction::LocalSet(temps.vtype));
        function.instruction(&Instruction::I32Const(i32::from(config.span)));
        function.instruction(&Instruction::LocalSet(temps.span));
        function.instruction(&Instruction::I32Const(i32::from(config.group_bytes)));
        function.instruction(&Instruction::LocalSet(temps.group_bytes));
    }

    if let VectorDirect::UnitStride {
        load,
        width,
        register,
        base,
        ..
    }
    | VectorDirect::Strided {
        load,
        width,
        register,
        base,
        ..
    } = direct
    {
        let memory = config.memory_config(width).ok_or_else(|| {
            EmitError("known vector memory effect has an illegal effective LMUL".into())
        })?;
        if let Some(system_memory) = layout.sys {
            validate_system_memory(system_memory)?;
        }
        if !unit_shape_proven {
            function.instruction(&Instruction::I32Const(i32::from(memory.span)));
            function.instruction(&Instruction::LocalSet(temps.span));
            function.instruction(&Instruction::I32Const(i32::from(memory.group_bytes)));
            function.instruction(&Instruction::LocalSet(temps.group_bytes));
        }

        function.instruction(&Instruction::I32Const(1));
        match direct {
            VectorDirect::UnitStride { .. } => emit_vector_memory_guard(
                function,
                layout,
                load,
                None,
                register,
                base,
                temps,
                memory.fractional_lmul,
                Some(u32::from(memory.group_bytes)),
                address_preloaded,
                unit_shape_proven && load && address_preloaded,
            ),
            VectorDirect::Strided { stride, .. } => emit_vector_strided_memory_guard(
                function, layout, state, load, width, register, base, stride, temps, false,
            ),
            _ => unreachable!("matched known vector memory operation"),
        }
        if !unit_shape_proven {
            emit_vector_system_enabled_guard(function, layout);
        }
        function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        if let VectorDirect::UnitStride {
            load,
            masked: false,
            register,
            ..
        } = direct
        {
            emit_known_vector_unit_stride_body(function, state, load, register, memory, temps);
            if unit_shape_proven {
                emit_profile_counter_add(function, state.simd_count_addr, 1);
            } else {
                emit_vector_direct_finish(function, layout, state);
            }
        } else {
            emit_vector_direct_body(
                function,
                layout,
                state,
                direct,
                temps,
                memory.fractional_lmul,
                false,
            );
        }
        function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
        function.instruction(&Instruction::Else);
        emit_vector_call(function, helpers, insn, prepare_fallback)?;
        function.instruction(&Instruction::End);
        function.instruction(&Instruction::LocalSet(status));
        return Ok(());
    }

    emit_vector_direct_body(
        function,
        layout,
        state,
        direct,
        temps,
        config.fractional_lmul,
        tail_merge,
    );
    function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
    function.instruction(&Instruction::LocalSet(status));
    Ok(())
}

fn emit_known_vector_config_execution(
    function: &mut Function,
    layout: JitLayout,
    config: KnownVectorConfig,
    status: u32,
) -> Result<(), EmitError> {
    let state = layout
        .vector_state
        .ok_or_else(|| EmitError("known vector configuration lacks architectural state".into()))?;
    emit_vector_config_immediate_body(function, layout, state, 0, config.vtype, config.vl);
    function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
    function.instruction(&Instruction::LocalSet(status));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_vector_execution(
    function: &mut Function,
    layout: JitLayout,
    helpers: HelperImports,
    insn: u32,
    direct: Option<VectorDirect>,
    temps: Option<VectorTemps>,
    status: u32,
    prepare_fallback: &mut dyn FnMut(&mut Function) -> Result<(), EmitError>,
    address_preloaded: bool,
) -> Result<(), EmitError> {
    let unit = direct
        .filter(|direct| {
            matches!(direct, VectorDirect::UnitStride { masked: false, .. })
                && vector_direct_available(layout, Some(*direct))
        })
        .and_then(|direct| {
            let VectorDirect::UnitStride { width, .. } = direct else {
                unreachable!("filtered unit-stride direct operation")
            };
            let vsew = match width {
                8 => 0u8,
                16 => 1,
                32 => 2,
                64 => 3,
                _ => return None,
            };
            let config = KnownVectorConfig::decode(u64::from(vsew) << 3, 16 >> vsew)?;
            known_vector_direct_tail_merge(config, direct)
                .map(|tail_merge| (config, direct, tail_merge, i32::from(vsew) + 1))
        });
    let Some((config, direct, tail_merge, expected_sew)) = unit else {
        emit_vector_execution_dynamic(
            function,
            layout,
            helpers,
            insn,
            direct,
            temps,
            status,
            prepare_fallback,
            address_preloaded,
        )?;
        return Ok(());
    };
    let temps = temps.ok_or_else(|| EmitError("unit-vector effect lacks temporaries".into()))?;
    function.instruction(&Instruction::LocalGet(temps.unit_lmul1_sew.ok_or_else(
        || EmitError("unit-vector effect lacks a configuration local".into()),
    )?));
    function.instruction(&Instruction::I32Const(expected_sew));
    function.instruction(&Instruction::I32Eq);
    function.instruction(&Instruction::If(BlockType::Empty));
    emit_known_vector_execution(
        function,
        layout,
        helpers,
        insn,
        direct,
        config,
        tail_merge,
        Some(temps),
        status,
        prepare_fallback,
        true,
        address_preloaded,
    )?;
    function.instruction(&Instruction::Else);
    emit_vector_execution_dynamic(
        function,
        layout,
        helpers,
        insn,
        Some(direct),
        Some(temps),
        status,
        prepare_fallback,
        address_preloaded,
    )?;
    function.instruction(&Instruction::End);
    Ok(())
}

fn emit_vector_execution_dynamic(
    function: &mut Function,
    layout: JitLayout,
    helpers: HelperImports,
    insn: u32,
    direct: Option<VectorDirect>,
    temps: Option<VectorTemps>,
    status: u32,
    prepare_fallback: &mut dyn FnMut(&mut Function) -> Result<(), EmitError>,
    address_preloaded: bool,
) -> Result<(), EmitError> {
    if vector_direct_available(layout, direct) {
        let state = layout.vector_state.expect("checked direct vector state");
        let temps =
            temps.ok_or_else(|| EmitError("direct vector effect lacks temporaries".into()))?;
        let direct = direct.expect("checked direct vector candidate");
        match direct {
            VectorDirect::ConfigRetainFull { vtype, vlmax } => {
                emit_vector_retain_full_guard(function, layout, state, temps, vlmax);
                function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                emit_vector_config_immediate_body(function, layout, state, 0, vtype, vlmax);
                function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
                function.instruction(&Instruction::Else);
                emit_vector_call(function, helpers, insn, prepare_fallback)?;
                function.instruction(&Instruction::LocalSet(status));
                // A successful cold helper owns the instruction's exact
                // semantics, then exits this generated region. Consequently
                // only the guarded full-VLMAX arm reaches following members,
                // making their compile-time configuration fact sound.
                function.instruction(&Instruction::LocalGet(status));
                function.instruction(&Instruction::I32Const(1));
                function.instruction(&Instruction::I32Eq);
                function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                function.instruction(&Instruction::I32Const(2));
                function.instruction(&Instruction::Else);
                function.instruction(&Instruction::LocalGet(status));
                function.instruction(&Instruction::End);
                function.instruction(&Instruction::End);
            }
            VectorDirect::ConfigImmediate {
                destination,
                vtype,
                vl,
            } => {
                emit_vector_config_enabled_guard(function, layout);
                function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                emit_vector_config_immediate_body(function, layout, state, destination, vtype, vl);
                function.instruction(&Instruction::I32Const(if destination == 0 {
                    VECTOR_STATUS_DIRECT
                } else {
                    VECTOR_STATUS_DIRECT_SCALAR
                }));
                function.instruction(&Instruction::Else);
                emit_vector_call(function, helpers, insn, prepare_fallback)?;
                function.instruction(&Instruction::End);
            }
            VectorDirect::WholeRegisterMemory {
                load,
                register,
                base,
                registers,
            } => {
                if let Some(memory) = layout.sys {
                    validate_system_memory(memory)?;
                }
                emit_vector_whole_memory_guard(
                    function, layout, state, load, register, base, registers, temps,
                );
                function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                emit_vector_whole_memory_body(
                    function, layout, state, load, register, registers, temps,
                );
                function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
                function.instruction(&Instruction::Else);
                emit_vector_call(function, helpers, insn, prepare_fallback)?;
                function.instruction(&Instruction::End);
            }
            VectorDirect::WholeRegisterMove {
                destination,
                source,
                registers,
            } => {
                emit_vector_direct_state_load(function, state, temps);
                emit_vector_whole_move_guard(function, layout, state, temps);
                function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                emit_vector_whole_move_body(
                    function,
                    layout,
                    state,
                    destination,
                    source,
                    registers,
                    temps,
                );
                function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
                function.instruction(&Instruction::Else);
                emit_vector_call(function, helpers, insn, prepare_fallback)?;
                function.instruction(&Instruction::End);
            }
            VectorDirect::ScalarInsert {
                destination,
                source,
            } => {
                emit_vector_direct_state_load(function, state, temps);
                emit_vector_valid_config_guard(function, layout, state, temps);
                function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                emit_vector_scalar_insert_body(function, layout, state, destination, source, temps);
                function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
                function.instruction(&Instruction::Else);
                emit_vector_call(function, helpers, insn, prepare_fallback)?;
                function.instruction(&Instruction::End);
            }
            VectorDirect::ScalarExtract {
                destination,
                source,
            } => {
                emit_vector_direct_state_load(function, state, temps);
                emit_vector_valid_config_guard(function, layout, state, temps);
                function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                emit_vector_scalar_extract_body(
                    function,
                    layout,
                    state,
                    destination,
                    source,
                    temps,
                );
                function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT_SCALAR));
                function.instruction(&Instruction::Else);
                emit_vector_call(function, helpers, insn, prepare_fallback)?;
                function.instruction(&Instruction::End);
            }
            VectorDirect::FloatScalarInsert {
                destination,
                source,
            } => {
                emit_vector_direct_state_load(function, state, temps);
                emit_vector_valid_config_guard(function, layout, state, temps);
                emit_vector_float_width_guard(function, layout, temps);
                function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                emit_vector_float_scalar_insert_body(
                    function,
                    layout,
                    state,
                    destination,
                    source,
                    temps,
                );
                function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
                function.instruction(&Instruction::Else);
                emit_vector_call(function, helpers, insn, prepare_fallback)?;
                function.instruction(&Instruction::End);
            }
            VectorDirect::FloatScalarExtract {
                destination,
                source,
            } => {
                emit_vector_direct_state_load(function, state, temps);
                emit_vector_valid_config_guard(function, layout, state, temps);
                emit_vector_float_width_guard(function, layout, temps);
                function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                emit_vector_float_scalar_extract_body(
                    function,
                    layout,
                    state,
                    destination,
                    source,
                    temps,
                );
                function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT_SCALAR));
                function.instruction(&Instruction::Else);
                emit_vector_call(function, helpers, insn, prepare_fallback)?;
                function.instruction(&Instruction::End);
            }
            VectorDirect::Lane { .. }
            | VectorDirect::UnitStride { .. }
            | VectorDirect::Strided { .. }
            | VectorDirect::SlideImmediate { .. }
            | VectorDirect::SlideOne { .. }
            | VectorDirect::GatherImmediate { .. }
            | VectorDirect::GatherVector { .. }
            | VectorDirect::Index { .. }
            | VectorDirect::Reduction { .. }
            | VectorDirect::MaskLogic { .. }
            | VectorDirect::WidenAddSub { .. }
            | VectorDirect::WidenMultiplyAccumulate { .. }
            | VectorDirect::FloatSign { .. }
            | VectorDirect::FloatBroadcast { .. }
            | VectorDirect::FloatSlideOne { .. }
            | VectorDirect::Compare { .. } => {
                if matches!(
                    direct,
                    VectorDirect::UnitStride { .. } | VectorDirect::Strided { .. }
                ) {
                    if let Some(memory) = layout.sys {
                        validate_system_memory(memory)?;
                    }
                }
                emit_vector_direct_state_load(function, state, temps);
                emit_vector_direct_guard(
                    function,
                    layout,
                    state,
                    direct,
                    temps,
                    false,
                    true,
                    address_preloaded,
                );
                function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                emit_vector_direct_body(function, layout, state, direct, temps, false, false);
                function.instruction(&Instruction::I32Const(VECTOR_STATUS_DIRECT));
                function.instruction(&Instruction::Else);
                // System-memory guards reuse `index` for the fused-TLB row.
                // Recover the architectural LMUL encoding before deciding
                // whether the fractional class should get a second chance.
                function.instruction(&Instruction::LocalGet(temps.vtype));
                function.instruction(&Instruction::I32WrapI64);
                function.instruction(&Instruction::I32Const(7));
                function.instruction(&Instruction::I32And);
                function.instruction(&Instruction::LocalSet(temps.index));
                function.instruction(&Instruction::LocalGet(temps.index));
                function.instruction(&Instruction::I32Const(3));
                function.instruction(&Instruction::I32GtU);
                function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                emit_vector_direct_class(
                    function,
                    layout,
                    helpers,
                    insn,
                    state,
                    direct,
                    temps,
                    true,
                    prepare_fallback,
                    address_preloaded,
                )?;
                function.instruction(&Instruction::Else);
                emit_vector_partial_class(
                    function,
                    layout,
                    helpers,
                    insn,
                    state,
                    direct,
                    temps,
                    false,
                    prepare_fallback,
                    address_preloaded,
                )?;
                function.instruction(&Instruction::End);
                function.instruction(&Instruction::End);
            }
        }
    } else {
        emit_vector_call(function, helpers, insn, prepare_fallback)?;
    }
    function.instruction(&Instruction::LocalSet(status));
    Ok(())
}

fn emit_pc_from_local(function: &mut Function, layout: JitLayout, local: u32) {
    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
    function.instruction(&Instruction::LocalGet(local));
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
}

/// Publish a cold monomorphic-guard miss for the runtime's bounded
/// polymorphic-target profiler. The owner is written last and acts as the
/// validity tag after `call_block` cleared it before entry.
fn emit_ic_guard_miss(
    function: &mut Function,
    region: &Region,
    layout: JitLayout,
    target_local: u32,
) {
    if layout.ic_miss_owner_addr == 0 || layout.ic_miss_target_addr == 0 {
        return;
    }
    function.instruction(&Instruction::I32Const(layout.ic_miss_target_addr as i32));
    function.instruction(&Instruction::LocalGet(target_local));
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
    function.instruction(&Instruction::I32Const(layout.ic_miss_owner_addr as i32));
    emit_guest_pc(function, region.entry_pc, layout);
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
}

fn emit_commit_outputs(
    function: &mut Function,
    layout: JitLayout,
    outputs: &[(u8, ValueId)],
    mut emit_output: impl FnMut(&mut Function, ValueId) -> Result<(), EmitError>,
) -> Result<(), EmitError> {
    for &(reg, value) in outputs {
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        emit_output(function, value)?;
        function.instruction(&Instruction::I64Store(memarg(3, u64::from(reg) * 8)));
    }
    Ok(())
}

fn emit_commit_f_outputs(
    function: &mut Function,
    layout: JitLayout,
    outputs: &[(u8, ValueId)],
    mut emit_output: impl FnMut(&mut Function, ValueId) -> Result<(), EmitError>,
) -> Result<(), EmitError> {
    for &(reg, value) in outputs {
        function.instruction(&Instruction::I32Const(layout.f_base as i32));
        emit_output(function, value)?;
        function.instruction(&Instruction::I64Store(memarg(3, u64::from(reg) * 8)));
    }
    Ok(())
}

fn emit_retirement_const(function: &mut Function, retired_addr: u32, retired: u32) {
    if retired == 0 {
        return;
    }
    function.instruction(&Instruction::I32Const(retired_addr as i32));
    function.instruction(&Instruction::I32Const(retired_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::I64Const(i64::from(retired)));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
}

fn emit_retirement_local(function: &mut Function, retired_addr: u32, local: u32, extra: u32) {
    function.instruction(&Instruction::I32Const(retired_addr as i32));
    function.instruction(&Instruction::I32Const(retired_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalGet(local));
    function.instruction(&Instruction::I64Add);
    if extra != 0 {
        function.instruction(&Instruction::I64Const(i64::from(extra)));
        function.instruction(&Instruction::I64Add);
    }
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
}

/// Finish an SC attempt after its conditional store has either completed or
/// been skipped. Reservation op 1 is deliberately a non-destructive probe:
/// store-address validation can side-exit, in which case the interpreter must
/// re-execute the same SC with the reservation still live. Only a path that
/// stays in generated code reaches this unconditional clear.
fn emit_reservation_clear(
    function: &mut Function,
    helpers: HelperImports,
    address_local: u32,
) -> Result<(), EmitError> {
    function.instruction(&Instruction::I32Const(2));
    function.instruction(&Instruction::LocalGet(0));
    function.instruction(&Instruction::LocalGet(address_local));
    function.instruction(&Instruction::Call(helpers.reservation_index().ok_or_else(
        || EmitError("conditional store lacks a reservation helper".into()),
    )?));
    function.instruction(&Instruction::Drop);
    Ok(())
}

fn import_guest_base(imports: &mut ImportSection, layout: JitLayout) {
    if layout.pic_code_base.is_some() {
        imports.import(
            "env",
            "guest_base",
            EntityType::Global(GlobalType {
                val_type: ValType::I64,
                mutable: false,
                shared: false,
            }),
        );
    }
}

fn finish_module(function: Function, helpers: HelperImports, layout: JitLayout) -> Vec<u8> {
    let mut module = Module::new();
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32], []);
    if helpers.fp {
        types.ty().function(
            [
                ValType::I32,
                ValType::I64,
                ValType::I64,
                ValType::I64,
                ValType::I32,
                ValType::I32,
            ],
            [ValType::I64],
        );
    }
    if helpers.reservation.is_some() {
        types
            .ty()
            .function([ValType::I32, ValType::I32, ValType::I64], [ValType::I32]);
    }
    if helpers.vector.is_some() {
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
    }
    if helpers.tlb_fill {
        types
            .ty()
            .function([ValType::I64, ValType::I32], [ValType::I64]);
    }
    if helpers.bulk_copy {
        types.ty().function(
            [
                ValType::I32,
                ValType::I64,
                ValType::I64,
                ValType::I64,
                ValType::I32,
                ValType::I32,
                ValType::I32,
            ],
            [ValType::I64],
        );
    }
    module.section(&types);

    let mut imports = ImportSection::new();
    imports.import(
        "env",
        "memory",
        MemoryType {
            minimum: 0,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        },
    );
    import_guest_base(&mut imports, layout);
    if helpers.fp {
        imports.import("env", "fp_exec", EntityType::Function(1));
    }
    if let Some(reservation) = helpers.reservation {
        imports.import(
            "env",
            match reservation {
                ReservationCapability::User => "user_reservation",
                ReservationCapability::System => "system_reservation",
            },
            EntityType::Function(1 + helpers.fp as u32),
        );
    }
    if let Some(vector) = helpers.vector {
        imports.import(
            "env",
            match vector {
                VectorCapability::User => "user_vector",
                VectorCapability::System => "system_vector",
            },
            EntityType::Function(1 + helpers.fp as u32 + helpers.reservation.is_some() as u32),
        );
    }
    if helpers.tlb_fill {
        imports.import(
            "env",
            "tlb_fill",
            EntityType::Function(
                1 + helpers.fp as u32
                    + helpers.reservation.is_some() as u32
                    + helpers.vector.is_some() as u32,
            ),
        );
    }
    if helpers.bulk_copy {
        imports.import(
            "env",
            "system_bulk_copy",
            EntityType::Function(
                1 + helpers.fp as u32
                    + helpers.reservation.is_some() as u32
                    + helpers.vector.is_some() as u32
                    + helpers.tlb_fill as u32,
            ),
        );
    }
    if helpers.chain {
        imports.import("env", "chain_next", EntityType::Function(0));
    }
    module.section(&imports);

    let mut functions = FunctionSection::new();
    functions.function(0);
    module.section(&functions);

    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, helpers.count());
    module.section(&exports);

    let mut code = CodeSection::new();
    code.function(&function);
    module.section(&code);
    module.finish()
}

/// Finish a module whose public entries all name one register-resident
/// dispatcher/body function. Keeping this distinct from `finish_multi_module`
/// makes the function-index calculation explicit: helper imports precede the
/// public dispatcher, followed by any private cold fallback functions, while
/// linear memory is not in the function index space.
fn finish_shared_module(
    function: Function,
    fallbacks: Vec<Function>,
    helpers: HelperImports,
    member_count: u32,
    export_members: bool,
    layout: JitLayout,
) -> Vec<u8> {
    let mut module = Module::new();
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32], []);
    if helpers.fp {
        types.ty().function(
            [
                ValType::I32,
                ValType::I64,
                ValType::I64,
                ValType::I64,
                ValType::I32,
                ValType::I32,
            ],
            [ValType::I64],
        );
    }
    if helpers.reservation.is_some() {
        types
            .ty()
            .function([ValType::I32, ValType::I32, ValType::I64], [ValType::I32]);
    }
    if helpers.vector.is_some() {
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
    }
    if helpers.tlb_fill {
        types
            .ty()
            .function([ValType::I64, ValType::I32], [ValType::I64]);
    }
    if helpers.bulk_copy {
        types.ty().function(
            [
                ValType::I32,
                ValType::I64,
                ValType::I64,
                ValType::I64,
                ValType::I32,
                ValType::I32,
                ValType::I32,
            ],
            [ValType::I64],
        );
    }
    if helpers.tail_chain {
        types.ty().function([ValType::I32, ValType::I32], []);
    }
    module.section(&types);

    let mut imports = ImportSection::new();
    imports.import(
        "env",
        "memory",
        MemoryType {
            minimum: 0,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        },
    );
    import_guest_base(&mut imports, layout);
    if helpers.fp {
        imports.import("env", "fp_exec", EntityType::Function(1));
    }
    if let Some(reservation) = helpers.reservation {
        imports.import(
            "env",
            match reservation {
                ReservationCapability::User => "user_reservation",
                ReservationCapability::System => "system_reservation",
            },
            EntityType::Function(1 + helpers.fp as u32),
        );
    }
    if let Some(vector) = helpers.vector {
        imports.import(
            "env",
            match vector {
                VectorCapability::User => "user_vector",
                VectorCapability::System => "system_vector",
            },
            EntityType::Function(1 + helpers.fp as u32 + helpers.reservation.is_some() as u32),
        );
    }
    if helpers.tlb_fill {
        imports.import(
            "env",
            "tlb_fill",
            EntityType::Function(
                1 + helpers.fp as u32
                    + helpers.reservation.is_some() as u32
                    + helpers.vector.is_some() as u32,
            ),
        );
    }
    if helpers.bulk_copy {
        imports.import(
            "env",
            "system_bulk_copy",
            EntityType::Function(
                1 + helpers.fp as u32
                    + helpers.reservation.is_some() as u32
                    + helpers.vector.is_some() as u32
                    + helpers.tlb_fill as u32,
            ),
        );
    }
    if helpers.chain {
        imports.import("env", "chain_next", EntityType::Function(0));
    }
    if helpers.tail_chain {
        imports.import(
            "env",
            "tail_chain",
            EntityType::Function(
                1 + helpers.fp as u32
                    + helpers.reservation.is_some() as u32
                    + helpers.vector.is_some() as u32
                    + helpers.tlb_fill as u32
                    + helpers.bulk_copy as u32,
            ),
        );
    }
    module.section(&imports);

    let mut functions = FunctionSection::new();
    functions.function(0);
    for _ in &fallbacks {
        functions.function(0);
    }
    module.section(&functions);

    let function_index = helpers.count();
    let mut exports = ExportSection::new();
    if export_members {
        for index in 0..member_count {
            exports.export(&format!("r{index}"), ExportKind::Func, function_index);
        }
    } else {
        exports.export("run", ExportKind::Func, function_index);
    }
    module.section(&exports);

    let mut code = CodeSection::new();
    code.function(&function);
    for fallback in &fallbacks {
        code.function(fallback);
    }
    module.section(&code);
    module.finish()
}

const MULTI_ENTRY_HOP_CAP: i32 = 65_536;

fn emit_multi_dispatch(entries: &[u64], layout: JitLayout, function_base: u32) -> Function {
    // local 0 is the opaque state parameter.
    const PC_LOCAL: u32 = 1;
    const RETIRED_BEFORE_LOCAL: u32 = 2;
    const MATCHED_LOCAL: u32 = 3;
    const HOPS_LOCAL: u32 = 4;
    let mut function =
        Function::new_with_locals_types([ValType::I64, ValType::I64, ValType::I32, ValType::I32]);

    let mut sorted: Vec<(u64, u32)> = entries
        .iter()
        .enumerate()
        .map(|(index, &pc)| (pc, function_base + index as u32))
        .collect();
    sorted.sort_unstable_by_key(|&(pc, _)| pc);

    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(HOPS_LOCAL));
    function.instruction(&Instruction::Block(BlockType::Empty));
    function.instruction(&Instruction::Loop(BlockType::Empty));

    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalSet(PC_LOCAL));
    function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalSet(RETIRED_BEFORE_LOCAL));
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::LocalSet(MATCHED_LOCAL));

    emit_dispatch_tree(&mut function, &sorted, layout, PC_LOCAL, MATCHED_LOCAL);

    // An uncovered PC or a zero-retirement precise side exit belongs to T0.
    function.instruction(&Instruction::LocalGet(MATCHED_LOCAL));
    function.instruction(&Instruction::I32Eqz);
    function.instruction(&Instruction::BrIf(1));
    function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalGet(RETIRED_BEFORE_LOCAL));
    function.instruction(&Instruction::I64Eq);
    function.instruction(&Instruction::BrIf(1));

    // RETIRED_CELL is cumulative for the entire public invocation. Loop
    // bodies compare against the same cumulative budget (see their guards),
    // and the dispatcher stops before beginning another member once it is
    // exhausted.
    if layout.fuel_addr != 0 {
        function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I32Const(layout.fuel_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64GeU);
        function.instruction(&Instruction::BrIf(1));
    }

    // Keep a malformed/no-fuel embedding bounded even when every member
    // retires one instruction and cycles entirely inside the module.
    function.instruction(&Instruction::LocalGet(HOPS_LOCAL));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalTee(HOPS_LOCAL));
    function.instruction(&Instruction::I32Const(MULTI_ENTRY_HOP_CAP));
    function.instruction(&Instruction::I32GeU);
    function.instruction(&Instruction::BrIf(1));
    function.instruction(&Instruction::Br(0));
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);
    function
}

fn emit_dispatch_tree(
    function: &mut Function,
    entries: &[(u64, u32)],
    layout: JitLayout,
    pc_local: u32,
    matched_local: u32,
) {
    if entries.len() <= 3 {
        for &(pc, target) in entries {
            function.instruction(&Instruction::LocalGet(pc_local));
            emit_guest_pc(function, pc, layout);
            function.instruction(&Instruction::I64Eq);
            function.instruction(&Instruction::If(BlockType::Empty));
            function.instruction(&Instruction::LocalGet(0));
            function.instruction(&Instruction::Call(target));
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::LocalSet(matched_local));
            function.instruction(&Instruction::End);
        }
        return;
    }

    let middle = entries.len() / 2;
    function.instruction(&Instruction::LocalGet(pc_local));
    emit_guest_pc(function, entries[middle].0, layout);
    function.instruction(&Instruction::I64LtU);
    function.instruction(&Instruction::If(BlockType::Empty));
    emit_dispatch_tree(
        function,
        &entries[..middle],
        layout,
        pc_local,
        matched_local,
    );
    function.instruction(&Instruction::Else);
    emit_dispatch_tree(
        function,
        &entries[middle..],
        layout,
        pc_local,
        matched_local,
    );
    function.instruction(&Instruction::End);
}

fn emit_tail_wrapper(body_index: u32, target: Option<(u64, u32)>, layout: JitLayout) -> Function {
    // local 0 is the opaque state pointer; local 1 snapshots cumulative
    // retirement so a zero-progress precise side exit can never tail-spin.
    let mut function = Function::new_with_locals_types([ValType::I64]);
    function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalSet(1));
    function.instruction(&Instruction::LocalGet(0));
    function.instruction(&Instruction::Call(body_index));

    if let Some((target_pc, target_index)) = target {
        function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::LocalGet(1));
        function.instruction(&Instruction::I64GtU);
        function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        emit_guest_pc(&mut function, target_pc, layout);
        function.instruction(&Instruction::I64Eq);
        function.instruction(&Instruction::I32And);
        function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I32Const(layout.fuel_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64LtU);
        function.instruction(&Instruction::I32And);
        function.instruction(&Instruction::If(BlockType::Empty));
        function.instruction(&Instruction::LocalGet(0));
        function.instruction(&Instruction::ReturnCall(target_index));
        function.instruction(&Instruction::End);
    }
    function.instruction(&Instruction::End);
    function
}

fn finish_multi_module(
    functions: Vec<Function>,
    wrappers: Vec<Function>,
    dispatcher: Function,
    helpers: HelperImports,
    export_members: bool,
    layout: JitLayout,
) -> Vec<u8> {
    let mut module = Module::new();
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32], []);
    if helpers.fp {
        types.ty().function(
            [
                ValType::I32,
                ValType::I64,
                ValType::I64,
                ValType::I64,
                ValType::I32,
                ValType::I32,
            ],
            [ValType::I64],
        );
    }
    if helpers.reservation.is_some() {
        types
            .ty()
            .function([ValType::I32, ValType::I32, ValType::I64], [ValType::I32]);
    }
    if helpers.vector.is_some() {
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
    }
    if helpers.tlb_fill {
        types
            .ty()
            .function([ValType::I64, ValType::I32], [ValType::I64]);
    }
    if helpers.bulk_copy {
        types.ty().function(
            [
                ValType::I32,
                ValType::I64,
                ValType::I64,
                ValType::I64,
                ValType::I32,
                ValType::I32,
                ValType::I32,
            ],
            [ValType::I64],
        );
    }
    module.section(&types);

    let mut imports = ImportSection::new();
    imports.import(
        "env",
        "memory",
        MemoryType {
            minimum: 0,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        },
    );
    import_guest_base(&mut imports, layout);
    if helpers.fp {
        imports.import("env", "fp_exec", EntityType::Function(1));
    }
    if let Some(reservation) = helpers.reservation {
        imports.import(
            "env",
            match reservation {
                ReservationCapability::User => "user_reservation",
                ReservationCapability::System => "system_reservation",
            },
            EntityType::Function(1 + helpers.fp as u32),
        );
    }
    if let Some(vector) = helpers.vector {
        imports.import(
            "env",
            match vector {
                VectorCapability::User => "user_vector",
                VectorCapability::System => "system_vector",
            },
            EntityType::Function(1 + helpers.fp as u32 + helpers.reservation.is_some() as u32),
        );
    }
    if helpers.tlb_fill {
        imports.import(
            "env",
            "tlb_fill",
            EntityType::Function(
                1 + helpers.fp as u32
                    + helpers.reservation.is_some() as u32
                    + helpers.vector.is_some() as u32,
            ),
        );
    }
    if helpers.bulk_copy {
        imports.import(
            "env",
            "system_bulk_copy",
            EntityType::Function(
                1 + helpers.fp as u32
                    + helpers.reservation.is_some() as u32
                    + helpers.vector.is_some() as u32
                    + helpers.tlb_fill as u32,
            ),
        );
    }
    module.section(&imports);

    let body_count = functions.len() as u32;
    let wrapper_count = wrappers.len() as u32;
    let mut function_section = FunctionSection::new();
    for _ in 0..=body_count + wrapper_count {
        function_section.function(0);
    }
    module.section(&function_section);

    let dispatcher_index = helpers.count() + body_count + wrapper_count;
    let mut exports = ExportSection::new();
    if export_members {
        for index in 0..body_count {
            exports.export(&format!("r{index}"), ExportKind::Func, dispatcher_index);
        }
    } else {
        exports.export("run", ExportKind::Func, dispatcher_index);
    }
    module.section(&exports);

    let mut code = CodeSection::new();
    for function in &functions {
        code.function(function);
    }
    for wrapper in &wrappers {
        code.function(wrapper);
    }
    code.function(&dispatcher);
    module.section(&code);
    module.finish()
}

fn emit_single_latch_loop(
    region: &Region,
    layout: JitLayout,
    loop_backedge: LoopBackedge,
) -> Result<Function, EmitError> {
    if !region.f_outputs.is_empty() || region.fcsr_output.is_some() {
        return Err(EmitError(
            "floating-point state is not yet supported by the loop carrier".into(),
        ));
    }
    // Every SSA value gets a local because the loop body recomputes values on
    // every iteration. ReadX locals are loop parameters initialized once from
    // architectural state and updated from final outputs on the backedge.
    let local_map: Vec<Option<u32>> = (0..region.values.len())
        .map(|index| Some(1 + index as u32))
        .collect();
    let retired_local = 1 + region.values.len() as u32;
    let next_pc_local = retired_local + 1;
    let mut local_types: Vec<ValType> = region.values.iter().map(|v| val_type(v.ty)).collect();
    local_types.extend([ValType::I64, ValType::I64]);
    let mut function = Function::new_with_locals_types(local_types);

    // Initialize loop-carried architectural inputs before any possible state
    // commit. Other values are recomputed inside the loop.
    for (index, value) in region.values.iter().enumerate() {
        if matches!(value.op, Op::ReadX(_)) {
            emit_value_body(
                &mut function,
                region,
                layout,
                &local_map,
                &vec![true; region.values.len()],
                ValueId(index),
            )?;
            function.instruction(&Instruction::LocalSet(1 + index as u32));
        }
    }
    function.instruction(&Instruction::I64Const(0));
    function.instruction(&Instruction::LocalSet(retired_local));

    function.instruction(&Instruction::Block(BlockType::Empty));
    function.instruction(&Instruction::Loop(BlockType::Empty));

    let all_defined = vec![true; region.values.len()];
    for (index, value) in region.values.iter().enumerate() {
        if matches!(value.op, Op::ReadX(_)) {
            continue;
        }
        emit_value_body(
            &mut function,
            region,
            layout,
            &local_map,
            &all_defined,
            ValueId(index),
        )?;
        function.instruction(&Instruction::LocalSet(1 + index as u32));
    }

    function.instruction(&Instruction::LocalGet(retired_local));
    function.instruction(&Instruction::I64Const(i64::from(region.retired)));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::LocalSet(retired_local));

    // Continue only when the guest branch is taken and another full iteration
    // fits the current dispatch's fuel. The first iteration is always allowed,
    // preserving the bounded-basic-block overshoot contract.
    if let Some(condition) = loop_backedge.condition {
        function.instruction(&Instruction::LocalGet(1 + condition.0 as u32));
    } else {
        function.instruction(&Instruction::I32Const(1));
    }
    if layout.fuel_addr != 0 {
        function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::LocalGet(retired_local));
        function.instruction(&Instruction::I64Add);
        function.instruction(&Instruction::I32Const(layout.fuel_addr as i32));
        function.instruction(&Instruction::I64Load(memarg(3, 0)));
        function.instruction(&Instruction::I64LtU);
        function.instruction(&Instruction::I32And);
    } else {
        function.instruction(&Instruction::I32Const(0));
        function.instruction(&Instruction::I32And);
    }
    function.instruction(&Instruction::If(BlockType::Empty));

    // Backedge parallel-copy: outputs are already materialized, so assigning
    // them to ReadX parameter locals cannot clobber another source.
    for (index, value) in region.values.iter().enumerate() {
        let Op::ReadX(reg) = value.op else { continue };
        if let Some(&(_, output)) = region.outputs.iter().find(|&&(r, _)| r == reg) {
            function.instruction(&Instruction::LocalGet(1 + output.0 as u32));
            function.instruction(&Instruction::LocalSet(1 + index as u32));
        }
    }
    // From inside the `if`, depth one is the surrounding loop.
    function.instruction(&Instruction::Br(1));
    function.instruction(&Instruction::End);

    // The SSA select already says fallthrough when the condition is false and
    // the loop header when it was true but fuel ended.
    function.instruction(&Instruction::LocalGet(1 + region.next_pc.0 as u32));
    function.instruction(&Instruction::LocalSet(next_pc_local));
    // Break out of the surrounding block (loop depth 0, block depth 1).
    function.instruction(&Instruction::Br(1));
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);

    for &(reg, value) in &region.outputs {
        function.instruction(&Instruction::I32Const(layout.x_base as i32));
        function.instruction(&Instruction::LocalGet(1 + value.0 as u32));
        function.instruction(&Instruction::I64Store(memarg(3, u64::from(reg) * 8)));
    }
    function.instruction(&Instruction::I32Const(layout.pc_addr as i32));
    function.instruction(&Instruction::LocalGet(next_pc_local));
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
    function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
    function.instruction(&Instruction::I32Const(layout.retired_addr as i32));
    function.instruction(&Instruction::I64Load(memarg(3, 0)));
    function.instruction(&Instruction::LocalGet(retired_local));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::I64Store(memarg(3, 0)));
    function.instruction(&Instruction::End);

    Ok(function)
}

fn emit_value(
    function: &mut Function,
    region: &Region,
    layout: JitLayout,
    local_map: &[Option<u32>],
    defined: &[bool],
    value: ValueId,
) -> Result<(), EmitError> {
    if let Some(local) = local_map[value.0] {
        if defined[value.0] {
            function.instruction(&Instruction::LocalGet(local));
            return Ok(());
        }
    }
    emit_value_body(function, region, layout, local_map, defined, value)
}

fn emit_value_body(
    function: &mut Function,
    region: &Region,
    layout: JitLayout,
    local_map: &[Option<u32>],
    defined: &[bool],
    value: ValueId,
) -> Result<(), EmitError> {
    let data = &region.values[value.0];
    let operand = |function: &mut Function, value: ValueId| {
        emit_value(function, region, layout, local_map, defined, value)
    };
    match data.op {
        Op::ConstI32(value) => {
            function.instruction(&Instruction::I32Const(value));
        }
        Op::ConstI64(value) => {
            function.instruction(&Instruction::I64Const(value));
        }
        Op::GuestPc(pc) => emit_guest_pc(function, pc, layout),
        Op::ReadX(reg) => {
            function.instruction(&Instruction::I32Const(layout.x_base as i32));
            function.instruction(&Instruction::I64Load(memarg(3, u64::from(reg) * 8)));
        }
        Op::ReadF(reg) => {
            function.instruction(&Instruction::I32Const(layout.f_base as i32));
            function.instruction(&Instruction::I64Load(memarg(3, u64::from(reg) * 8)));
        }
        Op::ReadFcsr => {
            function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
            function.instruction(&Instruction::I32Load(memarg(2, 0)));
        }
        Op::Binary { op, lhs, rhs } => {
            operand(function, lhs)?;
            operand(function, rhs)?;
            let instruction = match op {
                BinaryOp::I64Add => Instruction::I64Add,
                BinaryOp::I64Sub => Instruction::I64Sub,
                BinaryOp::I64Mul => Instruction::I64Mul,
                BinaryOp::I64And => Instruction::I64And,
                BinaryOp::I64Or => Instruction::I64Or,
                BinaryOp::I64Xor => Instruction::I64Xor,
                BinaryOp::I64Shl => Instruction::I64Shl,
                BinaryOp::I64ShrU => Instruction::I64ShrU,
                BinaryOp::I64ShrS => Instruction::I64ShrS,
                BinaryOp::I32Add => Instruction::I32Add,
                BinaryOp::I32Sub => Instruction::I32Sub,
                BinaryOp::I32Mul => Instruction::I32Mul,
                BinaryOp::I32And => Instruction::I32And,
                BinaryOp::I32Or => Instruction::I32Or,
                BinaryOp::I32Xor => Instruction::I32Xor,
                BinaryOp::I32Shl => Instruction::I32Shl,
                BinaryOp::I32ShrU => Instruction::I32ShrU,
                BinaryOp::I32ShrS => Instruction::I32ShrS,
                BinaryOp::I64Eq => Instruction::I64Eq,
                BinaryOp::I64Ne => Instruction::I64Ne,
                BinaryOp::I64LtS => Instruction::I64LtS,
                BinaryOp::I64LtU => Instruction::I64LtU,
                BinaryOp::I64GeS => Instruction::I64GeS,
                BinaryOp::I64GeU => Instruction::I64GeU,
            };
            function.instruction(&instruction);
        }
        Op::Divide { op, lhs, rhs } => {
            emit_guarded_divide(function, op, |function, which| {
                operand(function, if which == 0 { lhs } else { rhs })
            })?;
        }
        Op::WrapI64ToI32(value) => {
            operand(function, value)?;
            function.instruction(&Instruction::I32WrapI64);
        }
        Op::ExtendI32S(value) => {
            operand(function, value)?;
            function.instruction(&Instruction::I64ExtendI32S);
        }
        Op::ExtendI32U(value) => {
            operand(function, value)?;
            function.instruction(&Instruction::I64ExtendI32U);
        }
        Op::SelectI64 {
            condition,
            if_true,
            if_false,
        } => {
            operand(function, if_true)?;
            operand(function, if_false)?;
            operand(function, condition)?;
            function.instruction(&Instruction::Select);
        }
        Op::Load { .. } => {
            return Err(EmitError(
                "effectful load reached the pure stackifying emitter".into(),
            ));
        }
        Op::ExactFp { .. } => {
            return Err(EmitError(
                "effectful FP helper reached the pure stackifying emitter".into(),
            ));
        }
        Op::Reservation { .. } => {
            return Err(EmitError(
                "reservation helper reached the pure stackifying emitter".into(),
            ));
        }
        Op::ReloadFcsr(_) => {
            function.instruction(&Instruction::I32Const(layout.fcsr_addr as i32));
            function.instruction(&Instruction::I32Load(memarg(2, 0)));
        }
    }
    Ok(())
}

/// Lower RISC-V's non-trapping divide contract to structured Wasm. Wasm's
/// integer division is partial for a zero divisor and for MIN / -1, so `select`
/// cannot guard it (both select operands are evaluated). Nested result-typed
/// `if`s keep the trapping operator entirely off the exceptional path.
fn emit_guarded_divide(
    function: &mut Function,
    op: DivideOp,
    mut operand: impl FnMut(&mut Function, u8) -> Result<(), EmitError>,
) -> Result<(), EmitError> {
    let ty = op.value_type();
    operand(function, 1)?;
    function.instruction(&match ty {
        ValueType::I32 => Instruction::I32Eqz,
        ValueType::I64 => Instruction::I64Eqz,
    });
    function.instruction(&Instruction::If(BlockType::Result(val_type(ty))));
    if op.is_remainder() {
        operand(function, 0)?;
    } else {
        function.instruction(&match ty {
            ValueType::I32 => Instruction::I32Const(-1),
            ValueType::I64 => Instruction::I64Const(-1),
        });
    }
    function.instruction(&Instruction::Else);

    if op.is_signed() {
        operand(function, 0)?;
        function.instruction(&match ty {
            ValueType::I32 => Instruction::I32Const(i32::MIN),
            ValueType::I64 => Instruction::I64Const(i64::MIN),
        });
        function.instruction(&match ty {
            ValueType::I32 => Instruction::I32Eq,
            ValueType::I64 => Instruction::I64Eq,
        });
        operand(function, 1)?;
        function.instruction(&match ty {
            ValueType::I32 => Instruction::I32Const(-1),
            ValueType::I64 => Instruction::I64Const(-1),
        });
        function.instruction(&match ty {
            ValueType::I32 => Instruction::I32Eq,
            ValueType::I64 => Instruction::I64Eq,
        });
        function.instruction(&Instruction::I32And);
        function.instruction(&Instruction::If(BlockType::Result(val_type(ty))));
        if op.is_remainder() {
            function.instruction(&match ty {
                ValueType::I32 => Instruction::I32Const(0),
                ValueType::I64 => Instruction::I64Const(0),
            });
        } else {
            operand(function, 0)?;
        }
        function.instruction(&Instruction::Else);
        emit_raw_divide(function, op, &mut operand)?;
        function.instruction(&Instruction::End);
    } else {
        emit_raw_divide(function, op, &mut operand)?;
    }
    function.instruction(&Instruction::End);
    Ok(())
}

fn emit_raw_divide(
    function: &mut Function,
    op: DivideOp,
    operand: &mut impl FnMut(&mut Function, u8) -> Result<(), EmitError>,
) -> Result<(), EmitError> {
    operand(function, 0)?;
    operand(function, 1)?;
    let instruction = match op {
        DivideOp::I64DivS => Instruction::I64DivS,
        DivideOp::I64DivU => Instruction::I64DivU,
        DivideOp::I64RemS => Instruction::I64RemS,
        DivideOp::I64RemU => Instruction::I64RemU,
        DivideOp::I32DivS => Instruction::I32DivS,
        DivideOp::I32DivU => Instruction::I32DivU,
        DivideOp::I32RemS => Instruction::I32RemS,
        DivideOp::I32RemU => Instruction::I32RemU,
    };
    function.instruction(&instruction);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Builder, ExitKind};
    use wasmparser::{Operator, Parser, Payload};

    fn count_i64_state_ops(bytes: &[u8], offset: u64) -> (usize, usize) {
        let mut loads = 0;
        let mut stores = 0;
        for payload in Parser::new(0).parse_all(bytes) {
            let Payload::CodeSectionEntry(body) = payload.expect("generated payload parses") else {
                continue;
            };
            let mut operators = body
                .get_operators_reader()
                .expect("generated function operators parse");
            while !operators.eof() {
                match operators.read().expect("generated operator parses") {
                    Operator::I64Load { memarg } if memarg.offset == offset => loads += 1,
                    Operator::I64Store { memarg } if memarg.offset == offset => stores += 1,
                    _ => {}
                }
            }
        }
        (loads, stores)
    }

    fn count_direct_state_stores(bytes: &[u8], base: u32, offset: u64, i64_store: bool) -> usize {
        let mut stores = 0;
        for payload in Parser::new(0).parse_all(bytes) {
            let Payload::CodeSectionEntry(body) = payload.expect("generated payload parses") else {
                continue;
            };
            let mut operators = body
                .get_operators_reader()
                .expect("generated function operators parse");
            let mut stage = 0;
            while !operators.eof() {
                let operator = operators.read().expect("generated operator parses");
                match operator {
                    Operator::I32Const { value } if value == base as i32 => stage = 1,
                    Operator::LocalGet { .. } if stage == 1 => stage = 2,
                    Operator::I64Store { memarg }
                        if i64_store && stage == 2 && memarg.offset == offset =>
                    {
                        stores += 1;
                        stage = 0;
                    }
                    Operator::I32Store { memarg }
                        if !i64_store && stage == 2 && memarg.offset == offset =>
                    {
                        stores += 1;
                        stage = 0;
                    }
                    _ => stage = 0,
                }
            }
        }
        stores
    }

    fn import_names(bytes: &[u8]) -> Vec<(String, String)> {
        let mut names = Vec::new();
        for payload in Parser::new(0).parse_all(bytes) {
            let Payload::ImportSection(section) = payload.expect("generated payload parses") else {
                continue;
            };
            for import in section.into_imports() {
                let import = import.expect("generated import parses");
                names.push((import.module.to_owned(), import.name.to_owned()));
            }
        }
        names
    }

    fn simd_memory_ops(bytes: &[u8]) -> (usize, usize, usize) {
        let mut loads = 0;
        let mut adds = 0;
        let mut stores = 0;
        for payload in Parser::new(0).parse_all(bytes) {
            let Payload::CodeSectionEntry(body) = payload.expect("generated payload parses") else {
                continue;
            };
            let mut operators = body
                .get_operators_reader()
                .expect("generated function operators parse");
            while !operators.eof() {
                match operators.read().expect("generated operator parses") {
                    Operator::V128Load { .. } => loads += 1,
                    Operator::I8x16Add => adds += 1,
                    Operator::V128Store { .. } => stores += 1,
                    _ => {}
                }
            }
        }
        (loads, adds, stores)
    }

    fn two_pair_memory_region(transform_values: bool) -> Region {
        let mut builder = Builder::new(0x1000);
        let source = builder.read_x(1, 0x1000);
        let destination = builder.read_x(2, 0x1000);
        let eight = builder.const_i64(8, 0x1000);
        let one = builder.const_i64(1, 0x1000);
        let loaded0 = builder.load(source, LoadKind::I64, 0x1000, 0);
        let stored0 = if transform_values {
            builder.binary(BinaryOp::I64Add, loaded0, one, 0x1004)
        } else {
            loaded0
        };
        builder.store(destination, stored0, StoreKind::I64, 0x1004, 1);
        let source8 = builder.binary(BinaryOp::I64Add, source, eight, 0x1008);
        let destination8 = builder.binary(BinaryOp::I64Add, destination, eight, 0x1008);
        let loaded1 = builder.load(source8, LoadKind::I64, 0x1008, 2);
        let stored1 = if transform_values {
            builder.binary(BinaryOp::I64Add, loaded1, one, 0x100c)
        } else {
            loaded1
        };
        builder.store(destination8, stored1, StoreKind::I64, 0x100c, 3);
        let next = builder.const_i64(0x1010, 0x100c);
        builder.finish(0x1010, next, 4, ExitKind::Dispatch)
    }

    /// Match the common compiler ordering `ld tmp, (src); sd tmp, (dst)`: the
    /// destination architectural register is first materialized only while
    /// lifting the store, after the first load SSA value already exists.
    fn two_pair_late_destination_region() -> Region {
        let mut builder = Builder::new(0x1000);
        let source = builder.read_x(1, 0x1000);
        let loaded0 = builder.load(source, LoadKind::I64, 0x1000, 0);
        let destination = builder.read_x(2, 0x1004);
        builder.store(destination, loaded0, StoreKind::I64, 0x1004, 1);
        let eight = builder.const_i64(8, 0x1008);
        let source8 = builder.binary(BinaryOp::I64Add, source, eight, 0x1008);
        let loaded1 = builder.load(source8, LoadKind::I64, 0x1008, 2);
        let destination8 = builder.binary(BinaryOp::I64Add, destination, eight, 0x100c);
        builder.store(destination8, loaded1, StoreKind::I64, 0x100c, 3);
        let next = builder.const_i64(0x1010, 0x100c);
        builder.finish(0x1010, next, 4, ExitKind::Dispatch)
    }

    fn four_store_fill_region() -> Region {
        let mut builder = Builder::new(0x1000);
        let destination = builder.read_x(1, 0x1000);
        let fill = builder.read_x(2, 0x1000);
        for (index, offset) in [0i64, 8, 16, 24].into_iter().enumerate() {
            let immediate = builder.const_i64(offset, 0x1000 + index as u64 * 4);
            let address = builder.binary(
                BinaryOp::I64Add,
                destination,
                immediate,
                0x1000 + index as u64 * 4,
            );
            builder.store(
                address,
                fill,
                StoreKind::I64,
                0x1000 + index as u64 * 4,
                index as u32,
            );
        }
        let next = builder.const_i64(0x1010, 0x100c);
        builder.finish(0x1010, next, 4, ExitKind::Dispatch)
    }

    fn unrolled_copy_loop(step: i64) -> Region {
        let mut builder = Builder::new(0x2000);
        let source = builder.read_x(13, 0x2000);
        let destination = builder.read_x(14, 0x2000);
        let limit = builder.read_x(11, 0x2000);
        let count = builder.read_x(12, 0x2000);
        let offsets: Vec<i64> = if step > 0 {
            (0..8).map(|slot| slot * 8).collect()
        } else {
            (1..=8).map(|slot| -(slot * 8)).collect()
        };
        for (slot, offset) in offsets.into_iter().enumerate() {
            let immediate = builder.const_i64(offset, 0x2000 + slot as u64 * 8);
            let source_address = builder.binary(
                BinaryOp::I64Add,
                source,
                immediate,
                0x2000 + slot as u64 * 8,
            );
            let loaded = builder.load(
                source_address,
                LoadKind::I64,
                0x2000 + slot as u64 * 8,
                (slot * 2) as u32,
            );
            builder.write_x(15, loaded);
            let destination_address = builder.binary(
                BinaryOp::I64Add,
                destination,
                immediate,
                0x2004 + slot as u64 * 8,
            );
            builder.store(
                destination_address,
                loaded,
                StoreKind::I64,
                0x2004 + slot as u64 * 8,
                (slot * 2 + 1) as u32,
            );
        }
        let step_value = builder.const_i64(step, 0x2040);
        let next_destination = builder.binary(BinaryOp::I64Add, destination, step_value, 0x2040);
        builder.write_x(14, next_destination);
        let minus_64 = builder.const_i64(-64, 0x2044);
        let next_count = builder.binary(BinaryOp::I64Add, count, minus_64, 0x2044);
        builder.write_x(12, next_count);
        let next_source = builder.binary(BinaryOp::I64Add, source, step_value, 0x2048);
        builder.write_x(13, next_source);
        let condition = builder.binary(BinaryOp::I64LtU, limit, next_count, 0x204c);
        let taken = builder.const_i64(0x2000, 0x204c);
        let not_taken = builder.const_i64(0x2050, 0x204c);
        let next = builder.select_i64(condition, taken, not_taken, 0x204c);
        builder.finish(0x2050, next, 20, ExitKind::Dispatch)
    }

    fn single_word_copy_loop(step: i64) -> Region {
        let mut builder = Builder::new(0x4000);
        let source = builder.read_x(13, 0x4000);
        let destination = builder.read_x(14, 0x4000);
        let limit = builder.read_x(11, 0x4000);
        let count = builder.read_x(12, 0x4000);
        let offset = builder.const_i64(if step < 0 { -8 } else { 0 }, 0x4000);
        let source_address = builder.binary(BinaryOp::I64Add, source, offset, 0x4000);
        let loaded = builder.load(source_address, LoadKind::I64, 0x4000, 0);
        builder.write_x(15, loaded);
        let destination_address = builder.binary(BinaryOp::I64Add, destination, offset, 0x4004);
        builder.store(destination_address, loaded, StoreKind::I64, 0x4004, 1);
        let step_value = builder.const_i64(step, 0x4008);
        let next_destination = builder.binary(BinaryOp::I64Add, destination, step_value, 0x4008);
        builder.write_x(14, next_destination);
        let minus_eight = builder.const_i64(-8, 0x400c);
        let next_count = builder.binary(BinaryOp::I64Add, count, minus_eight, 0x400c);
        builder.write_x(12, next_count);
        let next_source = builder.binary(BinaryOp::I64Add, source, step_value, 0x4010);
        builder.write_x(13, next_source);
        let condition = builder.binary(BinaryOp::I64LtU, limit, next_count, 0x4014);
        let taken = builder.const_i64(0x4000, 0x4014);
        let not_taken = builder.const_i64(0x4018, 0x4014);
        let next = builder.select_i64(condition, taken, not_taken, 0x4014);
        builder.finish(0x4018, next, 6, ExitKind::Dispatch)
    }

    #[test]
    fn invocation_translation_cache_selects_only_dense_direct_copies() {
        let copied = two_pair_memory_region(false);
        assert_eq!(
            region_memory_profile(&copied),
            RegionMemoryProfile {
                loads: 2,
                stores: 2,
                direct_copies: 2,
            },
        );
        assert!(dense_copy_plan(&copied, 3).is_none());
        let plan = dense_copy_plan(&copied, 2).expect("two direct pairs form one copy range");
        assert_eq!(plan.bytes, 16);
        assert_eq!(plan.source_base_offset, 0);
        assert_eq!(plan.destination_base_offset, 0);
        assert_eq!(
            plan.accesses
                .iter()
                .map(|access| access.source_offset)
                .collect::<Vec<_>>(),
            vec![0, 8],
        );

        let late_destination = two_pair_late_destination_region();
        let late_plan = dense_copy_plan(&late_destination, 2)
            .expect("setup can move to the first store after its destination root");
        let first_store_position = late_destination
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Store { position, .. } => Some(*position),
                _ => None,
            })
            .min()
            .expect("copy has stores");
        assert_eq!(late_plan.setup_position, first_store_position);
        assert!(late_plan.load_access(late_plan.accesses[0].load).is_none());
        assert!(late_plan.load_access(late_plan.accesses[1].load).is_some());

        let transformed = two_pair_memory_region(true);
        assert_eq!(region_memory_profile(&transformed).direct_copies, 0);
        assert!(dense_copy_plan(&transformed, 2).is_none());

        let fill = four_store_fill_region();
        let fill_plan = dense_store_plan(&fill, 4).expect("contiguous stores form one range");
        assert_eq!(fill_plan.bytes, 32);
        assert_eq!(fill_plan.destination_base_offset, 0);
        let first_fill_store = match fill.effects[0] {
            Effect::Store { position, .. } => position,
            _ => unreachable!("fill contains only stores"),
        };
        assert_eq!(fill_plan.setup_position, first_fill_store);
        assert_eq!(
            fill_plan
                .accesses
                .iter()
                .map(|access| access.destination_offset)
                .collect::<Vec<_>>(),
            vec![0, 8, 16, 24],
        );
        let mut fill_layout = system_layout(false);
        fill_layout.sys = fill_layout.sys.map(|memory| {
            memory
                .with_invocation_cache(true)
                .with_invocation_cache_min_accesses(4)
        });
        assert!(uses_dense_store_plan(&fill, fill_layout));
        let fill_wasm = emit(&fill, fill_layout, None).expect("dense store emits");
        wasmparser::Validator::new()
            .validate_all(&fill_wasm)
            .expect("dense-store module validates");
    }

    #[test]
    fn recognizes_only_the_proven_whole_copy_loop_shape() {
        for step in [64, -64] {
            let region = unrolled_copy_loop(step);
            let plan = bulk_copy_loop_plan(&region).expect("canonical 64-byte copy loop");
            assert_eq!(plan.source_reg, 13);
            assert_eq!(plan.destination_reg, 14);
            assert_eq!(plan.count_reg, 12);
            assert_eq!(plan.limit_reg, 11);
            assert_eq!(plan.value_reg, 15);
            assert_eq!(plan.step, step);
            assert_eq!(plan.bytes_per_iteration, 64);
            assert_eq!(plan.exit_pc, 0x2050);

            let loop_backedge = LoopBackedge {
                condition: Some(plan.condition),
                exit_pc: plan.exit_pc,
            };
            let layout = system_layout(true);
            let standalone = emit(&region, layout, Some(loop_backedge))
                .expect("whole-copy standalone module emits");
            wasmparser::Validator::new()
                .validate_all(&standalone)
                .expect("whole-copy standalone module validates");

            let mut other_builder = Builder::new(0x3000);
            let old = other_builder.read_x(1, 0x3000);
            let one = other_builder.const_i64(1, 0x3000);
            let next_value = other_builder.binary(BinaryOp::I64Add, old, one, 0x3000);
            other_builder.write_x(1, next_value);
            let next = other_builder.const_i64(0x3004, 0x3000);
            let other = other_builder.finish(0x3004, next, 1, ExitKind::Dispatch);
            let cached = emit_multi_entry(
                &[(&region, Some(loop_backedge)), (&other, None)],
                layout,
                true,
            )
            .expect("whole-copy cached module emits");
            wasmparser::Validator::new()
                .validate_all(&cached)
                .expect("whole-copy cached module validates");
            let structured = emit_multi_entry_mode(
                &[(&region, Some(loop_backedge)), (&other, None)],
                layout,
                true,
                MultiEntryState::RegisterStructured,
            )
            .expect("whole-copy structured module emits");
            wasmparser::Validator::new()
                .validate_all(&structured)
                .expect("whole-copy structured module validates");
        }

        for step in [8, -8] {
            let region = single_word_copy_loop(step);
            let plan = bulk_copy_loop_plan(&region).expect("canonical 8-byte copy loop");
            assert_eq!(plan.bytes_per_iteration, 8);
            assert_eq!(plan.limit_value, 7);
            assert_eq!(plan.step, step);
            let loop_backedge = LoopBackedge {
                condition: Some(plan.condition),
                exit_pc: plan.exit_pc,
            };
            let bytes = emit(&region, system_layout(true), Some(loop_backedge))
                .expect("8-byte copy module emits");
            wasmparser::Validator::new()
                .validate_all(&bytes)
                .expect("8-byte copy module validates");
        }

        let mut wrong_count = unrolled_copy_loop(64);
        let count_output = output_for_reg(&wrong_count, 12).expect("count output");
        let Op::Binary { rhs, .. } = wrong_count.values[count_output.0].op else {
            panic!("count induction must be binary")
        };
        wrong_count.values[rhs.0].op = Op::ConstI64(-8);
        assert!(bulk_copy_loop_plan(&wrong_count).is_none());
    }

    #[test]
    fn structured_execution_profile_is_opt_in_and_valid() {
        let mut first_builder = Builder::new(0x1000);
        let old = first_builder.read_x(1, 0x1000);
        let one = first_builder.const_i64(1, 0x1000);
        let value = first_builder.binary(BinaryOp::I64Add, old, one, 0x1000);
        first_builder.write_x(1, value);
        let next = first_builder.const_i64(0x1004, 0x1000);
        let mut first = first_builder.finish(0x1004, next, 1, ExitKind::Dispatch);
        first.trace_mix = [1, 0, 0, 0, 0];

        let mut second_builder = Builder::new(0x1004);
        let old = second_builder.read_x(2, 0x1004);
        let one = second_builder.const_i64(1, 0x1004);
        let value = second_builder.binary(BinaryOp::I64Add, old, one, 0x1004);
        second_builder.write_x(2, value);
        let next = second_builder.const_i64(0x1008, 0x1004);
        let mut second = second_builder.finish(0x1008, next, 1, ExitKind::Dispatch);
        second.trace_mix = [1, 0, 0, 0, 0];

        let members = [(&first, None), (&second, None)];
        let control = emit_multi_entry_mode(
            &members,
            JitLayout::bare(),
            true,
            MultiEntryState::RegisterStructured,
        )
        .expect("uninstrumented structured module emits");

        let mut profiled_layout = JitLayout::bare();
        profiled_layout.structured_profile =
            Some([1024, 1032, 1040, 1048, 1056, 1064, 1072, 1080, 1088]);
        let profiled = emit_multi_entry_mode(
            &members,
            profiled_layout,
            true,
            MultiEntryState::RegisterStructured,
        )
        .expect("profiled structured module emits");

        assert_ne!(control, profiled);
        assert!(profiled.len() > control.len());
        wasmparser::Validator::new()
            .validate_all(&profiled)
            .expect("profiled structured module validates");
    }

    #[test]
    fn structured_member_range_outlines_only_proven_ordinary_accesses() {
        let load_region = |count: usize| {
            let mut builder = Builder::new(0x1000);
            // x3 is deliberately outside the fixed structured resident bank,
            // exercising the canonical materialized-root guard path.
            let root = builder.read_x(3, 0x1000);
            for index in 0..count {
                let pc = 0x1000 + index as u64 * 4;
                let offset = builder.const_i64((index * 8) as i64, pc);
                let address = builder.binary(BinaryOp::I64Add, root, offset, pc);
                let value = builder.load(address, LoadKind::I64, pc, index as u32);
                builder.write_x(10 + index, value);
            }
            let next_pc = 0x1000 + count as u64 * 4;
            let next = builder.const_i64(next_pc as i64, next_pc.saturating_sub(4));
            builder.finish(next_pc, next, count as u32, ExitKind::Dispatch)
        };

        let two_loads = load_region(2);
        assert!(member_range_plan(&two_loads).is_none());

        let three_loads = load_region(3);
        let plan = member_range_plan(&three_loads).expect("three common-root loads are eligible");
        assert_eq!(plan.root_reg(&three_loads), Some(3));
        assert_eq!(plan.min_offset, 0);
        assert_eq!(plan.span, 24);
        assert_eq!((plan.loads, plan.stores), (3, 0));

        // Conditional stores retain their exact per-access lowering and do
        // not count toward the minimum needed to outline an ordinary member.
        let mut conditional_builder = Builder::new(0x2000);
        let root = conditional_builder.read_x(3, 0x2000);
        let value = conditional_builder.const_i64(7, 0x2000);
        for (index, offset) in [0i64, 8].into_iter().enumerate() {
            let pc = 0x2000 + index as u64 * 4;
            let offset = conditional_builder.const_i64(offset, pc);
            let address = conditional_builder.binary(BinaryOp::I64Add, root, offset, pc);
            let _ = conditional_builder.load(address, LoadKind::I64, pc, index as u32);
        }
        let condition =
            conditional_builder.reservation(ReservationOp::StoreConditional, root, 0x2008);
        conditional_builder.store_conditional(condition, root, value, StoreKind::I64, 0x2008, 2);
        let next = conditional_builder.const_i64(0x200c, 0x2008);
        let conditional = conditional_builder.finish(0x200c, next, 3, ExitKind::Dispatch);
        assert!(member_range_plan(&conditional).is_none());

        let mut next_builder = Builder::new(0x100c);
        let old = next_builder.read_x(1, 0x100c);
        let one = next_builder.const_i64(1, 0x100c);
        let value = next_builder.binary(BinaryOp::I64Add, old, one, 0x100c);
        next_builder.write_x(1, value);
        let next = next_builder.const_i64(0x1010, 0x100c);
        let next_region = next_builder.finish(0x1010, next, 1, ExitKind::Dispatch);
        let members = [(&three_loads, None), (&next_region, None)];
        let bytes = emit_multi_entry_mode(
            &members,
            system_layout(true),
            true,
            MultiEntryState::RegisterStructured,
        )
        .expect("outlined structured module emits");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("outlined structured module validates");

        let defined = Parser::new(0)
            .parse_all(&bytes)
            .find_map(|payload| match payload.expect("generated payload parses") {
                Payload::FunctionSection(section) => Some(section.count()),
                _ => None,
            })
            .expect("generated module has a function section");
        assert_eq!(defined, 2, "one dispatcher plus one private fallback");
    }

    #[test]
    fn structured_state_keeps_only_the_fixed_rvc_bank_resident() {
        let member = |entry_pc, next_pc| {
            let mut builder = Builder::new(entry_pc);
            let resident = builder.read_x(1, entry_pc);
            let materialized = builder.read_x(3, entry_pc);
            let one = builder.const_i64(1, entry_pc);
            let resident = builder.binary(BinaryOp::I64Add, resident, one, entry_pc);
            let materialized = builder.binary(BinaryOp::I64Add, materialized, one, entry_pc);
            builder.write_x(1, resident);
            builder.write_x(3, materialized);
            let next = builder.const_i64(next_pc as i64, entry_pc);
            builder.finish(next_pc, next, 1, ExitKind::Dispatch)
        };
        let first = member(0x1000, 0x1004);
        let second = member(0x1004, 0x1008);
        let bytes = emit_multi_entry_mode(
            &[(&first, None), (&second, None)],
            JitLayout::bare(),
            true,
            MultiEntryState::RegisterStructured,
        )
        .expect("hybrid structured module emits");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("hybrid structured module validates");

        // x1 is loaded/committed once around the generated function. x3 has no
        // function-wide state local and is synchronized by both member bodies.
        assert_eq!(count_i64_state_ops(&bytes, 8), (1, 1));
        assert_eq!(count_i64_state_ops(&bytes, 3 * 8), (2, 2));
    }

    #[test]
    fn structured_precise_effect_publishes_side_exit_only_state() {
        // Model the compiler sequence that exposed this bug:
        //   addi a6, s0, -128
        //   vse8.v v8, (a6)
        // The vector boundary ends the scalar SSA epoch, so a6 is absent from
        // the region's final outputs and from the structured resident bank.
        // Its precise SideExit snapshot must nevertheless reach canonical
        // state before the opaque helper call. FP and fcsr use the same rule.
        let mut first_builder = Builder::new(0x1000);
        let frame = first_builder.read_x(8, 0x1000);
        let minus_128 = first_builder.const_i64(-128, 0x1000);
        let address = first_builder.binary(BinaryOp::I64Add, frame, minus_128, 0x1000);
        first_builder.write_x(16, address);
        let fp_value = first_builder.const_i64(0x1234, 0x1000);
        first_builder.write_f(31, fp_value);
        let fcsr_value = first_builder.const_i32(0x5, 0x1000);
        first_builder.write_fcsr(fcsr_value);
        first_builder.vector(0x0208_0427, 0x1004, 0x1008, 1);
        let next = first_builder.const_i64(0x1008, 0x1004);
        let first = first_builder.finish(0x1008, next, 2, ExitKind::Dispatch);

        assert!(first.outputs.iter().all(|&(reg, _)| reg != 16));
        assert!(first.f_outputs.iter().all(|&(reg, _)| reg != 31));
        assert!(first.fcsr_output.is_none());
        let vector_exit = first
            .effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Vector { exit, .. } => Some(exit),
                _ => None,
            })
            .expect("region contains its vector boundary");
        assert!(vector_exit.outputs.iter().any(|&(reg, _)| reg == 16));
        assert!(vector_exit.f_outputs.iter().any(|&(reg, _)| reg == 31));
        assert!(vector_exit.fcsr_output.is_some());

        let mut second_builder = Builder::new(0x1008);
        let next = second_builder.const_i64(0x100c, 0x1008);
        let second = second_builder.finish(0x100c, next, 1, ExitKind::Dispatch);

        let mut layout = system_layout(true);
        layout.x_base = 0x20_000;
        layout.f_base = 0x21_000;
        layout.fcsr_addr = 0x22_000;
        layout.pc_addr = 0x22_008;
        layout.retired_addr = 0x22_010;
        layout.vector = Some(VectorCapability::System);
        let bytes = emit_multi_entry_mode(
            &[(&first, None), (&second, None)],
            layout,
            true,
            MultiEntryState::RegisterStructured,
        )
        .expect("structured vector module emits");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("structured vector module validates");

        // The hot direct arm publishes the scalar epoch outputs needed after
        // the vector boundary. The cold helper arm contains the complete
        // precise snapshot, so a static operator census sees two mutually
        // exclusive stores for every current-member output.
        assert_eq!(
            count_direct_state_stores(&bytes, layout.x_base, 16 * 8, true),
            2
        );
        assert_eq!(
            count_direct_state_stores(&bytes, layout.f_base, 31 * 8, true),
            2
        );
        assert_eq!(
            count_direct_state_stores(&bytes, layout.fcsr_addr, 0, false),
            2
        );
    }

    fn system_layout(refill_on_miss: bool) -> JitLayout {
        let mut layout = JitLayout::bare();
        layout.sys = Some(SystemMemory::fused_4k(
            4096,
            4096 + 32 * 1024,
            4096 + 64 * 1024,
            4096 + 96 * 1024,
            4096 + 128 * 1024,
            4095,
            refill_on_miss,
        ));
        layout.fuel_addr = 272;
        layout
    }

    #[test]
    fn emits_a_valid_imported_memory_module() {
        let mut builder = Builder::new(0x1000);
        let old = builder.read_x(1, 0x1000);
        let one = builder.const_i64(1, 0x1000);
        let value = builder.binary(BinaryOp::I64Add, old, one, 0x1000);
        builder.write_x(1, value);
        let next = builder.const_i64(0x1004, 0x1000);
        let region = builder.finish(0x1004, next, 1, ExitKind::Dispatch);
        let bytes = emit(&region, JitLayout::bare(), None).unwrap();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("generated module must validate");
    }

    #[test]
    fn vector_import_is_conditional_and_typed_to_the_memory_capability() {
        let vector_region = || {
            let mut builder = Builder::new(0x1000);
            builder.vector(0x0285_0457, 0x1000, 0x1004, 0);
            let next = builder.const_i64(0x1004, 0x1000);
            builder.finish(0x1004, next, 1, ExitKind::Dispatch)
        };
        let region = vector_region();

        let mut user = JitLayout::bare();
        user.mem = Some((4096, 65536));
        user.vector = Some(VectorCapability::User);
        let user_bytes = emit(&region, user, None).expect("user vector module emits");
        assert!(import_names(&user_bytes)
            .iter()
            .any(|(module, name)| module == "env" && name == "user_vector"));
        assert!(!import_names(&user_bytes)
            .iter()
            .any(|(_, name)| name == "system_vector"));

        let mut system = system_layout(true);
        system.vector = Some(VectorCapability::System);
        let system_bytes = emit(&region, system, None).expect("system vector module emits");
        assert!(import_names(&system_bytes)
            .iter()
            .any(|(module, name)| module == "env" && name == "system_vector"));
        assert!(!import_names(&system_bytes)
            .iter()
            .any(|(_, name)| name == "user_vector"));

        let mut missing = user;
        missing.vector = None;
        assert!(emit(&region, missing, None)
            .expect_err("untyped vector module is rejected")
            .0
            .contains("typed capability"));
        let mut mismatched = user;
        mismatched.vector = Some(VectorCapability::System);
        assert!(emit(&region, mismatched, None)
            .expect_err("system helper cannot receive a user machine")
            .0
            .contains("full-system memory"));

        let mut scalar_builder = Builder::new(0x2000);
        let next = scalar_builder.const_i64(0x2004, 0x2000);
        let scalar = scalar_builder.finish(0x2004, next, 1, ExitKind::Dispatch);
        let scalar_bytes = emit(&scalar, user, None).expect("scalar module remains valid");
        assert!(!import_names(&scalar_bytes)
            .iter()
            .any(|(_, name)| name.ends_with("_vector")));
    }

    #[test]
    fn read_only_vector_state_effect_emits_without_a_vector_helper() {
        let mut builder = Builder::new(0x1000);
        builder.vector_state(0x1000, 0);
        let vlenb = builder.const_i64(rv64_core::cpu::VLEN_BYTES as i64, 0x1000);
        builder.write_x(10, vlenb);
        let next = builder.const_i64(0x1004, 0x1000);
        let region = builder.finish(0x1004, next, 1, ExitKind::Dispatch);

        let mut user = JitLayout::bare();
        user.vector = Some(VectorCapability::User);
        let user_bytes = emit(&region, user, None).expect("user vector CSR module emits");
        assert!(import_names(&user_bytes)
            .iter()
            .all(|(_, name)| name != "user_vector" && name != "system_vector"));
        wasmparser::Validator::new()
            .validate_all(&user_bytes)
            .expect("user vector CSR module validates");

        let mut system = system_layout(true);
        system.vector = Some(VectorCapability::System);
        system.mstatus_addr = 0x20_000;
        let system_bytes = emit(&region, system, None).expect("system vector CSR module emits");
        assert!(import_names(&system_bytes)
            .iter()
            .all(|(_, name)| name != "user_vector" && name != "system_vector"));
        wasmparser::Validator::new()
            .validate_all(&system_bytes)
            .expect("system vector CSR module validates");

        let mut missing = user;
        missing.vector = None;
        assert!(emit(&region, missing, None).is_err());
    }

    #[test]
    fn direct_vector_state_emits_valid_simd_with_helper_fallback() {
        let mut builder = Builder::new(0x1000);
        // vadd.vv v16,v8,v12
        builder.vector(0x0286_0857, 0x1000, 0x1004, 0);
        let next = builder.const_i64(0x1004, 0x1000);
        let region = builder.finish(0x1004, next, 1, ExitKind::Dispatch);

        let mut helper_only = JitLayout::bare();
        helper_only.mem = Some((4096, 65536));
        helper_only.vector = Some(VectorCapability::User);
        let helper_bytes = emit(&region, helper_only, None).expect("helper vector module emits");
        assert_eq!(simd_memory_ops(&helper_bytes), (0, 0, 0));

        let mut direct = helper_only;
        direct.vector_state = Some(VectorStateLayout {
            regs_base: 0x20_000,
            vl_addr: 0x20_200,
            vtype_addr: 0x20_208,
            vstart_addr: 0x20_210,
            simd_count_addr: 0x20_218,
        });
        let direct_bytes = emit(&region, direct, None).expect("direct vector module emits");
        wasmparser::Validator::new()
            .validate_all(&direct_bytes)
            .expect("direct SIMD module must validate");
        let (loads, adds, stores) = simd_memory_ops(&direct_bytes);
        assert!(loads >= 2, "both vector operands must be loaded");
        assert_eq!(
            adds, 4,
            "full and tail-preserving integer/fractional arms each use i8x16.add"
        );
        assert!(stores >= 1, "the vector result must be stored");
        assert!(import_names(&direct_bytes)
            .iter()
            .any(|(module, name)| module == "env" && name == "user_vector"));
    }

    #[test]
    fn direct_vector_body_lowers_guard_approved_variants() {
        // Regression test for the round-17 "RuntimeError: unreachable" trap.
        // emit_vector_direct_body() and emit_vector_direct_guard() used to
        // have a single catch-all unreachable!() for these eight VectorDirect
        // variants even though vector_direct_available() had approved them;
        // any hot guest PC executing vsetvli / vmv.r / vmv.s / etc. then
        // crashed the wasm module. The body helpers all already existed; the
        // fix wires them through the dispatcher arms and adds minimal guard
        // checks. This test drives each guard-approved variant through the
        // public emit() pipeline so any future reintroduction of an arm
        // mismatch fails fast.

        let layout_for = || {
            let mut layout = JitLayout::bare();
            layout.mem = Some((4096, 65536));
            layout.vector = Some(VectorCapability::User);
            layout.vector_state = Some(VectorStateLayout {
                regs_base: 0x20_000,
                vl_addr: 0x20_200,
                vtype_addr: 0x20_208,
                vstart_addr: 0x20_210,
                simd_count_addr: 0x20_218,
            });
            layout
        };

        let build = |insn: u32| {
            let mut builder = Builder::new(0x1000);
            builder.vector(insn, 0x1000, 0x1004, 0);
            let next = builder.const_i64(0x1004, 0x1000);
            builder.finish(0x1004, next, 1, ExitKind::Dispatch)
        };

        // Helper: confirm the IR carries the variant we expect, then emit
        // the JIT and validate the resulting module. Either step would have
        // panicked under the pre-fix code.
        let check = |insn: u32, label: &str, expected: fn(&crate::ir::Region) -> bool| {
            let region = build(insn);
            assert!(
                expected(&region),
                "{label}: expected decode to produce the named VectorDirect variant"
            );
            let bytes = emit(&region, layout_for(), None).unwrap_or_else(|e| {
                panic!("{label}: direct emit failed: {e:?}")
            });
            wasmparser::Validator::new()
                .validate_all(&bytes)
                .unwrap_or_else(|e| panic!("{label}: module must validate: {e:?}"));
        };

        // Encodings lifted from the existing ir::decode tests so the exact
        // VectorDirect variant is known to match what we expect.
        //   0xcc08_7057 -> vsetivli x0,16,e8,m1,ta,ma -> ConfigImmediate { dest:0, vtype:0xc0, vl:16 }
        //   0x0da0_7557 -> vsetivli x10,8,e8,m1,ta,ma -> ConfigImmediate { dest:10, vtype:0xda, vl:8 }
        //   0x0c80_7057 -> vsetvli x0,x0,e8,m1,ta,ma -> ConfigRetainFull { vtype:0xc8, vlmax:8 }
        //   0x9f01_b457 -> vmv4r.v v8,v16 -> WholeRegisterMove { dest:8, src:16, regs:4 }
        //   0x4280_2557 -> vmv.x.s a0,v8 -> ScalarExtract { dest:10, src:8 }
        //   0x4206_e6d7 -> vmv.s.x v13,a3 -> ScalarInsert { dest:13, src:13 }
        // vsetivli ConfigImmediate cases
        check(0xcc08_7057, "ConfigImmediate x0", |r| matches!(
            r.effects.first(),
            Some(Effect::Vector {
                direct: Some(VectorDirect::ConfigImmediate { destination: 0, .. }),
                ..
            })
        ));
        check(0x0da0_7557, "ConfigImmediate x10", |r| matches!(
            r.effects.first(),
            Some(Effect::Vector {
                direct: Some(VectorDirect::ConfigImmediate { destination: 10, .. }),
                ..
            })
        ));
        // vsetvli x0,x0 ConfigRetainFull
        check(0x0c80_7057, "ConfigRetainFull", |r| matches!(
            r.effects.first(),
            Some(Effect::Vector {
                direct: Some(VectorDirect::ConfigRetainFull { .. }),
                ..
            })
        ));
        // vmv4r.v WholeRegisterMove (vmv1r/vmv2r derived by clearing bits 19-15)
        check(0x9f01_b457, "WholeRegisterMove vmv4r", |r| matches!(
            r.effects.first(),
            Some(Effect::Vector {
                direct: Some(VectorDirect::WholeRegisterMove { registers: 4, .. }),
                ..
            })
        ));
        check(
            0x9f01_b457 - (3u32 << 15),
            "WholeRegisterMove vmv1r",
            |r| matches!(
                r.effects.first(),
                Some(Effect::Vector {
                    direct: Some(VectorDirect::WholeRegisterMove { registers: 1, .. }),
                    ..
                })
            ),
        );
        check(
            0x9f01_b457 - (2u32 << 15),
            "WholeRegisterMove vmv2r",
            |r| matches!(
                r.effects.first(),
                Some(Effect::Vector {
                    direct: Some(VectorDirect::WholeRegisterMove { registers: 2, .. }),
                    ..
                })
            ),
        );
        // vlr.v / vsr.v WholeRegisterMemory. The existing ir tests do not
        // cover these directly, but the decode function accepts the same
        // shape via opcode 0x07/0x27 + aux=8 + fields=1 (NF=0). Build the
        // instruction explicitly so we can pin the exact variant we expect.
        let whole_mem = |load: bool| -> u32 {
            let op: u32 = if load { 0x07 } else { 0x27 };
            let nf: u32 = 0;
            let mew: u32 = 0;
            let mop: u32 = 0;
            let width: u32 = 0;
            op
                | (8u32 << 7)        // vd = v8
                | (width << 12)
                | (u32::from(9u8) << 15) // base = x9
                | (8u32 << 20)       // aux = 8 (whole-register marker)
                | (1u32 << 25)       // vm = 1 (unmasked)
                | (mop << 26)
                | (mew << 28)
                | (nf << 29)
        };
        check(whole_mem(true), "WholeRegisterMemory vlr.v", |r| matches!(
            r.effects.first(),
            Some(Effect::Vector {
                direct: Some(VectorDirect::WholeRegisterMemory { load: true, .. }),
                ..
            })
        ));
        check(whole_mem(false), "WholeRegisterMemory vsr.v", |r| matches!(
            r.effects.first(),
            Some(Effect::Vector {
                direct: Some(VectorDirect::WholeRegisterMemory { load: false, .. }),
                ..
            })
        ));
        // vmv.x.s ScalarExtract
        check(0x4280_2557, "ScalarExtract vmv.x.s", |r| matches!(
            r.effects.first(),
            Some(Effect::Vector {
                direct: Some(VectorDirect::ScalarExtract { .. }),
                ..
            })
        ));
        // vmv.s.x ScalarInsert
        check(0x4206_e6d7, "ScalarInsert vmv.s.x", |r| matches!(
            r.effects.first(),
            Some(Effect::Vector {
                direct: Some(VectorDirect::ScalarInsert { .. }),
                ..
            })
        ));
        // vfmv.f.s / vfmv.s.f encodings derived from the same field layout
        // as the rest of vector_encoding(funct6=0x10, vm=1, vs2, source, format, vd).
        // The FloatScalar* checks use format=1 (extract) and format=5 (insert),
        // mirroring the existing ir test cases.
        let f_scalar_extract: u32 =
            0x57 | (5u32 << 7) | (1u32 << 12) | (8u32 << 20) | (1u32 << 25) | (0x10u32 << 26);
        let f_scalar_insert: u32 =
            0x57 | (16u32 << 7) | (5u32 << 12) | (4u32 << 15) | (1u32 << 25) | (0x10u32 << 26);
        check(f_scalar_extract, "FloatScalarExtract vfmv.f.s", |r| matches!(
            r.effects.first(),
            Some(Effect::Vector {
                direct: Some(VectorDirect::FloatScalarExtract { .. }),
                ..
            })
        ));
        check(f_scalar_insert, "FloatScalarInsert vfmv.s.f", |r| matches!(
            r.effects.first(),
            Some(Effect::Vector {
                direct: Some(VectorDirect::FloatScalarInsert { .. }),
                ..
            })
        ));
    }

        #[test]
    fn known_vector_configuration_is_legal_and_conservative() {
        let m1_e8 = KnownVectorConfig::decode(0xc0, 16).expect("e8,m1 is legal");
        assert_eq!(m1_e8.vsew, 0);
        assert_eq!(m1_e8.span, 1);
        assert_eq!(m1_e8.group_bytes, 16);
        assert_eq!(m1_e8.vlmax, 16);
        assert_eq!(
            known_vector_direct_tail_merge(
                m1_e8,
                VectorDirect::Lane {
                    op: VectorLaneOp::Add,
                    masked: false,
                    destination: 16,
                    source2: Some(8),
                    operand: VectorOperand::Vector(12),
                },
            ),
            Some(false),
        );

        let partial = KnownVectorConfig::decode(0xc0, 7).expect("partial e8,m1 is legal");
        assert_eq!(
            known_vector_direct_tail_merge(
                partial,
                VectorDirect::Lane {
                    op: VectorLaneOp::Add,
                    masked: false,
                    destination: 16,
                    source2: Some(8),
                    operand: VectorOperand::Vector(12),
                },
            ),
            Some(true),
        );
        assert_eq!(
            known_vector_direct_tail_merge(
                partial,
                VectorDirect::SlideOne {
                    up: false,
                    destination: 16,
                    source: 8,
                    scalar: 1,
                },
            ),
            None,
            "partial vslide1 remains on the architectural helper",
        );

        let mf2_e32 = KnownVectorConfig::decode(0xd7, 2).expect("e32,mf2 is legal");
        assert!(mf2_e32.fractional_lmul);
        assert_eq!(mf2_e32.group_bytes, 8);
        assert!(
            KnownVectorConfig::decode(0xdf, 1).is_none(),
            "e64,mf2 is illegal"
        );
        assert!(KnownVectorConfig::decode(1 << 63, 0).is_none());
    }

    #[test]
    fn cached_vector_region_specializes_configuration_established_by_vsetivli() {
        let mut vector_builder = Builder::new(0x1000);
        // vsetivli zero,16,e8,m1,ta,ma; vadd.vv v16,v8,v12
        vector_builder.vector(0xcc08_7057, 0x1000, 0x1004, 0);
        vector_builder.vector(0x0286_0857, 0x1004, 0x1008, 1);
        let next = vector_builder.const_i64(0x1008, 0x1004);
        let vector_region = vector_builder.finish(0x1008, next, 2, ExitKind::Dispatch);

        let mut scalar_builder = Builder::new(0x1008);
        let next = scalar_builder.const_i64(0x100c, 0x1008);
        let scalar_region = scalar_builder.finish(0x100c, next, 1, ExitKind::Dispatch);

        let mut layout = JitLayout::bare();
        layout.mem = Some((4096, 65536));
        layout.vector = Some(VectorCapability::User);
        layout.vector_state = Some(VectorStateLayout {
            regs_base: 0x20_000,
            vl_addr: 0x20_200,
            vtype_addr: 0x20_208,
            vstart_addr: 0x20_210,
            simd_count_addr: 0x20_218,
        });
        let bytes = emit_multi_entry_mode(
            &[(&vector_region, None), (&scalar_region, None)],
            layout,
            true,
            MultiEntryState::RegisterEager,
        )
        .expect("cached configured-vector module emits");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("configured-vector module validates");
        assert_eq!(
            simd_memory_ops(&bytes).1,
            1,
            "known e8 configuration emits only the selected direct body",
        );
    }

    #[test]
    fn emits_a_valid_fuel_metered_loop() {
        // addi x1,x1,1; bne x1,x2,-4
        let words = [0x0010_8093u32, 0xfe20_9ee3u32];
        let code: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let lifted = crate::lift::lift_t1(
            &code,
            0x1000,
            0x1000,
            false,
            crate::lift::FpMode::Disabled,
            false,
        )
        .unwrap();
        let mut layout = JitLayout::bare();
        layout.fuel_addr = 272;
        let bytes = emit(&lifted.ir, layout, lifted.loop_backedge).unwrap();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("generated loop module must validate");
    }

    #[test]
    fn emits_valid_checked_flat_memory_accesses() {
        let mut builder = Builder::new(0x1000);
        let address = builder.read_x(1, 0x1000);
        let loaded = builder.load(address, LoadKind::I32U, 0x1000, 0);
        builder.write_x(2, loaded);
        let four = builder.const_i64(4, 0x1004);
        let next_address = builder.binary(BinaryOp::I64Add, address, four, 0x1004);
        builder.store(next_address, loaded, StoreKind::I64, 0x1004, 1);
        let next = builder.const_i64(0x1008, 0x1004);
        let region = builder.finish(0x1008, next, 2, ExitKind::Dispatch);
        let mut layout = JitLayout::bare();
        layout.mem = Some((4096, 65536));
        let bytes = emit(&region, layout, None).unwrap();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("generated memory module must validate");
    }

    #[test]
    fn emits_a_valid_memory_loop_with_precise_exits() {
        let branch = |rs1: u32, rs2: u32, offset: i32| {
            let immediate = offset as u32 & 0x1fff;
            0x63 | (1 << 12)
                | (rs1 << 15)
                | (rs2 << 20)
                | (((immediate >> 11) & 1) << 7)
                | (((immediate >> 1) & 0xf) << 8)
                | (((immediate >> 5) & 0x3f) << 25)
                | (((immediate >> 12) & 1) << 31)
        };
        // ld x3,0(x1); addi x1,x1,8; addi x2,x2,-1; bne x2,x0,-12
        let words = [0x0000_b183u32, 0x0080_8093, 0xfff1_0113, branch(2, 0, -12)];
        let code: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let lifted = crate::lift::lift_t1(
            &code,
            0x1000,
            0x1000,
            true,
            crate::lift::FpMode::User,
            false,
        )
        .unwrap();
        assert!(lifted.loop_backedge.is_some());
        let mut layout = JitLayout::bare();
        layout.mem = Some((4096, 65536));
        layout.fuel_addr = 272;
        let bytes = emit(&lifted.ir, layout, lifted.loop_backedge).unwrap();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("generated effectful loop module must validate");
    }

    #[test]
    fn emits_valid_full_system_memory_for_side_exit_and_refill_misses() {
        let branch = |rs1: u32, rs2: u32, offset: i32| {
            let immediate = offset as u32 & 0x1fff;
            0x63 | (1 << 12)
                | (rs1 << 15)
                | (rs2 << 20)
                | (((immediate >> 11) & 1) << 7)
                | (((immediate >> 1) & 0xf) << 8)
                | (((immediate >> 5) & 0x3f) << 25)
                | (((immediate >> 12) & 1) << 31)
        };
        // ld x3,0(x1); addi x1,x1,8; addi x2,x2,-1; bne x2,x0,-12
        let words = [0x0000_b183u32, 0x0080_8093, 0xfff1_0113, branch(2, 0, -12)];
        let code: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let lifted = crate::lift::lift_t1(
            &code,
            0x1000,
            0x1000,
            true,
            crate::lift::FpMode::Disabled,
            false,
        )
        .unwrap();
        assert!(lifted.loop_backedge.is_some());
        for refill in [false, true] {
            let bytes = emit(&lifted.ir, system_layout(refill), lifted.loop_backedge).unwrap();
            wasmparser::Validator::new()
                .validate_all(&bytes)
                .unwrap_or_else(|error| panic!("system module (refill={refill}) failed: {error}"));
        }
    }

    #[test]
    fn emits_valid_full_system_fp_state_memory_and_helper_ordering() {
        let branch = |rs1: u32, rs2: u32, offset: i32| {
            let immediate = offset as u32 & 0x1fff;
            0x63 | (1 << 12)
                | (rs1 << 15)
                | (rs2 << 20)
                | (((immediate >> 11) & 1) << 7)
                | (((immediate >> 1) & 0xf) << 8)
                | (((immediate >> 5) & 0x3f) << 25)
                | (((immediate >> 12) & 1) << 31)
        };
        // fld f1,0(x1); fadd.d f1,f1,f2,rne; fsd f1,0(x1);
        // addi x3,x3,-1; bne x3,x0,-16
        let words = [
            0x0000_b087u32,
            0x0220_80d3,
            0x0010_b027,
            0xfff1_8193,
            branch(3, 0, -16),
        ];
        let code: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let lifted = crate::lift::lift_t1(
            &code,
            0x1000,
            0x1000,
            true,
            crate::lift::FpMode::System,
            false,
        )
        .unwrap();
        assert!(lifted.loop_backedge.is_some());
        assert!(lifted
            .ir
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::FpState { dirty: false, .. })));
        let mut layout = system_layout(true);
        layout.f_base = 1024;
        layout.fcsr_addr = 1280;
        layout.mstatus_addr = 1288;
        let bytes = emit(&lifted.ir, layout, lifted.loop_backedge).unwrap();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("generated system FP/memory module must validate");
    }

    #[test]
    fn emits_a_valid_lr_sc_loop_with_a_typed_reservation_import() {
        let branch = |rs1: u32, rs2: u32, offset: i32| {
            let immediate = offset as u32 & 0x1fff;
            0x63 | (1 << 12)
                | (rs1 << 15)
                | (rs2 << 20)
                | (((immediate >> 11) & 1) << 7)
                | (((immediate >> 1) & 0xf) << 8)
                | (((immediate >> 5) & 0x3f) << 25)
                | (((immediate >> 12) & 1) << 31)
        };
        let amo = |funct5: u32, rd: u32, rs1: u32, rs2: u32| {
            0x2f | (rd << 7) | (3 << 12) | (rs1 << 15) | (rs2 << 20) | (funct5 << 27)
        };
        // lr.d x3,(x1); addi x3,x3,1; sc.d x4,x3,(x1);
        // addi x2,x2,-1; bne x2,x0,-16
        let words = [
            amo(0x02, 3, 1, 0),
            0x0011_8193,
            amo(0x03, 4, 1, 3),
            0xfff1_0113,
            branch(2, 0, -16),
        ];
        let code: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let lifted =
            crate::lift::lift_t1(&code, 0x1000, 0x1000, true, crate::lift::FpMode::User, true)
                .unwrap();
        assert!(lifted.loop_backedge.is_some());
        assert!(lifted.ir.has_reservation_helper());
        let mut layout = JitLayout::bare();
        layout.mem = Some((4096, 65536));
        layout.reservation = Some(ReservationCapability::User);
        layout.fuel_addr = 272;
        let bytes = emit(&lifted.ir, layout, lifted.loop_backedge).unwrap();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("generated LR/SC module must validate");
    }

    #[test]
    fn emits_a_guarded_trace_without_requiring_guest_memory() {
        // addi x1,x0,1; beq x2,x0,+8; addi x1,x1,2; ecall
        let words = [0x0010_0093u32, 0x0001_0463, 0x0020_8093, 0x0000_0073];
        let code: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let lifted = crate::lift::lift_t1(
            &code,
            0x1000,
            0x1000,
            false,
            crate::lift::FpMode::Disabled,
            false,
        )
        .unwrap();
        assert!(lifted.ir.has_effects());
        let bytes = emit(&lifted.ir, JitLayout::bare(), None).unwrap();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("generated guarded trace module must validate");
    }

    #[test]
    fn refuses_effectful_regions_without_a_memory_capability() {
        let mut builder = Builder::new(0x1000);
        let address = builder.read_x(1, 0x1000);
        let loaded = builder.load(address, LoadKind::I64, 0x1000, 0);
        builder.write_x(2, loaded);
        let next = builder.const_i64(0x1004, 0x1000);
        let region = builder.finish(0x1004, next, 1, ExitKind::Dispatch);
        assert!(emit(&region, JitLayout::bare(), None).is_err());
    }

    #[test]
    fn emits_valid_non_trapping_division_paths() {
        let mut builder = Builder::new(0x1000);
        let lhs = builder.read_x(1, 0x1000);
        let rhs = builder.read_x(2, 0x1000);
        let div = builder.divide(DivideOp::I64DivS, lhs, rhs, 0x1000);
        let rem = builder.divide(DivideOp::I64RemU, lhs, rhs, 0x1004);
        builder.write_x(3, div);
        builder.write_x(4, rem);
        let lhs32 = builder.wrap_i32(lhs, 0x1008);
        let rhs32 = builder.wrap_i32(rhs, 0x1008);
        let div32 = builder.divide(DivideOp::I32DivU, lhs32, rhs32, 0x1008);
        let div32 = builder.extend_i32_s(div32, 0x1008);
        builder.write_x(5, div32);
        let next = builder.const_i64(0x100c, 0x1008);
        let region = builder.finish(0x100c, next, 3, ExitKind::Dispatch);
        let bytes = emit(&region, JitLayout::bare(), None).unwrap();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("generated guarded division module must validate");
    }

    #[test]
    fn emits_valid_fp_register_and_memory_state() {
        // fmv.d.x f1,x2; fsd f1,0(x3); fld f4,0(x3); fmv.x.d x5,f4; ecall
        let words = [
            0xf201_00d3u32,
            0x0011_b027,
            0x0001_b207,
            0xe202_82d3,
            0x0000_0073,
        ];
        let code: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let lifted = crate::lift::lift_t1(
            &code,
            0x1000,
            0x1000,
            true,
            crate::lift::FpMode::User,
            false,
        )
        .unwrap();
        let mut layout = JitLayout::bare();
        layout.f_base = 1024;
        layout.fcsr_addr = 1280;
        layout.mem = Some((4096, 65536));
        let bytes = emit(&lifted.ir, layout, lifted.loop_backedge).unwrap();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("generated FP state module must validate");
    }

    #[test]
    fn emits_a_valid_exact_fp_helper_call() {
        let words = [0x0220_81d3u32, 0x0000_0073]; // fadd.d f3,f1,f2,rne; ecall
        let code: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let lifted = crate::lift::lift_t1(
            &code,
            0x1000,
            0x1000,
            true,
            crate::lift::FpMode::User,
            false,
        )
        .unwrap();
        let mut layout = JitLayout::bare();
        layout.f_base = 1024;
        layout.fcsr_addr = 1280;
        let bytes = emit(&lifted.ir, layout, lifted.loop_backedge).unwrap();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("generated exact FP helper module must validate");
    }

    #[test]
    fn emits_a_valid_fp_helper_loop_with_carried_fcsr() {
        let branch = |rs1: u32, rs2: u32, offset: i32| {
            let immediate = offset as u32 & 0x1fff;
            0x63 | (1 << 12)
                | (rs1 << 15)
                | (rs2 << 20)
                | (((immediate >> 11) & 1) << 7)
                | (((immediate >> 1) & 0xf) << 8)
                | (((immediate >> 5) & 0x3f) << 25)
                | (((immediate >> 12) & 1) << 31)
        };
        // fadd.d f1,f1,f2,rne; addi x3,x3,-1; bne x3,x0,-8
        let words = [0x0220_80d3u32, 0xfff1_8193, branch(3, 0, -8)];
        let code: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let lifted = crate::lift::lift_t1(
            &code,
            0x1000,
            0x1000,
            true,
            crate::lift::FpMode::User,
            false,
        )
        .unwrap();
        assert!(lifted.loop_backedge.is_some());
        let mut layout = JitLayout::bare();
        layout.f_base = 1024;
        layout.fcsr_addr = 1280;
        layout.fuel_addr = 1288;
        let bytes = emit(&lifted.ir, layout, lifted.loop_backedge).unwrap();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("generated exact FP loop module must validate");
    }
}
