#!/usr/bin/env bash
#
# fetch-busybox.sh: download Debian's busybox-static and stage a static
# busybox binary per arch into integrations/wanix/bin/busybox-<arch>.
#
# Used only on the Arch rootfs path — Alpine already ships its own
# musl-linked busybox inside the rootfs tarball, so we don't need to
# overlay another copy there. The Debian busybox-static is a true static
# ELF (glibc statically linked), so it runs unchanged against any glibc
# rootfs (Arch).
#
# Outputs (per arch):
#   integrations/wanix/bin/busybox-riscv64
#   integrations/wanix/bin/busybox-x86_64
#   integrations/wanix/bin/busybox-i686
#   integrations/wanix/bin/busybox-aarch64
#
# Args:
#   <arch>...   only stage binaries for the requested wanix arch names
#               (defaults to all four).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
wanix="$here/../integrations/wanix"
out_dir="$wanix/bin"
mkdir -p "$out_dir"

# Map wanix arch names to Debian deb arch suffixes.
declare -A ARCH_DEB=(
  [rv64]="riscv64"
  [x86_64]="amd64"
  [i386]="i386"
  [arm64]="arm64"
)

# Version pinned to current Debian trixie release.
default_version="1.37.0-6+b9"
mirror="${BUSYBOX_DEB_MIRROR:-https://deb.debian.org/debian/pool/main/b/busybox}"

requested=("$@")
if [ "${#requested[@]}" -eq 0 ]; then
  requested=("${!ARCH_DEB[@]}")
fi

work="$(mktemp -d /tmp/busybox-static.XXXXXX)"
trap 'rm -rf "$work"' EXIT

for wanix_arch in "${requested[@]}"; do
  deb_arch="${ARCH_DEB[$wanix_arch]:-}"
  if [ -z "$deb_arch" ]; then
    echo "fetch-busybox: no deb mapping for arch '$wanix_arch'" >&2
    exit 2
  fi
  deb="${BUSYBOX_DEB_URL:-$mirror/busybox-static_${default_version}_${deb_arch}.deb}"
  if ! curl -fsSL --retry 3 -o "$work/busybox.deb" "$deb"; then
    echo "fetch-busybox: failed to download $deb" >&2
    exit 1
  fi
  rm -rf "$work/extracted"
  mkdir -p "$work/extracted"
  ( cd "$work/extracted" && ar x "$work/busybox.deb" )
  if [ ! -f "$work/extracted/data.tar.xz" ] && [ ! -f "$work/extracted/data.tar.zst" ]; then
    echo "fetch-busybox: no data tarball in $deb" >&2
    exit 1
  fi
  data_tar=$(ls "$work/extracted"/data.tar.* | head -1)
  mkdir -p "$work/payload"
  tar -xf "$data_tar" -C "$work/payload"
  src="$work/payload/usr/bin/busybox"
  if [ ! -f "$src" ]; then
    echo "fetch-busybox: /usr/bin/busybox missing in $deb" >&2
    exit 1
  fi
  cp "$src" "$out_dir/busybox-$wanix_arch"
  chmod 0755 "$out_dir/busybox-$wanix_arch"
  file "$out_dir/busybox-$wanix_arch"
  sha256sum "$out_dir/busybox-$wanix_arch"
done
