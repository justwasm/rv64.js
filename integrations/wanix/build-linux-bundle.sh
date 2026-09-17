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
        go_arch=386
        kernel_attr=v86-kernel
        kernel_name=bzImage
        default_out="$here/dist/wanix-linux-x86.tgz"
        kernel="${WANIX_KERNEL:-${V86_KERNEL:-}}"
        ;;
    arm64)
        docker_platform=linux/arm64
        apk_arch=aarch64
        crush_arch=arm64
        go_arch=arm64
        kernel_attr=arm64-kernel
        kernel_name=Image
        default_out="$here/dist/wanix-linux-arm64.tgz"
        kernel="${WANIX_KERNEL:-${ARM64_KERNEL:-}}"
        ;;
    *)
        echo "unsupported WANIX_GUEST_ARCH: $guest_arch (expected riscv64, x86, or arm64)" >&2
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
    golang)
        profile_packages=(go)
        ;;
    full)
        profile_packages=(attr ca-certificates podman python3 strace tmux uv)
        ;;
    *)
        echo "unsupported WANIX_ROOTFS_PROFILE: $profile (expected minimal, crush, python, nodejs, claude, golang, or full)" >&2
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
