{ lib }:

# Linux 6.12 configuration contract for the rv64.js riscv-virt machine.
#
# This deliberately starts from allnoconfig. Every requested option must be
# justified by hardware rv64.js implements or functionality used by the Alpine
# demo. The Nix build is strict: an option that Kconfig cannot satisfy fails the
# build instead of silently growing a general-purpose distro kernel.
with lib.kernel;
{
  # Single RV64GCV hart entered through OpenSBI or rv64.js's direct SBI.
  MMU = yes;
  FPU = yes;
  RISCV_SBI = yes;
  RISCV_ISA_C = yes;
  # rv64.js always implements misaligned memory accesses efficiently. Declare
  # that fixed platform contract instead of benchmarking it on every boot.
  NONPORTABLE = yes;
  RISCV_EFFICIENT_UNALIGNED_ACCESS = yes;
  RISCV_PROBE_UNALIGNED_ACCESS = no;
  RISCV_TIMER = yes;
  RISCV_INTC = yes;
  SIFIVE_PLIC = yes;
  RISCV_ISA_V = yes;
  SMP = no;

  # Small deterministic kernel; 100 Hz is sufficient for this VM and avoids
  # spending guest time on a 250 Hz scheduler tick.
  CC_OPTIMIZE_FOR_SIZE = yes;
  HZ_100 = yes;
  TINY_RCU = yes;
  MODULES = no;

  # Observable failures and a reproducible embedded config, without the broad
  # debug/tracing/perf infrastructure from the distro-derived kernel.
  PRINTK = yes;
  BUG = yes;
  IKCONFIG = yes;
  KERNEL_GZIP = yes;
  IKCONFIG_PROC = yes;
  BLK_DEV_INITRD = yes;

  # Normal musl/BusyBox process ABI used by Alpine and apk.
  BINFMT_ELF = yes;
  BINFMT_SCRIPT = yes;
  ELF_CORE = yes;
  FUTEX = yes;
  EPOLL = yes;
  SIGNALFD = yes;
  TIMERFD = yes;
  EVENTFD = yes;
  AIO = yes;
  ADVISE_SYSCALLS = yes;
  FHANDLE = yes;
  INOTIFY_USER = yes;
  SYSVIPC = yes;

  NAMESPACES = yes;
  UTS_NS = yes;
  IPC_NS = yes;
  USER_NS = yes;
  PID_NS = yes;
  NET_NS = yes;
  POSIX_MQUEUE = yes;
  SECCOMP = yes;
  SECCOMP_FILTER = yes;
  BPF = yes;
  BPF_SYSCALL = yes;

  CGROUPS = yes;
  CGROUP_SCHED = yes;
  FAIR_GROUP_SCHED = yes;
  CFS_BANDWIDTH = yes;
  CGROUP_PIDS = yes;
  CGROUP_CPUACCT = yes;
  CGROUP_FREEZER = yes;
  CPUSETS = yes;
  MEMCG = yes;
  BLK_CGROUP = yes;
  CGROUP_DEVICE = yes;
  CGROUP_BPF = yes;

  KEYS = yes;
  OVERLAY_FS = yes;
  BRIDGE = yes;
  BRIDGE_NETFILTER = yes;
  VETH = yes;
  NETFILTER = yes;
  NETFILTER_ADVANCED = yes;
  NETFILTER_XTABLES = yes;
  NETFILTER_XT_MATCH_ADDRTYPE = yes;
  NETFILTER_XT_MATCH_CONNTRACK = yes;
  NETFILTER_XT_MARK = yes;
  NF_CONNTRACK = yes;
  NF_NAT = yes;
  IP_NF_IPTABLES = yes;
  IP_NF_FILTER = yes;
  IP_NF_MANGLE = yes;
  IP_NF_RAW = yes;
  IP_NF_NAT = yes;
  IP_NF_TARGET_MASQUERADE = yes;

  # Console and dynamic /dev population. rv64.js implements one ns16550 UART.
  TTY = yes;
  HVC_DRIVER = yes;
  UNIX98_PTYS = yes;
  SERIAL_8250 = yes;
  SERIAL_8250_CONSOLE = yes;
  SERIAL_OF_PLATFORM = yes;
  DEVTMPFS = yes;
  DEVTMPFS_MOUNT = yes;

  # rv64.js exposes block and network devices only through virtio-mmio.
  BLOCK = yes;
  BLK_DEV = yes;
  NETDEVICES = yes;
  ETHERNET = yes;
  VIRTIO = yes;
  VIRTIO_MENU = yes;
  VIRTIO_MMIO = yes;
  VIRTIO_BLK = yes;
  VIRTIO_NET = yes;
  VIRTIO_CONSOLE = yes;
  VSOCKETS = yes;
  VIRTIO_VSOCKETS = yes;

  # Alpine root disk and the host-provided proxy-CA mount.
  EXT4_FS = yes;
  NET_9P = yes;
  NET_9P_VIRTIO = yes;
  "9P_FS" = yes;
  PROC_FS = yes;
  SYSFS = yes;
  TMPFS = yes;
  TMPFS_POSIX_ACL = yes;
  TMPFS_XATTR = yes;

  # IPv4 TCP/UDP plus packet sockets for udhcpc. TLS, signatures and apk index
  # decompression execute in userspace and do not require kernel crypto suites.
  NET = yes;
  PACKET = yes;
  UNIX = yes;
  INET = yes;
  IPV6 = no;
  INET_DIAG = no;
  PTP_1588_CLOCK = no;
  PPS = no;

  # Device-tree RTC at the only RTC address rv64.js implements.
  RTC_CLASS = yes;
  RTC_HCTOSYS = yes;
  RTC_DRV_GOLDFISH = yes;
}
