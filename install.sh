#!/usr/bin/env bash
# Download only data; verify SHA-256 before installing. Never execute the payload.
set -euo pipefail
fail() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
usage() {
    cat <<'HELP'
Usage: ./install.sh [--version VERSION] [--dry-run] [--uninstall]
Linux x86_64 / aarch64 (including Android/Termux). No sudo or Rust required.
Installs only ~/.local/bin/brain. No models, launchers, agent edits or services.
--version VERSION  release version (default: 0.2.0; optional leading v)
--dry-run          print plan; no network, temporary files, writes or payload execution
--uninstall        remove only ~/.local/bin/brain; preserve all ~/.brain data
BRAIN_RELEASE_BASE_URL overrides the HTTPS release directory (for mirrors).
For source builds use packaging/install-source.sh instead.
HELP
}
version=0.2.0
dry_run=0
uninstall=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) [[ $# -ge 2 ]] || fail '--version needs a value'; version="${2#v}"; shift ;;
        --dry-run) dry_run=1 ;;
        --uninstall) uninstall=1 ;;
        -h|--help) usage; exit 0 ;;
        *) fail "unknown option: $1" ;;
    esac
    shift
done
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]] || fail 'invalid release version'
[[ ${HOME:-} = /* ]] || fail 'HOME must be absolute'
[[ $(uname -s) == Linux ]] || fail 'supported OS: Linux (including Android/Termux)'
case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) fail 'supported architectures: x86_64, aarch64' ;;
esac
binary="$HOME/.local/bin/brain"
if [[ $uninstall == 1 ]]; then
    printf 'Remove %s; keep ~/.brain data\n' "$binary"
    [[ $dry_run == 1 ]] || rm -f -- "$binary"
    exit 0
fi
asset="brain-$version-$arch-unknown-linux-musl"
base="${BRAIN_RELEASE_BASE_URL:-https://github.com/vavavadusik-crypto/brain-ed/releases/download/v$version}"
[[ "$base" == https://* && "$base" != *$'\n'* && "$base" != *$'\r'* ]] || fail 'release URL must use HTTPS'
printf 'Download %s/%s and SHA256SUMS\nVerify SHA-256, then install %s\n' "${base%/}" "$asset" "$binary"
if [[ $dry_run == 1 ]]; then
    echo 'Dry run: no network, files written or payload executed.'
    exit 0
fi
command -v curl >/dev/null || fail 'curl is required (Termux: pkg install curl)'
if command -v sha256sum >/dev/null; then
    checksum() { sha256sum "$1"; }
elif command -v shasum >/dev/null; then
    checksum() { shasum -a 256 "$1"; }
else
    fail 'sha256sum or shasum is required (Termux: pkg install coreutils)'
fi
work="$(mktemp -d)"
staged=''
cleanup() { rm -rf -- "$work"; [[ -z "$staged" ]] || rm -f -- "$staged"; }
trap cleanup EXIT
fetch() { curl --fail --location --retry 3 --connect-timeout 20 --proto '=https' --proto-redir '=https' --output "$2" "$1"; }
fetch "${base%/}/SHA256SUMS" "$work/SHA256SUMS"
fetch "${base%/}/$asset" "$work/$asset"
# Select exactly one digest for this exact filename. Never run manifest content.
expected="$(awk -v name="$asset" '$2 == name {print $1}' "$work/SHA256SUMS")"
[[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]] || fail 'missing, duplicate or malformed checksum'
actual="$(checksum "$work/$asset")"
actual="${actual%% *}"
[[ "${actual,,}" == "${expected,,}" ]] || fail 'SHA-256 mismatch; installed binary unchanged'
mkdir -p -- "$(dirname "$binary")"
staged="$(mktemp "$HOME/.local/bin/.brain-install.XXXXXX")"
cp -- "$work/$asset" "$staged"
chmod 755 "$staged"
mv -f -- "$staged" "$binary"
staged=''
printf 'Installed %s (SHA-256 %s)\n' "$binary" "$actual"
# shellcheck disable=SC2016 # literal $HOME/$PATH: a line for the user to paste
printf 'Next: export PATH="$HOME/.local/bin:$PATH"\nbrain init --agents --dry-run\n'
