#!/usr/bin/env bash
#
# build-linux-bundle.sh: assemble a wanix guest as TWO tarballs.
#
#   $out_rootfs  (required)  base rootfs (Alpine or Arch). Safe to
#                             pair with an external wanix overlay.
#   $out_overlay (optional)  wanix overlay tarball: busybox (Arch only),
#                             kernel image, /bin/init, startnet/post-dhcp
#                             /domctl/workerctl, wexec/hostexport,
#                             /etc overlay, and profile binaries
#                             (crush / peri / zero / pi / claude).
#
# Both are bound on top of each other by the host's vm.create path
# (rootfs archive as dst=".", overlay archive as dst="." in union
# order overlay > rootfs), so the overlay can ship its own /bin/init
# even when the rootfs is Arch (which doesn't have it).
#
# Environment variables:
#   WANIX_GUEST_ARCH      rv64 | x86 | i686 | arm64
#   WANIX_KERNEL_PROFILE  minimal | container
#   WANIX_ROOTFS          alpine (default) | arch
#   WANIX_KERNEL          path to a prebuilt kernel image (optional;
#                         otherwise nix builds .#<kernel_attr>)
#   WANIX_ROOTFS_PROFILE  crush | python | nodejs | claude | peri |
#                         zero | pi | golang | full | minimal (default)
#   ALPINE_TAG            Alpine tag for the base image (default 3.24)
#   WANIX_REF             wanix repo ref to checkout (default
#                         6594fe3763eb8712e81914f78b79243bb403f5cc)
#   INSTALL_PYTHON        "1" to additionally install python3 into
#                         the rootfs (legacy opt-in)
#   DOCKER_CMD            docker or podman (default docker)

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
rv64_dir="${RV64_DIR:-$(cd "$here/../.." && pwd)}"
guest_arch="${WANIX_GUEST_ARCH:-riscv64}"
kernel_profile="${WANIX_KERNEL_PROFILE:-minimal}"

case "$kernel_profile" in
    minimal|container)
        ;;
    *)
        echo "unsupported WANIX_KERNEL_PROFILE: $kernel_profile (expected minimal or container)" >&2
        exit 2
        ;;
esac

case "$guest_arch" in
    riscv64)
        docker_platform=linux/riscv64
        apk_arch=riscv64
        crush_arch=riscv64
        peri_arch=riscv64
        zero_arch=riscv64
        go_arch=riscv64
        kernel_attr=virt-kernel-fast
        kernel_name=Image
        ;;
    x86)
        docker_platform=linux/386
        apk_arch=x86
        crush_arch=i386
        peri_arch=i686
        zero_arch=x86
        go_arch=386
        kernel_attr=v86-kernel
        kernel_name=bzImage
        ;;
    i686)
        docker_platform=linux/386
        apk_arch=x86
        crush_arch=i386
        peri_arch=i686
        zero_arch=x86
        go_arch=386
        kernel_attr=v86-kernel
        kernel_name=bzImage
        ;;
    arm64)
        docker_platform=linux/arm64
        apk_arch=aarch64
        crush_arch=arm64
        peri_arch=aarch64
        zero_arch=arm64
        go_arch=arm64
        kernel_attr=arm64-kernel
        kernel_name=Image
        ;;
    *)
        echo "unsupported WANIX_GUEST_ARCH: $guest_arch (expected riscv64, x86, i686, or arm64)" >&2
        exit 2
        ;;
esac

if [ "$kernel_profile" = container ]; then
    kernel_attr+="-container"
fi

# Two output tarballs:
#   $1 is the rootfs output path (required); $2 is the overlay output
#   path (optional — defaults to ${1%.tgz}-overlay.tgz).
out_rootfs="${1:?usage: build-linux-bundle.sh <rootfs.tgz> [overlay.tgz]}"
case "$out_rootfs" in
    *.tgz) out_overlay_default="${out_rootfs%.tgz}-overlay.tgz" ;;
    *) out_overlay_default="${out_rootfs}-overlay.tgz" ;;
esac
out_overlay="${2:-$out_overlay_default}"
mkdir -p "$(dirname "$out_rootfs")" "$(dirname "$out_overlay")"

docker_cmd="${DOCKER_CMD:-docker}"
profile="${WANIX_ROOTFS_PROFILE:-minimal}"
install_python="${INSTALL_PYTHON:-0}"
alpine_tag="${ALPINE_TAG:-3.24}"
alpine_image="alpine:${alpine_tag}"
case "$profile" in
    minimal)
        profile_packages=()
        ;;
    crush)
        profile_packages=()
        ;;
    python)
        profile_packages=(python3 uv)
        ;;
    nodejs)
        profile_packages=(nodejs-current npm)
        ;;
    claude)
        profile_packages=(nodejs-current npm ripgrep)
        ;;
    peri)
        profile_packages=()
        ;;
    zero)
        profile_packages=()
        ;;
    pi)
        profile_packages=(nodejs-current npm)
        ;;
    golang)
        profile_packages=(go)
        ;;
    full)
        profile_packages=(attr ca-certificates podman python3 strace tmux uv)
        ;;
    *)
        echo "unsupported WANIX_ROOTFS_PROFILE: $profile (expected minimal, crush, python, nodejs, claude, peri, zero, pi, golang, or full)" >&2
        exit 2
        ;;
esac
tmp="$(mktemp -d "/tmp/wanix-$guest_arch-root.XXXXXX")"
container="wanix-$guest_arch-root-$$"
wanix_src="$tmp/wanix"
rootfs="$tmp/rootfs"
overlay="$tmp/overlay"
wanix_ref="${WANIX_REF:-6594fe3763eb8712e81914f78b79243bb403f5cc}"
trap '$docker_cmd rm -f "$container" >/dev/null 2>&1 || true; chmod -R u+rwX "$tmp" >/dev/null 2>&1 || true; rm -rf "$tmp" >/dev/null 2>&1 || true' EXIT

if [ -z "${kernel:-}" ]; then
    kernel="$(nix build --no-link --print-out-paths "path:$rv64_dir#$kernel_attr" \
        | xargs -I{} find {} -maxdepth 2 -name "$kernel_name" -print | head -1)"
fi
test -n "$kernel"

"$docker_cmd" pull --platform="$docker_platform" "$alpine_image" >/dev/null
"$docker_cmd" create --platform="$docker_platform" --name "$container" "$alpine_image" true >/dev/null
mkdir -p "$rootfs"
"$docker_cmd" export "$container" | tar -C "$rootfs" -xf -

# Arch Linux swap-in: when WANIX_ROOTFS=arch, replace the alpine rootfs
# with the matching upstream Arch base tarball. The Arch tarball path is
# passed via WANIX_ROOTFS_TARBALL; defaults to fetching the latest
# release's archlinux-base-<arch>.tar.gz from btwiuse/archlinux.
arch_subdir=""
if [ "${WANIX_ROOTFS:-}" = arch ]; then
    if [ -n "${WANIX_ROOTFS_TARBALL:-}" ]; then
        arch_tarball="$WANIX_ROOTFS_TARBALL"
    else
        arch_arch="$guest_arch"
        case "$arch_arch" in
            riscv64) arch_arch=riscv64 ;;
            arm64) arch_arch=aarch64 ;;
            x86|i686) arch_arch=i686 ;;
        esac
        arch_tarball="$(dirname "$here")/../../wanix-dist/archlinux-base-${arch_arch}.tar.gz"
        if [ ! -f "$arch_tarball" ]; then
            arch_tarball_url="https://github.com/btwiuse/archlinux/releases/latest/download/archlinux-base-${arch_arch}.tar.gz"
            mkdir -p "$(dirname "$arch_tarball")"
            curl -fsSL --retry 3 "$arch_tarball_url" -o "$arch_tarball"
        fi
    fi
    # Replace the alpine skeleton with the Arch base tarball.
    rm -rf "$rootfs"
    mkdir -p "$rootfs"
    tar -xzf "$arch_tarball" -C "$rootfs"
    # Drop in our pacman configs and mirrorlist so the guest's pacman is
    # wired to the curated mirrors at first-boot. Wipe any preexisting
    # mirrorlist that came with the upstream tarball.
    rm -f "$rootfs/etc/pacman.d/mirrorlist"
    cp "$here/arch-configs/pacman.conf" "$rootfs/etc/pacman.conf"
    case "$guest_arch" in
        riscv64) cp "$here/arch-configs/mirrorlist.riscv64" "$rootfs/etc/pacman.d/mirrorlist" ;;
        i686|x86) cp "$here/arch-configs/mirrorlist.i686" "$rootfs/etc/pacman.d/mirrorlist" ;;
        *) cp "$here/arch-configs/mirrorlist" "$rootfs/etc/pacman.d/mirrorlist" ;;
    esac
fi

if [ "$profile" = crush ]; then
    crush_version=v0.94.0
    case "$crush_arch" in
        riscv64)
            crush_sha256=b2798cd2d44312714bb389d3cd3de12fbbd80c74f4b912b635c1855fc2e81676
            ;;
        i386)
            crush_sha256=2f36756048d3f5ee5f13bb2512492c487b573781e18d4a5afe34a244fc29377c
            ;;
        arm64)
            crush_sha256=ed2bf9bfa3e248ce917478f247d634ea942597299c2f05233ddbd356a932276a
            ;;
    esac
    crush_archive="$tmp/crush.tar.gz"
    crush_stage="$tmp/crush"
    mkdir -p "$crush_stage" "$overlay/usr/local/bin"
    curl -fsSL "https://github.com/justwasm/crush/releases/download/$crush_version/crush_${crush_version}_Linux_${crush_arch}.tar.gz" -o "$crush_archive"
    printf '%s  %s\n' "$crush_sha256" "$crush_archive" | sha256sum -c -
    tar -xzf "$crush_archive" --strip-components=1 -C "$crush_stage"
    install -m 0755 "$crush_stage/crush" "$overlay/usr/local/bin/crush"
fi

if [ "$profile" = peri ]; then
    peri_version=agent-v3.16.5
    case "$peri_arch" in
        riscv64)
            peri_sha256=acb827a9d1d4f97eb57ee80b60de9701054533558a29854ca1f1c9731c473e36
            ;;
        i686)
            peri_sha256=96978b393068b051069c60d72399b30d53896c6a1f531ac14b2869e92ae7596f
            ;;
        aarch64)
            peri_sha256=0c39d13cd13cb058e9888afd918fe2debb1a71060d3c55b6ec1bffdb2b02c385
            ;;
    esac
    peri_archive="$tmp/peri-linux-$peri_arch.tar.gz"
    mkdir -p "$overlay/usr/local/bin"
    curl -fsSL "https://github.com/justwasm/peri/releases/download/$peri_version/peri-linux-$peri_arch.tar.gz" -o "$peri_archive"
    printf '%s  %s\n' "$peri_sha256" "$peri_archive" | sha256sum -c -
    tar -xzf "$peri_archive" -O "peri-linux-$peri_arch" >"$overlay/usr/local/bin/peri"
    chmod 0755 "$overlay/usr/local/bin/peri"
fi

if [ "$profile" = zero ]; then
    zero_version=v0.9.0
    case "$zero_arch" in
        riscv64)
            zero_sha256=e7ce4e66e230661056176a57dc0018b32b799f2ce9d8946d9625b7dfb8ada4af
            ;;
        x86)
            zero_sha256=ada2844dad1251da033b13ebc689342371e79b92eb6b19fddbddd36b6d2a9810
            ;;
        arm64)
            zero_sha256=61d8b5d399c068dd14db258a42889c274ebe69a7fd621f5c0fce8ce25cdff41b
            ;;
    esac
    zero_archive="$tmp/zero-linux-$zero_arch.tar.gz"
    mkdir -p "$overlay/usr/local/bin" "$overlay/usr/local/lib/zero/bin" "$overlay/usr/local/share/zero"
    curl -fsSL "https://github.com/justwasm/zero/releases/download/$zero_version/zero-$zero_version-linux-$zero_arch.tar.gz" -o "$zero_archive"
    printf '%s  %s\n' "$zero_sha256" "$zero_archive" | sha256sum -c -
    tar -xzf "$zero_archive" -C "$overlay/usr/local/bin/" zero zero-seccomp zero-linux-sandbox
    tar -xzf "$zero_archive" -C "$overlay/usr/local/lib/zero/bin/" --strip-components=1 bin/zero.js
    tar -xzf "$zero_archive" -C "$overlay/usr/local/share/zero/" package.json README.md VERSION
    chmod 0755 "$overlay/usr/local/bin/zero" "$overlay/usr/local/bin/zero-seccomp" "$overlay/usr/local/bin/zero-linux-sandbox"
fi

# Keep the default guest rootfs minimal, matching the existing x86 and RV64
# archives. Python is an opt-in workload dependency for benchmark images.
if [ "$install_python" = 1 ] || [ "${#profile_packages[@]}" -gt 0 ]; then
    packages=("${profile_packages[@]}")
    if [ "$install_python" = 1 ]; then
        packages+=(python3)
    fi
    "$docker_cmd" run --rm --platform=linux/amd64 -v "$rootfs:/target" "$alpine_image" \
        apk --root /target --initdb --arch "$apk_arch" --no-scripts --allow-untrusted \
        --repository "https://dl-cdn.alpinelinux.org/alpine/v${alpine_tag%.*}/main" \
        --repository "https://dl-cdn.alpinelinux.org/alpine/v${alpine_tag%.*}/community" \
        add "${packages[@]}"
fi
if [ "$profile" = claude ]; then
    "$docker_cmd" run --rm --platform=linux/amd64 -v "$rootfs:/target" "$alpine_image" \
        sh -ec 'apk add --no-cache nodejs-current npm ripgrep; npm --prefix /target/usr/local install --global claude-code-best'
fi
if [ "$profile" = pi ]; then
    "$docker_cmd" run --rm --platform=linux/amd64 -v "$rootfs:/target" "$alpine_image" \
        sh -ec 'apk add --no-cache nodejs-current npm; npm --prefix /target/usr/local install --global --ignore-scripts @earendil-works/pi-coding-agent'
fi
case "$profile" in
    crush)
        test -x "$overlay/usr/local/bin/crush"
        ;;
    python)
        test -x "$rootfs/usr/bin/python3"
        test -x "$rootfs/usr/bin/uv"
        ;;
    nodejs)
        test -x "$rootfs/usr/bin/node"
        test -x "$rootfs/usr/bin/npm"
        ;;
    claude)
        test -x "$rootfs/usr/bin/node"
        test -x "$rootfs/usr/bin/npm"
        test -x "$rootfs/usr/bin/rg"
        test -L "$rootfs/usr/local/bin/claude-code-best"
        ;;
    peri)
        test -x "$overlay/usr/local/bin/peri"
        ;;
    zero)
        test -x "$overlay/usr/local/bin/zero"
        test -x "$overlay/usr/local/bin/zero-seccomp"
        test -x "$overlay/usr/local/bin/zero-linux-sandbox"
        test -f "$overlay/usr/local/lib/zero/bin/zero.js"
        ;;
    pi)
        test -x "$rootfs/usr/bin/node"
        test -x "$rootfs/usr/bin/npm"
        test -L "$rootfs/usr/local/bin/pi"
        ;;
    golang)
        test -L "$rootfs/usr/bin/go"
        test -x "$rootfs/usr/lib/go/bin/go"
        ;;
    full)
        test -x "$rootfs/usr/bin/getfattr"
        test -x "$rootfs/usr/bin/podman"
        test -x "$rootfs/usr/bin/python3"
        test -x "$rootfs/usr/bin/strace"
        test -x "$rootfs/usr/bin/tmux"
        test -x "$rootfs/usr/bin/uv"
        ;;
esac
"$docker_cmd" run --rm --platform=linux/amd64 -v "$rootfs:/target" "$alpine_image" \
    find -H /target \( -type f -o -type d \) -exec chown "$(id -u):$(id -g)" {} + || true

# Wanix overlay: kernel + busybox + init + wexec + hostexport + /etc overlay.
# Busybox is only added on Arch rootfs (Alpine already ships its own in
# /bin/busybox, and it's the same musl build).
mkdir -p "$overlay/boot" "$overlay/bin" "$overlay/etc"
if [ "$kernel_profile" = container ]; then
    : >"$overlay/etc/wanix-container"
fi
if [ "${WANIX_ROOTFS:-}" = arch ]; then
    busybox_bin="$here/bin/busybox-$guest_arch"
    if [ ! -f "$busybox_bin" ]; then
        echo "missing busybox for $guest_arch at $busybox_bin; run tools/fetch-busybox.sh" >&2
        exit 1
    fi
    install -m 0755 "$busybox_bin" "$overlay/bin/busybox"
    ln -sf /bin/busybox "$overlay/bin/sh"
fi
git clone --quiet https://github.com/tractordev/wanix.git "$wanix_src"
git -C "$wanix_src" checkout --quiet "$wanix_ref"
if [ "$guest_arch" = riscv64 ]; then
    git -C "$wanix_src" apply "$here/wanix-riscv64.patch"
fi
git -C "$wanix_src" apply "$here/wanix-wexec-js.patch"
git -C "$wanix_src" apply "$here/wanix-wexec-poll.patch"
git -C "$wanix_src" apply "$here/wanix-wexec-signal.patch"
git -C "$wanix_src" apply "$here/wanix-wexec-live-read.patch"
cp "$kernel" "$overlay/boot/$kernel_name"
cp "$here/guest/init" "$overlay/bin/init"
cp "$wanix_src/extras/linux/bin/domctl" "$wanix_src/extras/linux/bin/post-dhcp" \
    "$wanix_src/extras/linux/bin/startnet" "$wanix_src/extras/linux/bin/workerctl" "$overlay/bin/"
cp "$wanix_src/extras/linux/etc/"* "$overlay/etc/"
GOWORK=off GOOS=linux GOARCH="$go_arch" go build -C "$wanix_src" -o "$overlay/bin/wexec" ./extras/wexec
GOWORK=off GOOS=linux GOARCH="$go_arch" go build -C "$wanix_src" -o "$overlay/bin/hostexport" ./extras/hostexport

find "$rootfs" -name '._*' -type f -delete
find "$overlay" -name '._*' -type f -delete

# Emit both tarballs. Both directories are walked in sorted order so
# the produced archive is deterministic for a given input set.
emit_tarball() {
    local src="$1" out="$2"
    ROOT="$src" OUTPUT="$out" python3 - <<'PY'
import os, tarfile
src = os.environ["ROOT"]
output = os.environ["OUTPUT"]
with tarfile.open(output, "w:gz") as archive:
    for current, directories, files in os.walk(src):
        directories.sort()
        files.sort()
        for name in directories + files:
            path = os.path.join(current, name)
            archive.add(path, os.path.relpath(path, src), recursive=False)
PY
}

emit_tarball "$rootfs" "$out_rootfs"
emit_tarball "$overlay" "$out_overlay"

echo "$guest_arch Linux namespace: $out_rootfs"
echo "$guest_arch Linux overlay: $out_overlay"
