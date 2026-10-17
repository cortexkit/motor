#!/usr/bin/env bash
# Build and verify a kernel from the immutable inputs exported by build.py.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd -P)
out=${1:?usage: build-kernel.sh OUTPUT_DIRECTORY [--config-only]}
mode=${2:-build}
[[ $mode == build || $mode == --config-only ]]
: "${KERNEL_VERSION:?}" "${KERNEL_SHA256:?}" "${KERNEL_GCC:?}" "${JOBS:?}"
compiler=${CC:-gcc-13}
[[ $("$compiler" -dumpfullversion -dumpversion) == "$KERNEL_GCC" ]] || {
    echo "kernel needs GCC $KERNEL_GCC" >&2; exit 1;
}
mkdir -p "$out"
out=$(cd "$out" && pwd -P)
curl --fail --location --proto '=https' --tlsv1.2 \
    "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KERNEL_VERSION.tar.xz" -o "$out/linux.tar.xz"
echo "$KERNEL_SHA256  $out/linux.tar.xz" | sha256sum --check
tar -xJf "$out/linux.tar.xz" -C "$out"
cd "$out/linux-$KERNEL_VERSION"
make CC="$compiler" HOSTCC="$compiler" x86_64_defconfig
while IFS='=' read -r option value; do
    case "$option" in ''|'#'*) continue ;; esac
    case "$value" in
        y) scripts/config --enable "$option" ;;
        n) scripts/config --disable "$option" ;;
        *) echo "invalid kernel option $option=$value" >&2; exit 1 ;;
    esac
done < "$here/kernel-options.txt"
make CC="$compiler" HOSTCC="$compiler" olddefconfig
python3 "$here/build.py" check-kernel .config
cp .config "$out/kernel.config"
if [[ $mode == build ]]; then
    make CC="$compiler" HOSTCC="$compiler" -j"$JOBS" vmlinux
    cp vmlinux "$out/vmlinux"
fi
