import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const read = (path) => readFileSync(resolve(root, path), "utf8");
const required = (source, text, name) => {
  assert.ok(source.includes(text), `${name} is missing ${text}`);
};

const flake = read("flake.nix");
required(flake, 'version = "7.2.6";', "flake kernel version");
required(flake, "ignoreConfigErrors = false;", "flake strict kernel config");
for (const attribute of [
  "virt-kernel-fast",
  "virt-kernel-fast-container",
  "v86-kernel",
  "v86-kernel-container",
  "arm64-kernel",
  "arm64-kernel-container",
]) {
  required(flake, `packages.${attribute}`, "flake package");
}

for (const config of [
  "kernel/rv64-config.nix",
  "kernel/rv64-container-config.nix",
  "kernel/x86-v86-config.nix",
  "kernel/x86-v86-container-config.nix",
  "kernel/arm64-config.nix",
  "kernel/arm64-container-config.nix",
]) {
  const source = read(config);
  required(source, "IKCONFIG = yes;", config);
  required(source, "IKCONFIG_PROC = yes;", config);
}
for (const config of [
  "kernel/x86-v86-config.nix",
  "kernel/x86-v86-container-config.nix",
]) {
  const source = read(config);
  required(source, "COMPAT_32BIT_TIME = yes;", config);
  for (const option of [
    "NAMESPACES",
    "UTS_NS",
    "IPC_NS",
    "USER_NS",
    "PID_NS",
    "NET_NS",
    "CGROUPS",
    "CGROUP_SCHED",
    "FAIR_GROUP_SCHED",
  ]) {
    required(source, `  ${option} = yes;`, `${config} mount namespace ${option}`);
  }
}
for (const config of [
  "kernel/rv64-config.nix",
  "kernel/rv64-container-config.nix",
  "kernel/x86-v86-config.nix",
  "kernel/x86-v86-container-config.nix",
  "kernel/arm64-config.nix",
  "kernel/arm64-container-config.nix",
]) {
  required(read(config), "MULTIUSER = yes;", `${config} init capabilities`);
}

for (const config of [
  "kernel/rv64-container-config.nix",
  "kernel/x86-v86-container-config.nix",
  "kernel/arm64-container-config.nix",
]) {
  const source = read(config);
  for (const option of [
    "CGROUP_CPUACCT",
    "CGROUP_FREEZER",
    "KEYS",
    "NETFILTER_XT_MATCH_ADDRTYPE",
    "NETFILTER_XT_MATCH_CONNTRACK",
    "NETFILTER_XT_MARK",
    "NF_NAT_MASQUERADE",
    "NETFILTER_XT_TARGET_MASQUERADE",
    "NF_TABLES",
    "NFT_CT",
    "NFT_MASQ",
  ]) {
    required(source, `  ${option} = yes;`, `${config} container ${option}`);
  }
}

const build = read("tools/build-wanix-release-assets.sh");
for (const profile of ["minimal", "crush", "python", "nodejs", "claude", "peri", "zero", "pi", "golang", "container", "container-full"]) {
  required(build, `    ${profile})`, "guest build profile");
}
required(build, "ALPINE_TAG=3.24", "guest Alpine version");
required(build, 'overlay_archive="$output_dir/wanix-overlay-${archive_arch}.tgz"', "per-architecture overlay archive variable");
for (const binary of ["getfattr", "podman", "python3", "strace", "tmux", "uv", "node", "npm", "go"]) {
  required(build, `tar -tf "$archive" --wildcards "usr/bin/${binary}" >/dev/null`, `${binary} rootfs verification`);
}
required(build, 'tar -tf "$archive" --wildcards \'boot/Image\' >/dev/null 2>&1', "rootfs kernel-free assertion (would fail build)");
required(build, 'tar -tf "$overlay_archive" --wildcards "usr/local/bin/crush" >/dev/null', "Crush overlay verification");
required(build, 'tar -tf "$archive" --wildcards "usr/bin/rg" >/dev/null', "ripgrep rootfs verification");
required(build, 'tar -tf "$overlay_archive" --wildcards "usr/local/bin/claude-code-best" >/dev/null', "Claude overlay verification");
required(build, 'tar -tf "$overlay_archive" --wildcards "usr/local/bin/peri" >/dev/null', "Peri overlay verification");
required(build, 'tar -tf "$overlay_archive" --wildcards "usr/local/bin/zero" >/dev/null', "Zero overlay verification");
required(build, 'tar -tf "$overlay_archive" --wildcards "usr/local/bin/zero-seccomp" >/dev/null', "Zero seccomp verification");
required(build, 'tar -tf "$overlay_archive" --wildcards "usr/local/bin/zero-linux-sandbox" >/dev/null', "Zero sandbox verification");
required(build, 'tar -tf "$overlay_archive" --wildcards "usr/local/lib/zero/bin/zero.js" >/dev/null', "Zero runtime verification");
required(build, 'tar -tf "$overlay_archive" --wildcards "usr/local/bin/pi" >/dev/null', "Pi overlay verification");
assert.doesNotMatch(build, /tar -tzf[^\n]*\|\s*grep/, "archive verification must not use a SIGPIPE-prone pipeline");

const bundle = read("integrations/wanix/build-linux-bundle.sh");
const overlay = read("integrations/wanix/build-wanix-overlay.sh");
const guestInit = read("integrations/wanix/guest/init");
required(guestInit, 'mount_fs() {\n    /bin/busybox mount "$@"\n}', "guest direct BusyBox mount helper");
required(guestInit, "mount_fs -t proc none /proc", "guest proc mount");
required(guestInit, "mount_fs -t tmpfs tmpfs /tmp", "guest tmpfs mount");
required(guestInit, "/bin/busybox ifconfig eth0", "guest direct BusyBox network setup");
required(guestInit, 'exec setsid -c "$SHELL" -i', "guest configured shell");
required(overlay, 'go build -C "$wanix_src"', "overlay Go build");
required(overlay, 'wanix-overlay-$arch.XXXXXX', "overlay-only temporary workspace");

required(bundle, 'WANIX_ROOTFS=arch', "Arch rootfs opt-in");
required(bundle, 'WANIX_ROOTFS_TARBALL', "Arch rootfs tarball path");
required(bundle, 'tar -xzf "$arch_tarball" -C "$rootfs"', "Arch rootfs extraction");
required(bundle, 'cp "$here/arch-configs/pacman.conf" "$rootfs/etc/pacman.conf"', "Arch pacman.conf override");
required(bundle, 'cp "$here/arch-configs/mirrorlist" "$rootfs/etc/pacman.d/mirrorlist"', "Arch mirrorlist override");
required(bundle, 'cp "$here/arch-configs/mirrorlist.riscv64" "$rootfs/etc/pacman.d/mirrorlist"', "Arch riscv64 mirrorlist override");

// Arch rootfs tarballs now come from btwiuse/archlinux; the wanix
// guest bundle overlays pacman.conf + mirrorlist on top so first-boot
// pacman hits the curated mirrors instead of upstream defaults.
const mirrorlistNames = ["mirrorlist", "mirrorlist.riscv64"];
for (const name of mirrorlistNames) {
    const source = read(`integrations/wanix/arch-configs/${name}`);
    required(source, "Server = https://", `${name} must declare a mirror`);
}
required(read("integrations/wanix/arch-configs/pacman.conf"), "Include = /etc/pacman.d/mirrorlist", "pacman.conf must include the mirror list");
required(read("integrations/wanix/arch-configs/pacman-bootstrap.conf"), "Include = /etc/pacman.d/mirrorlist", "pacman-bootstrap.conf must include the mirror list");
required(bundle, "crush_version=v0.94.0", "Crush release version");
for (const pair of [
  ["riscv64", "b2798cd2d44312714bb389d3cd3de12fbbd80c74f4b912b635c1855fc2e81676"],
  ["i386", "2f36756048d3f5ee5f13bb2512492c487b573781e18d4a5afe34a244fc29377c"],
  ["arm64", "ed2bf9bfa3e248ce917478f247d634ea942597299c2f05233ddbd356a932276a"],
]) {
  required(bundle, `crush_arch=${pair[0]}`, `Crush ${pair[0]} archive mapping`);
  required(bundle, `crush_sha256=${pair[1]}`, `Crush ${pair[0]} archive checksum`);
}
required(bundle, 'test -x "$rootfs/usr/local/bin/crush"', "Crush rootfs validation");
required(bundle, "chmod -R u+rwX \"$tmp\"", "temporary guest cleanup permissions");
required(bundle, "profile_packages=(python3 uv)", "Python profile package set");
required(bundle, "profile_packages=(nodejs-current npm)", "Node.js profile package set");
required(bundle, "profile_packages=(nodejs-current npm ripgrep)", "Claude profile package set");
required(bundle, "npm --prefix /target/usr/local install --global claude-code-best", "Claude Code Best installation");
required(bundle, 'test -x "$rootfs/usr/bin/rg"', "ripgrep rootfs validation");
required(bundle, 'test -L "$rootfs/usr/local/bin/claude-code-best"', "Claude command validation");
required(bundle, "peri_version=agent-v3.16.5", "Peri release version");
for (const pair of [
  ["riscv64", "acb827a9d1d4f97eb57ee80b60de9701054533558a29854ca1f1c9731c473e36"],
  ["i686", "96978b393068b051069c60d72399b30d53896c6a1f531ac14b2869e92ae7596f"],
  ["aarch64", "0c39d13cd13cb058e9888afd918fe2debb1a71060d3c55b6ec1bffdb2b02c385"],
]) {
  required(bundle, `peri_arch=${pair[0]}`, `Peri ${pair[0]} archive mapping`);
  required(bundle, `peri_sha256=${pair[1]}`, `Peri ${pair[0]} archive checksum`);
}
required(bundle, 'test -x "$rootfs/usr/local/bin/peri"', "Peri rootfs validation");
required(bundle, "zero_version=v0.9.0", "Zero release version");
for (const pair of [
  ["riscv64", "e7ce4e66e230661056176a57dc0018b32b799f2ce9d8946d9625b7dfb8ada4af"],
  ["x86", "ada2844dad1251da033b13ebc689342371e79b92eb6b19fddbddd36b6d2a9810"],
  ["arm64", "61d8b5d399c068dd14db258a42889c274ebe69a7fd621f5c0fce8ce25cdff41b"],
]) {
  required(bundle, `zero_arch=${pair[0]}`, `Zero ${pair[0]} archive mapping`);
  required(bundle, `zero_sha256=${pair[1]}`, `Zero ${pair[0]} archive checksum`);
}
required(bundle, 'test -x "$rootfs/usr/local/bin/zero"', "Zero rootfs validation");
required(bundle, 'test -x "$rootfs/usr/local/bin/zero-seccomp"', "Zero seccomp rootfs validation");
required(bundle, 'test -x "$rootfs/usr/local/bin/zero-linux-sandbox"', "Zero sandbox rootfs validation");
required(bundle, 'test -f "$rootfs/usr/local/lib/zero/bin/zero.js"', "Zero runtime rootfs validation");
required(bundle, "profile_packages=(nodejs-current npm)", "Pi profile package set");
required(bundle, "npm --prefix /target/usr/local install --global --ignore-scripts @earendil-works/pi-coding-agent", "Pi Coding Agent installation");
required(bundle, 'test -L "$rootfs/usr/local/bin/pi"', "Pi command validation");
required(bundle, "profile_packages=(go)", "Go profile package set");
required(bundle, "profile_packages=(attr ca-certificates podman python3 strace tmux uv)", "full OCI, Python, multitasking, and debugging package set");
for (const binary of ["getfattr", "podman", "python3", "strace", "tmux", "uv", "node", "npm"]) {
  required(bundle, `test -x "$rootfs/usr/bin/${binary}"`, `${binary} rootfs validation`);
}
required(bundle, 'test -L "$rootfs/usr/bin/go"', "Go command symlink validation");
required(bundle, 'test -x "$rootfs/usr/lib/go/bin/go"', "Go executable validation");
assert.doesNotMatch(bundle, /profile_packages=.*\b(docker|docker-proxy|dockerd)\b/, "full image must not include Docker");
required(bundle, ': >"$overlay/etc/wanix-container"', "container guest marker");

const init = read("integrations/wanix/guest/init");
required(init, 'if [ -f /etc/wanix-container ]; then', "container forwarding guard");
required(init, 'echo 1 >/proc/sys/net/ipv4/ip_forward', "container IPv4 forwarding");

const makefile = read("integrations/wanix/Makefile");
for (const archive of [
  "wanix-linux-rv64.tgz",
  "wanix-linux-rv64-crush.tgz",
  "wanix-linux-rv64-python.tgz",
  "wanix-linux-rv64-nodejs.tgz",
  "wanix-linux-rv64-claude.tgz",
  "wanix-linux-rv64-peri.tgz",
  "wanix-linux-rv64-zero.tgz",
  "wanix-linux-rv64-pi.tgz",
  "wanix-linux-rv64-golang.tgz",
  "wanix-linux-rv64-container.tgz",
  "wanix-linux-rv64-container-full.tgz",
  "wanix-linux-x86.tgz",
  "wanix-linux-x86-crush.tgz",
  "wanix-linux-x86-python.tgz",
  "wanix-linux-x86-nodejs.tgz",
  "wanix-linux-x86-claude.tgz",
  "wanix-linux-x86-peri.tgz",
  "wanix-linux-x86-zero.tgz",
  "wanix-linux-x86-pi.tgz",
  "wanix-linux-x86-golang.tgz",
  "wanix-linux-x86-container.tgz",
  "wanix-linux-x86-container-full.tgz",
  "wanix-linux-arm64.tgz",
  "wanix-linux-arm64-crush.tgz",
  "wanix-linux-arm64-python.tgz",
  "wanix-linux-arm64-nodejs.tgz",
  "wanix-linux-arm64-claude.tgz",
  "wanix-linux-arm64-peri.tgz",
  "wanix-linux-arm64-zero.tgz",
  "wanix-linux-arm64-pi.tgz",
  "wanix-linux-arm64-golang.tgz",
  "wanix-linux-arm64-container.tgz",
  "wanix-linux-arm64-container-full.tgz",
]) {
  required(makefile, `$(DIST)/${archive}:`, "guest archive target");
}

const releaseWorkflow = read(".github/workflows/release.yml");
required(releaseWorkflow, 'name: rv64.js and WANIX release', "unified release workflow");
required(releaseWorkflow, '- "v[0-9]+.[0-9]+.[0-9]+"', "semver release trigger");
required(releaseWorkflow, "arch: [riscv64, x86, arm64]", "guest workflow architecture matrix");
required(releaseWorkflow, "profile: [minimal, crush, python, nodejs, claude, peri, zero, pi, golang, container, container-full]", "guest workflow profile matrix");
required(releaseWorkflow, "grep -qx 'CONFIG_MULTIUSER=y' \"$config_path\"", "kernel capability release check");
required(releaseWorkflow, 'name: Create semver release', "semver release preparation job");
required(releaseWorkflow, 'needs: prepare-release', "guest release dependency");
required(releaseWorkflow, 'Publish guest archive immediately', "independent guest archive publication");
required(releaseWorkflow, 'gh release upload "$release_tag"', "guest workflow release upload");
required(releaseWorkflow, 'name: Publish WANIX rv64 archive', "rv64 archive publish job");
required(releaseWorkflow, 'needs: [prepare-release, build-rv64-archive]', "rv64 archive publish dependency");
required(releaseWorkflow, 'contents: write', "guest build job needs write scope for gh release upload");
required(releaseWorkflow, 'permissions:\n      contents: write\n    strategy:', "guest build job declares explicit write permission");
required(releaseWorkflow, 'gh release edit "$release_tag" \\\n                --repo "$GITHUB_REPOSITORY" \\\n                --draft=false', "prepare-release promotes the auto-created draft to published");
required(releaseWorkflow, 'gh release upload "$release_tag" target/release/rv64.tgz', "rv64 archive release asset");
required(releaseWorkflow, 'cp "$kernel" "$staging/boot/Image"', "unified kernel archive path");
required(releaseWorkflow, 'rv64-kernel-x86-*) ln -s Image "$staging/boot/bzImage"', "v86 kernel archive compatibility link");
required(releaseWorkflow, 'tar -tzf "$kernel.tgz" | grep -qx "boot/Image"', "kernel archive layout verification");
required(releaseWorkflow, 'rv64-kernel-x86-*) tar -tzf "$kernel.tgz" | grep -qx "boot/bzImage"', "v86 kernel archive link verification");
required(releaseWorkflow, '"${expected[@]/%/.tgz}"', "kernel archive checksums");
required(releaseWorkflow, 'needs: [build-library, publish-rv64-archive]', "library publish dependency");
required(releaseWorkflow, '"rv64.js-${release_tag#v}.tar.gz"', "library release asset");
assert.doesNotMatch(releaseWorkflow, /wanix-(guest|rv64)-[^\n]*tag/i, "legacy WANIX release tag family");

console.log("WANIX release contract: PASS");
