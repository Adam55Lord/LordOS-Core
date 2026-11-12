#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Copyright (C) 2026 Adam55Lord
# Lordixyn Core (kernel): builds the kernel and writes ./dist/lordixyn-core.lkf, the kernel as an LKF image
# (Lord Kernel Format, lxf_tools v3: the only kernel format Lord Boot loads).
#   ./build.sh                    release build (default)
#   ./build.sh --debug            debug build (opt-level 1, slower but easier to debug)
#   BOOT_BACKEND=<n> ./build.sh   another boot backend: needs the cargo feature `boot-<n>`
#   ARCH=<a> ./build.sh           another architecture: x86_64 (default), aarch64, riscv64.
#                                 Only x86_64 is complete; the others are skeletons (docs/23).
# Needs Rust and python3 (tools/lxf/mkkernel.py packs the linked kernel into LKF).
# To boot it, use the lord-docs-tests package: put dist/lordixyn-core.lkf into its kernel/ folder (as kernel.lkf)
# (or unpack lordixyn-core next to it), together with Lord Boot's BOOTX64.EFI.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

PROFILE=release; CARGO_PROFILE=(--release)
case "${1:-}" in
    "") ;;
    --debug) PROFILE=debug; CARGO_PROFILE=() ;;
    -h|--help) sed -n '4,12p' "$0"; exit 0 ;;
    *) echo "error: unknown option '$1' (try --help)" >&2; exit 1 ;;
esac
BACKEND="${BOOT_BACKEND:-lord-boot}"
ARCH="${ARCH:-x86_64}"
case "$ARCH" in
    x86_64)  TARGET=x86_64-unknown-none ;;
    aarch64) TARGET=aarch64-unknown-none-softfloat ;;
    riscv64) TARGET=riscv64gc-unknown-none-elf ;;
    *) echo "error: unknown ARCH '$ARCH' (x86_64, aarch64, riscv64)" >&2; exit 1 ;;
esac

command -v python3 >/dev/null 2>&1 || { echo "error: python3 not found (needed for tools/lxf/mkkernel.py)" >&2; exit 1; }
# A half-failed `rustup update` can leave the toolchain without its compiler. Say how to repair it.
if command -v rustup >/dev/null 2>&1 && ! rustc --version >/dev/null 2>&1; then
    echo "error: the Rust toolchain is broken (rustc does not run). Repair it with:" >&2
    echo "  rustup toolchain uninstall stable && rm -rf ~/.rustup/tmp ~/.rustup/downloads" >&2
    echo "  rustup toolchain install stable --profile minimal -t x86_64-unknown-none" >&2
    exit 1
fi
command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found. Install Rust: https://rustup.rs" >&2; exit 1; }

# Needs Rust 1.85+ (edition 2024). rustup reads rust-toolchain.toml and also
# installs the bare-metal target automatically on first build.
echo "==> Building Lordixyn Core ($PROFILE, $TARGET, boot backend: $BACKEND)..."
# Only x86_64 is listed in rust-toolchain.toml. Another ARCH needs its own target: add it on demand.
if [ "$TARGET" != x86_64-unknown-none ] && command -v rustup >/dev/null 2>&1; then
    rustup target add "$TARGET" || echo "warning: could not install $TARGET with rustup; trying -Zbuild-std" >&2
fi
SYSROOT="$(rustc --print sysroot)"
if [ -d "$SYSROOT/lib/rustlib/$TARGET" ]; then
    cargo build "${CARGO_PROFILE[@]}" --target "$TARGET" --no-default-features --features "boot-$BACKEND"
else
    # Distro rustc without the bare-metal target: build core/alloc from rust-src (needs rust-src + lld).
    RUSTC_BOOTSTRAP=1 cargo build "${CARGO_PROFILE[@]}" --target "$TARGET" --no-default-features --features "boot-$BACKEND" \
        -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem
fi

# The linker's output stays in target/ (Cargo's own artifact, useful for gdb). Only the LKF image is installed.
LINKED="target/$TARGET/$PROFILE/lordixyn-core"
[ -f "$LINKED" ] || { echo "error: $LINKED missing after build" >&2; exit 1; }
mkdir -p dist
rm -f dist/lordixyn-core dist/lordixyn-core.lxf   # (the pre-v3 name of the image)
echo "==> Packing the kernel as an LKF image..."
# Kernel version = the crate version; SMP is always on; the DEBUG flag follows --debug.
VERSION="$(sed -n 's/^version *= *"\([0-9][0-9.]*\)".*/\1/p' Cargo.toml | head -n1)"
PACK_FLAGS=(--kernel-version "${VERSION:-0.0.0}" --smp)
[ "$PROFILE" = debug ] && PACK_FLAGS+=(--kdebug)
python3 tools/lxf/mkkernel.py "$LINKED" dist/lordixyn-core.lkf "${PACK_FLAGS[@]}"
echo "==> Done: $PWD/dist/lordixyn-core.lkf"
echo "    Next: copy it to lord-docs-tests/kernel/ as kernel.lkf, then run ./assemble.sh and ./run.sh there."
