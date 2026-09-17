# Changelog

All notable user-visible changes to rv64.js will be recorded here. The project
uses [Semantic Versioning](https://semver.org/) once a release is tagged.

## Unreleased

- Alpine WANIX guest profiles now include Peri Agent v3.16.5 on x86, RISC-V, and ARM64.
- RV64GCV interpreter and WebAssembly JIT for user-mode and full-system use,
  including the mandatory RVV 1.0 instruction surface for the selected
  VLEN=128/ELEN=64 machine and architecture-general direct vector lowering.
- Measurement-valid RV64GCV and scalar JIT scorecards now each record 13/13
  wins or matches against pinned copy/v86; the protected WANIX browser gate
  also passes without benchmark, binary, PC, symbol, or opcode recognizers.
- TinyEMU-compatible Linux machine and modern OpenSBI virt machine.
- Virtio console, block, 9p, and network devices.
- Native and browser HTTP proxy paths, including HTTPS CONNECT interception.
- Stable JavaScript networking modes (`fetch`, `wisp`, `wsproxy`, `inbrowser`, `external`,
  and `none`), with the HTTP proxy enabled by default for Linux machines.
- Firmware-free direct Linux boot through emulator-provided SBI services.
- Alpine 3.24 release guest with automatic proxy/CA setup and a tested `apk`
  package-index path.
- Correct large HTTPS response delivery through the guest proxy; rustls output
  is drained incrementally instead of truncating at its plaintext buffer limit.
- ACK-clocked proxy response delivery and lossless host-to-NIC admission keep
  large browser downloads bounded while waiting for guest progress.
- On-demand public JIT snapshots and opt-in detailed profiles, plus a live demo
  status panel. Ordinary embedders incur no timer or polling work unless they
  call the diagnostic methods themselves.
- Differential ISA, architecture-signature, lockstep, Wasm, and boot
  validation suites.
