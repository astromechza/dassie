#!/usr/bin/env bash
# Builds the guest kernel for one architecture from pinned sources (ADR 0006).
#
#   guest/kernel/build.sh [x86_64|aarch64]
#
# Starts from Firecracker's recommended guest config, merges the fragments in
# this directory, and checks that every fragment option survived
# `olddefconfig`. Outputs $OUT/vmlinux (x86_64) or $OUT/Image (aarch64).
#
# Needs: make, gcc, flex, bison, bc, libelf-dev, curl, sha256sum. Builds for
# the host arch; set CROSS_COMPILE (e.g. aarch64-linux-gnu-) to cross-build.

# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"
: "${ARCH:?}"

KERNEL_VERSION=6.18.60
KERNEL_SHA256=3c2f44ef6acc021a7ad444d9a2e6ec1ef488ff09368793d0004a6080f69d621e
# Firecracker v1.17.0, which the CI job runs.
FC_REV=95f868c8e345b1cc8faccd1a3c910b4989dc3f58
FC_CONFIG_URL="https://raw.githubusercontent.com/firecracker-microvm/firecracker/$FC_REV/resources/guest_configs/microvm-kernel-ci-$ARCH-6.18.config"

case "$ARCH" in
  x86_64)
    KARCH=x86_64 TARGET=vmlinux IMAGE=vmlinux
    FC_CONFIG_SHA256=ba22401a0c7292a4c024ebcd10a562d4a1f1bfd2faed671406d3b159c0cf5215
    ;;
  aarch64)
    KARCH=arm64 TARGET=Image IMAGE=arch/arm64/boot/Image
    FC_CONFIG_SHA256=35cce8e8b754a84523ca20dc068fc3f8f03a15523be36a8623bad54e11972e2a
    ;;
esac

here=$(cd "$(dirname "$0")" && pwd)
fragments=("$here/dassie.config")
[[ -f "$here/dassie-$ARCH.config" ]] && fragments+=("$here/dassie-$ARCH.config")

tarball=$(fetch "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KERNEL_VERSION.tar.xz" "$KERNEL_SHA256")
base_config=$(fetch "$FC_CONFIG_URL" "$FC_CONFIG_SHA256")

src="$OUT/build/linux-$KERNEL_VERSION"
if [[ ! -f "$src/Makefile" ]]; then
  log "unpacking linux-$KERNEL_VERSION"
  mkdir -p "$OUT/build"
  tar -xJf "$tarball" -C "$OUT/build"
fi

kmake() { make -C "$src" ARCH="$KARCH" CROSS_COMPILE="${CROSS_COMPILE:-}" "$@"; }

log "configuring from $(basename "$FC_CONFIG_URL") + ${fragments[*]##*/}"
cp "$base_config" "$src/.config"
(cd "$src" && ARCH="$KARCH" scripts/kconfig/merge_config.sh -m .config "${fragments[@]}" >/dev/null)
kmake olddefconfig >/dev/null

# Fail if a dependency silently dropped a required option.
missing=0
for frag in "${fragments[@]}"; do
  while IFS= read -r line; do
    [[ "$line" =~ ^CONFIG_ ]] || continue
    if ! grep -qxF "$line" "$src/.config"; then
      echo "kernel config: $line did not survive olddefconfig" >&2
      missing=1
    fi
  done <"$frag"
done
((missing == 0)) || exit 1

log "building $TARGET"
kmake -j"$(nproc)" "$TARGET"
cp "$src/$IMAGE" "$OUT/$TARGET"
cp "$src/.config" "$OUT/kernel.config"
log "wrote $OUT/$TARGET"
