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
  required(read(config), "COMPAT_32BIT_TIME = yes;", config);
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
required(
  build,
  'archive="$output_dir/wanix-linux-${archive_arch}${profile_suffix}.tgz"',
  "archive verification",
);
required(build, "tar -tf \"$archive\" --wildcards \"boot/$kernel_name\" >/dev/null", "archive verification");
for (const binary of ["getfattr", "podman", "python3", "strace", "tmux", "uv", "node", "npm", "go"]) {
  required(build, `tar -tf "$archive" --wildcards "usr/bin/${binary}" >/dev/null`, `${binary} archive verification`);
}
required(build, 'tar -tf "$archive" --wildcards "usr/local/bin/crush" >/dev/null', "Crush archive verification");
required(build, 'tar -tf "$archive" --wildcards "usr/bin/rg" >/dev/null', "ripgrep archive verification");
required(build, 'tar -tf "$archive" --wildcards "usr/local/bin/claude-code-best" >/dev/null', "Claude archive verification");
required(build, 'tar -tf "$archive" --wildcards "usr/local/bin/peri" >/dev/null', "Peri archive verification");
required(build, 'tar -tf "$archive" --wildcards "usr/local/bin/zero" >/dev/null', "Zero archive verification");
required(build, 'tar -tf "$archive" --wildcards "usr/local/bin/zero-seccomp" >/dev/null', "Zero seccomp verification");
required(build, 'tar -tf "$archive" --wildcards "usr/local/bin/zero-linux-sandbox" >/dev/null', "Zero sandbox verification");
required(build, 'tar -tf "$archive" --wildcards "usr/local/lib/zero/bin/zero.js" >/dev/null', "Zero runtime verification");
required(build, 'tar -tf "$archive" --wildcards "usr/local/bin/pi" >/dev/null', "Pi archive verification");
assert.doesNotMatch(build, /tar -tzf[^\n]*\|\s*grep/, "archive verification must not use a SIGPIPE-prone pipeline");

const bundle = read("integrations/wanix/build-linux-bundle.sh");

required(bundle, 'WANIX_ROOTFS=arch', "Arch rootfs opt-in");
required(bundle, 'rootfs_nix="${WANIX_ROOTFS_NIX:-', "Arch rootfs Nix output lookup");
required(bundle, 'arch_subdir="$rootfs_nix/$guest_arch"', "Arch rootfs arch subdir");
required(bundle, 'cp "$here/arch-configs/pacman.conf" "$rootfs/etc/pacman.conf"', "Arch pacman.conf override");
required(bundle, 'cp "$here/arch-configs/mirrorlist" "$rootfs/etc/pacman.d/mirrorlist"', "Arch mirrorlist override");
required(bundle, 'cp "$here/arch-configs/mirrorlist.riscv64" "$rootfs/etc/pacman.d/mirrorlist"', "Arch riscv64 mirrorlist override");

const flakeArch = read("flake.nix");
required(flakeArch, "packages.arch-bootstrap-riscv64", "flake arch riscv64 recipe");
required(flakeArch, "packages.arch-bootstrap-aarch64", "flake arch aarch64 recipe");
required(flakeArch, "packages.arch-bootstrap-i686", "flake arch i686 recipe");
required(flakeArch, "packages.arch-recipe", "flake arch bundle recipe");
required(flakeArch, "riscv.mirror.pkgbuild.com/images/archriscv-2026-08-27.tar.zst", "flake arch riscv64 url");
required(flakeArch, "ca.us.mirror.archlinuxarm.org/os/ArchLinuxARM-aarch64-latest.tar.gz", "flake arch aarch64 url");
required(flakeArch, "pkgs.pkgsi686Linux.pacstrap", "flake arch i686 pacstrap builder");
required(flakeArch, "qemu-i386-static", "flake arch i686 qemu interpreter");
// The i686 mirror lives in the pacstrap mirrorlist (pulled at build
// time), not the flake directly.
const mirrorlistI686 = read("integrations/wanix/arch-configs/mirrorlist.i686");
required(mirrorlistI686, "mirror.ufscar.br/archlinux32", "i686 mirrorlist must declare ufscar");
required(mirrorlistI686, "$repo/os/$arch", "i686 mirrorlist must use $repo/$arch placeholders");

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
  ["riscv64", "e1c15813c2f7a73e7b980f839eb0224c490887f51cb601d75b6245b62d0f891c"],
  ["i686", "4caa76cd61cf959c9814233dd5c333af1194a31c5cb4d0697b748ac256c7e406"],
  ["aarch64", "93bebb64cd6624095d4f1456647ee01e029849f35ecf90f6a32ef1a6158678b6"],
]) {
  required(bundle, `peri_arch=${pair[0]}`, `Peri ${pair[0]} archive mapping`);
  required(bundle, `peri_sha256=${pair[1]}`, `Peri ${pair[0]} archive checksum`);
}
required(bundle, 'test -x "$rootfs/usr/local/bin/peri"', "Peri command validation");
required(bundle, "zero_version=v0.9.0", "Zero release version");
for (const pair of [
  ["riscv64", "e7ce4e66e230661056176a57dc0018b32b799f2ce9d8946d9625b7dfb8ada4af"],
  ["x86", "ada2844dad1251da033b13ebc689342371e79b92eb6b19fddbddd36b6d2a9810"],
  ["arm64", "61d8b5d399c068dd14db258a42889c274ebe69a7fd621f5c0fce8ce25cdff41b"],
]) {
  required(bundle, `zero_arch=${pair[0]}`, `Zero ${pair[0]} archive mapping`);
  required(bundle, `zero_sha256=${pair[1]}`, `Zero ${pair[0]} archive checksum`);
}
required(bundle, 'test -x "$rootfs/usr/local/bin/zero"', "Zero command validation");
required(bundle, 'test -x "$rootfs/usr/local/bin/zero-seccomp"', "Zero seccomp validation");
required(bundle, 'test -x "$rootfs/usr/local/bin/zero-linux-sandbox"', "Zero sandbox validation");
required(bundle, 'test -f "$rootfs/usr/local/lib/zero/bin/zero.js"', "Zero runtime validation");
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
required(bundle, ': >"$rootfs/etc/wanix-container"', "container guest marker");

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
required(releaseWorkflow, 'needs: [build-library, publish-rv64-archive]', "library publish dependency");
required(releaseWorkflow, '"rv64.js-${release_tag#v}.tar.gz"', "library release asset");
assert.doesNotMatch(releaseWorkflow, /wanix-(guest|rv64)-[^\n]*tag/i, "legacy WANIX release tag family");

console.log("WANIX release contract: PASS");
