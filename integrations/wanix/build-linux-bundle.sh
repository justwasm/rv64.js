#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
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
        default_out="$here/dist/wanix-linux-rv64.tgz"
        kernel="${WANIX_KERNEL:-${RV64_KERNEL:-}}"
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
        default_out="$here/dist/wanix-linux-x86.tgz"
        kernel="${WANIX_KERNEL:-${V86_KERNEL:-}}"
        ;;
    # archlinux32 i686 path: same v86-kernel, but the alpine docker
    # platform still needs to match the guest's libc/musl pair. Use
    # linux/386 so the alpine skeleton we layer on top can install
    # riscv64+x86-only packages if anything ever wants that.
    i686)
        docker_platform=linux/386
        apk_arch=x86
        crush_arch=i386
        peri_arch=i686
        zero_arch=x86
        go_arch=386
        kernel_attr=v86-kernel
        kernel_name=bzImage
        default_out="$here/dist/wanix-linux-i686.tgz"
        kernel="${WANIX_KERNEL:-${V86_KERNEL:-}}"
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
        default_out="$here/dist/wanix-linux-arm64.tgz"
        kernel="${WANIX_KERNEL:-${ARM64_KERNEL:-}}"
        ;;
    *)
        echo "unsupported WANIX_GUEST_ARCH: $guest_arch (expected riscv64, x86, i686, or arm64)" >&2
        exit 2
        ;;
esac

if [ "$kernel_profile" = container ]; then
    kernel_attr+="-container"
fi

out="${1:-$default_out}"
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
wanix_ref="${WANIX_REF:-6594fe3763eb8712e81914f78b79243bb403f5cc}"
trap '$docker_cmd rm -f "$container" >/dev/null 2>&1 || true; chmod -R u+rwX "$tmp" >/dev/null 2>&1 || true; rm -rf "$tmp" >/dev/null 2>&1 || true' EXIT

if [ -z "$kernel" ]; then
    kernel="$(nix build --no-link --print-out-paths "path:$rv64_dir#$kernel_attr" \
        | xargs -I{} find {} -maxdepth 2 -name "$kernel_name" -print | head -1)"
fi
test -n "$kernel"

"$docker_cmd" pull --platform="$docker_platform" "$alpine_image"
"$docker_cmd" create --platform="$docker_platform" --name "$container" "$alpine_image" true >/dev/null
mkdir -p "$rootfs"
"$docker_cmd" export "$container" | tar -C "$rootfs" -xf -

# Arch Linux swap-in: when WANIX_ROOTFS=arch, replace the alpine rootfs
# with the matching upstream Arch bootstrap produced by the Nix recipe
# `arch-recipe` (see flake.nix). The recipe ships per-arch subdirs under
# $rootfs_nix/<arch>/; we copy the requested arch on top of the alpine
# skeleton. WANIX_ROOTFS_KEEP_ALPINE=1 keeps the alpine export as a base
# layer (handy for adding apk fallback tools before pacstrap runs).
rootfs_nix="${WANIX_ROOTFS_NIX:-$(nix build --no-link --print-out-paths "path:$rv64_dir#arch-recipe" 2>/dev/null || true)}"
if [ -n "${WANIX_ROOTFS:-}" ] && [ "$WANIX_ROOTFS" = arch ] && [ -n "$rootfs_nix" ]; then
    arch_subdir="$rootfs_nix/$guest_arch"
    if [ ! -d "$arch_subdir" ]; then
        echo "arch bootstrap for guest arch '$guest_arch' missing under $rootfs_nix" >&2
        exit 2
    fi
    # Preserve the alpine skeleton (usr/local/bin, etc.) so later apk
    # fallbacks still work; overwrite the system tree with the Arch
    # bootstrap contents. Bind-mounted paths keep working because the
    # overlay is created lazily.
    cp -a "$arch_subdir"/. "$rootfs"/
    # Drop in our pacman configs and mirrorlist so the guest's pacman is
    # wired to the curated mirrors at first-boot. Wipe any preexisting
    # mirrorlist that came with the upstream bootstrap.
    rm -f "$rootfs/etc/pacman.d/mirrorlist"
    cp "$here/arch-configs/pacman.conf" "$rootfs/etc/pacman.conf"
    if [ "$guest_arch" = riscv64 ]; then
        cp "$here/arch-configs/mirrorlist.riscv64" "$rootfs/etc/pacman.d/mirrorlist"
    elif [ "$guest_arch" = i686 ] || [ "$guest_arch" = x86 ]; then
        cp "$here/arch-configs/mirrorlist.i686" "$rootfs/etc/pacman.d/mirrorlist"
    else
        cp "$here/arch-configs/mirrorlist" "$rootfs/etc/pacman.d/mirrorlist"
    fi
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
    mkdir -p "$crush_stage" "$rootfs/usr/local/bin"
    curl -fsSL "https://github.com/justwasm/crush/releases/download/$crush_version/crush_${crush_version}_Linux_${crush_arch}.tar.gz" -o "$crush_archive"
    printf '%s  %s\n' "$crush_sha256" "$crush_archive" | sha256sum -c -
    tar -xzf "$crush_archive" --strip-components=1 -C "$crush_stage"
    install -m 0755 "$crush_stage/crush" "$rootfs/usr/local/bin/crush"
fi

if [ "$profile" = peri ]; then
    peri_version=agent-v3.16.5
    case "$peri_arch" in
        riscv64)
            peri_sha256=e1c15813c2f7a73e7b980f839eb0224c490887f51cb601d75b6245b62d0f891c
            ;;
        i686)
            peri_sha256=4caa76cd61cf959c9814233dd5c333af1194a31c5cb4d0697b748ac256c7e406
            ;;
        aarch64)
            peri_sha256=93bebb64cd6624095d4f1456647ee01e029849f35ecf90f6a32ef1a6158678b6
            ;;
    esac
    peri_archive="$tmp/peri-linux-$peri_arch.tar.gz"
    mkdir -p "$rootfs/usr/local/bin"
    curl -fsSL "https://github.com/justwasm/peri/releases/download/$peri_version/peri-linux-$peri_arch.tar.gz" -o "$peri_archive"
    printf '%s  %s\n' "$peri_sha256" "$peri_archive" | sha256sum -c -
    tar -xzf "$peri_archive" -O "peri-linux-$peri_arch" >"$rootfs/usr/local/bin/peri"
    chmod 0755 "$rootfs/usr/local/bin/peri"
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
    mkdir -p "$rootfs/usr/local/bin" "$rootfs/usr/local/lib/zero/bin" "$rootfs/usr/local/share/zero"
    curl -fsSL "https://github.com/justwasm/zero/releases/download/$zero_version/zero-$zero_version-linux-$zero_arch.tar.gz" -o "$zero_archive"
    printf '%s  %s\n' "$zero_sha256" "$zero_archive" | sha256sum -c -
    tar -xzf "$zero_archive" -C "$rootfs/usr/local/bin/" zero zero-seccomp zero-linux-sandbox
    tar -xzf "$zero_archive" -C "$rootfs/usr/local/lib/zero/bin/" --strip-components=1 bin/zero.js
    tar -xzf "$zero_archive" -C "$rootfs/usr/local/share/zero/" package.json README.md VERSION
    chmod 0755 "$rootfs/usr/local/bin/zero" "$rootfs/usr/local/bin/zero-seccomp" "$rootfs/usr/local/bin/zero-linux-sandbox"
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
        test -x "$rootfs/usr/local/bin/crush"
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
        test -x "$rootfs/usr/local/bin/peri"
        ;;
    zero)
        test -x "$rootfs/usr/local/bin/zero"
        test -x "$rootfs/usr/local/bin/zero-seccomp"
        test -x "$rootfs/usr/local/bin/zero-linux-sandbox"
        test -f "$rootfs/usr/local/lib/zero/bin/zero.js"
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
git clone --quiet https://github.com/tractordev/wanix.git "$wanix_src"
git -C "$wanix_src" checkout --quiet "$wanix_ref"
if [ "$guest_arch" = riscv64 ]; then
    git -C "$wanix_src" apply "$here/wanix-riscv64.patch"
fi
git -C "$wanix_src" apply "$here/wanix-wexec-js.patch"
git -C "$wanix_src" apply "$here/wanix-wexec-poll.patch"
git -C "$wanix_src" apply "$here/wanix-wexec-signal.patch"
git -C "$wanix_src" apply "$here/wanix-wexec-live-read.patch"

mkdir -p "$rootfs/boot" "$rootfs/bin" "$rootfs/etc" "$(dirname "$out")"
if [ "$kernel_profile" = container ]; then
    : >"$rootfs/etc/wanix-container"
fi
cp "$kernel" "$rootfs/boot/$kernel_name"
cp "$here/guest/init" "$rootfs/bin/init"
cp "$wanix_src/extras/linux/bin/domctl" "$wanix_src/extras/linux/bin/post-dhcp" \
    "$wanix_src/extras/linux/bin/startnet" "$wanix_src/extras/linux/bin/workerctl" "$rootfs/bin/"
cp "$wanix_src/extras/linux/etc/"* "$rootfs/etc/"
GOWORK=off GOOS=linux GOARCH="$go_arch" go build -C "$wanix_src" -o "$rootfs/bin/wexec" ./extras/wexec
GOWORK=off GOOS=linux GOARCH="$go_arch" go build -C "$wanix_src" -o "$rootfs/bin/hostexport" ./extras/hostexport
find "$rootfs" -name '._*' -type f -delete
ROOTFS="$rootfs" OUTPUT="$out" python3 - <<'PY'
import os
import tarfile

root = os.environ["ROOTFS"]
output = os.environ["OUTPUT"]
with tarfile.open(output, "w:gz") as archive:
    for current, directories, files in os.walk(root):
        directories.sort()
        files.sort()
        for name in directories + files:
            path = os.path.join(current, name)
            archive.add(path, os.path.relpath(path, root), recursive=False)
PY
echo "$guest_arch Linux namespace: $out"
