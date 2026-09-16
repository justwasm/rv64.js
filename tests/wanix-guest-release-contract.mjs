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

const build = read("tools/build-wanix-release-assets.sh");
for (const profile of ["minimal", "container", "container-full"]) {
  required(build, `    ${profile})`, "guest build profile");
}
required(build, "ALPINE_TAG=3.24", "guest Alpine version");
required(
  build,
  'archive="$output_dir/wanix-linux-${archive_arch}${profile_suffix}.tgz"',
  "archive verification",
);
required(build, "tar -tf \"$archive\" --wildcards \"boot/$kernel_name\" >/dev/null", "archive verification");
required(build, "tar -tf \"$archive\" --wildcards \"usr/bin/docker-proxy\" >/dev/null", "Docker archive verification");
required(build, "tar -xOf \"$archive\" etc/docker/daemon.json", "Docker daemon archive verification");
assert.doesNotMatch(build, /tar -tzf[^\n]*\|\s*grep/, "archive verification must not use a SIGPIPE-prone pipeline");

const bundle = read("integrations/wanix/build-linux-bundle.sh");
required(bundle, 'test -x "$rootfs/usr/bin/docker-proxy"', "Docker proxy binary validation");
required(
  bundle,
  "{\"userland-proxy-path\":\"/usr/bin/docker-proxy\"}",
  "Docker daemon configuration",
);

const makefile = read("integrations/wanix/Makefile");
for (const archive of [
  "wanix-linux-rv64.tgz",
  "wanix-linux-rv64-container.tgz",
  "wanix-linux-rv64-container-full.tgz",
  "wanix-linux-x86.tgz",
  "wanix-linux-x86-container.tgz",
  "wanix-linux-x86-container-full.tgz",
  "wanix-linux-arm64.tgz",
  "wanix-linux-arm64-container.tgz",
  "wanix-linux-arm64-container-full.tgz",
]) {
  required(makefile, `$(DIST)/${archive}:`, "guest archive target");
}

const workflow = read(".github/workflows/wanix-guests.yml");
required(workflow, "arch: [riscv64, x86, arm64]", "guest workflow architecture matrix");
required(workflow, "profile: [minimal, container, container-full]", "guest workflow profile matrix");
required(workflow, 'test "${#assets[@]}" -eq 9', "guest workflow archive count");
required(workflow, 'gh release upload "$RELEASE_TAG"', "guest workflow release upload");

const archiveWorkflow = read(".github/workflows/rv64-archive.yml");
required(
  archiveWorkflow,
  'https://github.com/justwasm/rv64.js/releases/download/${UPSTREAM}/rv64.js',
  "rv64 archive loader source",
);
assert.doesNotMatch(archiveWorkflow, /github\.com\/btwiuse\/rv64\.js\/releases/, "rv64 archive loader source");

console.log("WANIX guest release contract: PASS");
