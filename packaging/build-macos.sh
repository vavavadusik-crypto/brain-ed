#!/usr/bin/env bash
# Native Darwin release; no codesigning/notarization or publication.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$root"
[[ $(uname -s) == Darwin ]] || { echo 'Native macOS runner required' >&2; exit 2; }
case "$(uname -m)" in
    arm64|aarch64) native=aarch64-apple-darwin ;;
    x86_64) native=x86_64-apple-darwin ;;
    *) echo 'Unsupported macOS architecture' >&2; exit 2 ;;
esac
target="${1:-$native}"
[[ "$target" == "$native" ]] || { echo 'Release target must match native runner' >&2; exit 2; }
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
out="${RELEASE_OUT:-$CARGO_TARGET_DIR/dist}"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)"
cargo build --locked --release -p brain-cli --all-features --target "$target"
mkdir -p "$out"
artifact="$out/brain-$version-$target"
cp "$CARGO_TARGET_DIR/$target/release/brain" "$artifact"
chmod 755 "$artifact"
file "$artifact" | tee "$artifact.file"
otool -L "$artifact" > "$artifact.libraries"
# Keep the manifest format identical to the Linux download installer.
(cd "$out"; shasum -a 256 "brain-$version-$target" > SHA256SUMS)
printf 'Unsigned artifacts: %s\n' "$out"
