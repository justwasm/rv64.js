#!/usr/bin/env bash
set -euo pipefail

arch="${1:?usage: build-wanix-overlay.sh <riscv64|x86|arm64> <output.tgz>}"
out="${2:?usage: build-wanix-overlay.sh <riscv64|x86|arm64> <output.tgz>}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
wanix_ref="${WANIX_REF:-6594fe3763eb8712e81914f78b79243bb403f5cc}"
tmp="$(mktemp -d "/tmp/wanix-overlay-$arch.XXXXXX")"
wanix_src="$tmp/wanix"
overlay="$tmp/overlay"
trap 'rm -rf "$tmp" >/dev/null 2>&1 || true' EXIT

case "$arch" in
    riscv64)
        go_arch=riscv64
        busybox_arch=rv64
        ;;
    x86)
        go_arch=386
        busybox_arch=i386
        ;;
    arm64)
        go_arch=arm64
        busybox_arch=arm64
        ;;
    *)
        echo "unsupported architecture: $arch" >&2
        exit 2
        ;;
esac

mkdir -p "$overlay/bin" "$overlay/etc"
busybox_bin="$here/bin/busybox-$busybox_arch"
if [ -f "$busybox_bin" ]; then
    install -m 0755 "$busybox_bin" "$overlay/bin/busybox"
    ln -sf /bin/busybox "$overlay/bin/sh"
fi

git clone --quiet https://github.com/tractordev/wanix.git "$wanix_src"
git -C "$wanix_src" checkout --quiet "$wanix_ref"
if [ "$arch" = riscv64 ]; then
    git -C "$wanix_src" apply "$here/wanix-riscv64.patch"
fi
git -C "$wanix_src" apply "$here/wanix-wexec-js.patch"
git -C "$wanix_src" apply "$here/wanix-wexec-poll.patch"
git -C "$wanix_src" apply "$here/wanix-wexec-signal.patch"
git -C "$wanix_src" apply "$here/wanix-wexec-live-read.patch"
cp "$here/guest/init" "$overlay/bin/init"
cp "$wanix_src/extras/linux/bin/domctl" "$wanix_src/extras/linux/bin/post-dhcp" \
    "$wanix_src/extras/linux/bin/startnet" "$wanix_src/extras/linux/bin/workerctl" "$overlay/bin/"
cp "$wanix_src/extras/linux/etc/"* "$overlay/etc/"
GOWORK=off GOOS=linux GOARCH="$go_arch" go build -C "$wanix_src" -trimpath -ldflags="-s -w" -o "$overlay/bin/wexec" ./extras/wexec
GOWORK=off GOOS=linux GOARCH="$go_arch" go build -C "$wanix_src" -trimpath -ldflags="-s -w" -o "$overlay/bin/hostexport" ./extras/hostexport

find "$overlay" -name '._*' -type f -delete
mkdir -p "$(dirname "$out")"
ROOT="$overlay" OUTPUT="$out" python3 - <<'PY'
import os
import tarfile
src = os.environ["ROOT"]
out = os.environ["OUTPUT"]
with tarfile.open(out, "w:gz") as archive:
    for current, directories, files in os.walk(src):
        directories.sort()
        files.sort()
        for name in directories + files:
            path = os.path.join(current, name)
            archive.add(path, os.path.relpath(path, src), recursive=False)
PY

test -s "$out"
echo "$arch Linux overlay: $out"
