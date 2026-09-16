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
for (const profile of ["minimal", "crush", "python", "nodejs", "golang", "container", "container-full"]) {
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
assert.doesNotMatch(build, /tar -tzf[^\n]*\|\s*grep/, "archive verification must not use a SIGPIPE-prone pipeline");

const bundle = read("integrations/wanix/build-linux-bundle.sh");
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
required(bundle, "profile_packages=(python3 uv)", "Python profile package set");
required(bundle, "profile_packages=(nodejs-current npm)", "Node.js profile package set");
required(bundle, "profile_packages=(go)", "Go profile package set");
required(bundle, "profile_packages=(attr ca-certificates podman python3 strace tmux uv)", "full OCI, Python, multitasking, and debugging package set");
for (const binary of ["getfattr", "podman", "python3", "strace", "tmux", "uv", "node", "npm", "go"]) {
  required(bundle, `test -x "$rootfs/usr/bin/${binary}"`, `${binary} rootfs validation`);
}
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
  "wanix-linux-rv64-golang.tgz",
  "wanix-linux-rv64-container.tgz",
  "wanix-linux-rv64-container-full.tgz",
  "wanix-linux-x86.tgz",
  "wanix-linux-x86-crush.tgz",
  "wanix-linux-x86-python.tgz",
  "wanix-linux-x86-nodejs.tgz",
  "wanix-linux-x86-golang.tgz",
  "wanix-linux-x86-container.tgz",
  "wanix-linux-x86-container-full.tgz",
  "wanix-linux-arm64.tgz",
  "wanix-linux-arm64-crush.tgz",
  "wanix-linux-arm64-python.tgz",
  "wanix-linux-arm64-nodejs.tgz",
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
required(releaseWorkflow, "profile: [minimal, crush, python, nodejs, golang, container, container-full]", "guest workflow profile matrix");
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
