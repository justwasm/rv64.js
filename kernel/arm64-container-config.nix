{ lib }:
with lib.kernel;
{
  ARM64 = yes;
  MMU = yes;
  MODULES = no;
  CC_OPTIMIZE_FOR_SIZE = yes;
  HZ_100 = yes;

  PRINTK = yes;
  BUG = yes;
  IKCONFIG = yes;
  IKCONFIG_PROC = yes;

  MULTIUSER = yes;
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

  BLK_DEV_INITRD = yes;
  DEVTMPFS = yes;
  DEVTMPFS_MOUNT = yes;
  TTY = yes;
  UNIX98_PTYS = yes;
  SERIAL_8250 = yes;
  SERIAL_8250_CONSOLE = yes;

  BLOCK = yes;
  NET = yes;
  PACKET = yes;
  UNIX = yes;
  INET = yes;
  NETDEVICES = yes;
  ETHERNET = yes;
  VIRTIO = yes;
  VIRTIO_MENU = yes;
  VIRTIO_MMIO = yes;
  VIRTIO_NET = yes;
  VIRTIO_CONSOLE = yes;
  VSOCKETS = yes;
  VIRTIO_VSOCKETS = yes;
  NET_9P = yes;
  NET_9P_VIRTIO = yes;
  "9P_FS" = yes;

  PROC_FS = yes;
  SYSFS = yes;
  TMPFS = yes;
  TMPFS_POSIX_ACL = yes;
  TMPFS_XATTR = yes;

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
  NF_NAT_MASQUERADE = yes;
  NETFILTER_XT_TARGET_MASQUERADE = yes;
  IP_NF_IPTABLES = yes;
  NF_TABLES = yes;
  NFT_CT = yes;
  NFT_MASQ = yes;
}
