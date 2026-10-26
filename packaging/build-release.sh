#!/usr/bin/env bash
# Requires pinned Rust (rust-toolchain.toml), installed musl targets and Zig 0.15.1.
# CARGO_TARGET_DIR and RELEASE_OUT may be outside the checkout. No rustup mutations.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$root"
for program in cargo file readelf sha256sum; do
    command -v "$program" >/dev/null || { echo "Required tool missing: $program" >&2; exit 1; }
done
export ZIG="${ZIG:-zig}"
[[ $("$ZIG" version) == 0.15.1 ]] || { echo 'Zig 0.15.1 is required' >&2; exit 1; }
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
out="${RELEASE_OUT:-$CARGO_TARGET_DIR/dist}"
mkdir -p "$out"
out="$(cd "$out" && pwd -P)"
wrappers="$(mktemp -d)"
trap 'rm -rf -- "$wrappers"' EXIT
for driver in cc cxx ar; do ln -s "$root/packaging/zig-tool.sh" "$wrappers/$driver"; done
package_only=0
if [[ ${1:-} == --package-only ]]; then package_only=1; shift; fi
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)"
targets=("$@")
if [[ ${#targets[@]} == 0 ]]; then targets=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl); fi
for target in "${targets[@]}"; do
    case "$target" in
        x86_64-unknown-linux-musl) export BRAIN_ZIG_TARGET=x86_64-linux-musl ;;
        aarch64-unknown-linux-musl) export BRAIN_ZIG_TARGET=aarch64-linux-musl ;;
        *) echo "Unsupported release target: $target" >&2; exit 2 ;;
    esac
    key="${target//-/_}"
    upper="${key^^}"
    # Override checkout config and ambient compiler flags with one explicit toolchain.
    export "CC_$key=$wrappers/cc" "CXX_$key=$wrappers/cxx" "AR_$key=$wrappers/ar"
    export "CARGO_TARGET_${upper}_LINKER=$wrappers/cc" "CARGO_TARGET_${upper}_AR=$wrappers/ar"
    export "CARGO_TARGET_${upper}_RUSTFLAGS=-C link-self-contained=no -C target-feature=+crt-static -C link-arg=-static --remap-path-prefix=$root=/build/brain-ed --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup"
    unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
    export "CFLAGS_$key=-ffile-prefix-map=$root=/build/brain-ed -ffile-prefix-map=${CARGO_HOME:-$HOME/.cargo}=/cargo"
    export "CXXFLAGS_$key=-ffile-prefix-map=$root=/build/brain-ed -ffile-prefix-map=${CARGO_HOME:-$HOME/.cargo}=/cargo"
    if [[ $package_only == 0 ]]; then
        cargo build --locked --release -p brain-cli --target "$target"
    fi
    artifact="$out/brain-$version-$target"
    cp "$CARGO_TARGET_DIR/$target/release/brain" "$artifact"
    chmod 755 "$artifact"
    readelf -h "$artifact" >/dev/null
    file "$artifact" | tee "$artifact.file"
    # Fail if an interpreter or dynamically loaded library slipped into the ELF.
    if readelf -l "$artifact" | grep -q INTERP || readelf -d "$artifact" | grep -q NEEDED; then
        echo "Non-static release artifact: $artifact" >&2; exit 1
    fi
    wc -c < "$artifact" | tee "$artifact.bytes"
done
# Explicit names prevent accidental inclusion of report files or old versions.
(cd "$out"; sha256sum brain-"$version"-*-unknown-linux-musl > SHA256SUMS)
printf 'Artifacts: %s\n' "$out"
