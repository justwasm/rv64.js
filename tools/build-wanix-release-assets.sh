#!/usr/bin/env bash
set -euo pipefail

arch="${1:?usage: build-wanix-release-assets.sh <riscv64|x86|arm64> <minimal|container|container-full> <output-dir>}"
profile="${2:?usage: build-wanix-release-assets.sh <riscv64|x86|arm64> <minimal|container|container-full> <output-dir>}"
output_dir="${3:?usage: build-wanix-release-assets.sh <riscv64|x86|arm64> <minimal|container|container-full> <output-dir>}"

case "$arch" in
    riscv64)
        archive_arch=rv64
        kernel_attr=virt-kernel-fast
        kernel_name=Image
        ;;
    x86)
        archive_arch=x86
        kernel_attr=v86-kernel
        kernel_name=bzImage
        ;;
    arm64)
        archive_arch=arm64
        kernel_attr=arm64-kernel
        kernel_name=Image
        ;;
    *)
        echo "unsupported architecture: $arch" >&2
        exit 2
        ;;
esac

case "$profile" in
    minimal)
        kernel_profile=minimal
        rootfs_profile=minimal
        profile_suffix=""
        ;;
    crush|python|nodejs|claude|golang)
        kernel_profile=minimal
        rootfs_profile="$profile"
        profile_suffix="-$profile"
        ;;
    container)
        kernel_profile=container
        rootfs_profile=minimal
        kernel_attr+="-container"
        profile_suffix="-container"
        ;;
    container-full)
        kernel_profile=container
        rootfs_profile=full
        kernel_attr+="-container"
        profile_suffix="-container-full"
        ;;
    *)
        echo "unsupported profile: $profile (expected minimal, crush, python, nodejs, claude, golang, container, or container-full)" >&2
        exit 2
        ;;
esac

mkdir -p "$output_dir/kernels"
config_path="$(nix build --no-link --print-out-paths ".#$kernel_attr.configfile")"
grep -qx 'CONFIG_IKCONFIG=y' "$config_path"
grep -qx 'CONFIG_IKCONFIG_PROC=y' "$config_path"
kernel_output="$(nix build --no-link --print-out-paths ".#$kernel_attr")"
kernel_path="$kernel_output/$kernel_name"
test -s "$kernel_path"
install -m 0644 "$kernel_path" "$output_dir/kernels/${archive_arch}${profile_suffix}-${kernel_name}"

WANIX_KERNEL="$kernel_path" \
WANIX_GUEST_ARCH="$arch" \
WANIX_KERNEL_PROFILE="$kernel_profile" \
WANIX_ROOTFS_PROFILE="$rootfs_profile" \
ALPINE_TAG=3.24 \
  integrations/wanix/build-linux-bundle.sh \
  "$output_dir/wanix-linux-${archive_arch}${profile_suffix}.tgz"

file "$output_dir/kernels/${archive_arch}${profile_suffix}-${kernel_name}"
archive="$output_dir/wanix-linux-${archive_arch}${profile_suffix}.tgz"
tar -tf "$archive" --wildcards "boot/$kernel_name" >/dev/null
case "$rootfs_profile" in
    crush)
        tar -tf "$archive" --wildcards "usr/local/bin/crush" >/dev/null
        ;;
    python)
        tar -tf "$archive" --wildcards "usr/bin/python3" >/dev/null
        tar -tf "$archive" --wildcards "usr/bin/uv" >/dev/null
        ;;
    nodejs)
        tar -tf "$archive" --wildcards "usr/bin/node" >/dev/null
        tar -tf "$archive" --wildcards "usr/bin/npm" >/dev/null
        ;;
    claude)
        tar -tf "$archive" --wildcards "usr/bin/node" >/dev/null
        tar -tf "$archive" --wildcards "usr/bin/npm" >/dev/null
        tar -tf "$archive" --wildcards "usr/local/bin/claude-code-best" >/dev/null
        ;;
    golang)
        tar -tf "$archive" --wildcards "usr/bin/go" >/dev/null
        ;;
    full)
        tar -tf "$archive" --wildcards "usr/bin/getfattr" >/dev/null
        tar -tf "$archive" --wildcards "usr/bin/podman" >/dev/null
        tar -tf "$archive" --wildcards "usr/bin/python3" >/dev/null
        tar -tf "$archive" --wildcards "usr/bin/strace" >/dev/null
        tar -tf "$archive" --wildcards "usr/bin/tmux" >/dev/null
        tar -tf "$archive" --wildcards "usr/bin/uv" >/dev/null
        ;;
esac
