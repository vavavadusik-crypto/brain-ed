#!/usr/bin/env bash
# Brain ED Linux bundle installer.
#
#   ./install-linux.sh --dry-run     check the bundle, show what would be installed
#   ./install-linux.sh               check the bundle, then install the launchers
#   ./install-linux.sh --uninstall [--dry-run]
#   ./install-linux.sh --help
#
# Before the bundled binary runs at all, the unpacked bundle must hold exactly
# the expected files and directories (no symlinks, special files or extras;
# any error while listing it fails closed), and SHA256SUMS must be bounded,
# strict, list each expected file exactly once and match every file. These
# checksums come from the same download, so they detect corruption or a
# partial unpack; they do not prove who built the binary.
#
# Installing runs the verified bundled binary's own `brain ui
# --install-shortcut`: it copies itself to ~/.brain/bin/brain and writes two
# application-menu entries and, when an XDG desktop directory exists, two
# desktop icons (Settings, Graph). Uninstalling runs the same verified bundled
# binary's `brain ui --remove-shortcut`; the installed copy in ~/.brain/bin is
# never executed by this script, because nothing here can check it. No agent
# integration, archive collector, network listener or embedding model is
# installed or enabled. Databases, settings, archives, models and configs
# under ~/.brain are never deleted. No sudo, no downloads.
set -euo pipefail
export LC_ALL=C

MAX_SUMS_LINES=64
MAX_SUMS_BYTES=16384
MAX_FILE_BYTES=$((512 * 1024 * 1024))
# The exact inventory of a bundle: type (f/d) and path relative to its top.
EXPECTED_FILES=(LICENSE MANIFEST.json README.md SHA256SUMS THIRD_PARTY_NOTICES.md bin/brain
    docs/INSTALL.md docs/linux-download.md install-linux.sh)
EXPECTED_DIRS=(bin docs)

usage() {
    cat <<'EOF'
Usage: ./install-linux.sh [--dry-run] [--uninstall] [--help]

  (no option)   verify the bundle, then install launchers for this user
  --dry-run     verify the bundle and print the planned paths; no installation
                changes (a private temporary copy of the binary is written and
                removed on exit)
  --uninstall   verify the bundle, then remove the launchers; ~/.brain data is kept
  --help        show this text

Linux x86_64 only, as a normal user (never root). Uninstall uses the verified
binary from this bundle. Without the bundle you can run
  ~/.brain/bin/brain ui --remove-shortcut
yourself; that installed copy is not checked by this script.
Portability across distributions is not yet proven; see docs/linux-download.md.
EOF
}

fail() { printf 'install-linux: %s\n' "$*" >&2; exit 1; }

dry_run=0
uninstall=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        --uninstall) uninstall=1 ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; fail "unknown option: $arg" ;;
    esac
done

[ "$(id -u)" -ne 0 ] || fail "refusing to run as root; run as the user who will use Brain"
[ "$(uname -s)" = "Linux" ] || fail "this bundle is for Linux only"
[ "$(uname -m)" = "x86_64" ] || fail "this bundle is for x86_64 only (found $(uname -m))"
case "${HOME:-}" in /*) ;; *) fail "HOME must be an absolute path" ;; esac
for tool in sha256sum find stat sort wc tail grep cut cp mktemp chmod rm head tr cat; do
    command -v "$tool" >/dev/null || fail "required tool not found: $tool"
done

bundle="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
sums="$bundle/SHA256SUMS"

verify_bundle() {
    # Every entry, the top directory included, must be owned by this user and
    # not group/world-writable, so no other account can swap content between
    # the checks and the run. This bounds trust to the own user account; it
    # does not stop a process running as the same user.
    local unsafe
    unsafe="$(find "$bundle" \( ! -uid "$(id -u)" -o -perm /022 \) -print -quit)" || fail "cannot check bundle ownership and modes"
    [ -z "$unsafe" ] || fail "bundle entry not owned by you or group/world-writable: ${unsafe#"$bundle"}; unpack it again into your own directory"
    # Inventory: exactly the expected entries. find errors (unreadable
    # directory, vanished entry) fail closed through the explicit status check.
    local inventory expected
    inventory="$(LC_ALL=C find "$bundle" -mindepth 1 -printf '%y %P\n')" || fail "cannot list the bundle completely"
    inventory="$(printf '%s\n' "$inventory" | LC_ALL=C sort)"
    expected="$( { printf 'd %s\n' "${EXPECTED_DIRS[@]}"; printf 'f %s\n' "${EXPECTED_FILES[@]}"; } | LC_ALL=C sort)"
    [ "$inventory" = "$expected" ] || fail "bundle content is not exactly the expected files (extra, missing, link or special file)"
    local size lines
    size="$(stat -c %s -- "$sums")"
    [ "$size" -le "$MAX_SUMS_BYTES" ] || fail "SHA256SUMS is larger than $MAX_SUMS_BYTES bytes"
    # The shell cannot hold NUL bytes (command substitution drops them), so
    # the raw file is checked for NUL first, with a bounded read.
    local nuls
    nuls="$(head -c "$((MAX_SUMS_BYTES + 1))" -- "$sums" | tr -cd '\000' | wc -c)" || fail "cannot read SHA256SUMS"
    [ "$nuls" -eq 0 ] || fail "SHA256SUMS contains NUL bytes; the bundle is corrupted"
    # Read once, bounded; every later check and the expected binary hash use
    # this snapshot. Its byte count (LC_ALL=C) must equal the size measured
    # above, so a dropped byte or a size change between the reads is refused.
    sums_text="$(head -c "$((MAX_SUMS_BYTES + 1))" -- "$sums"; printf x)" || fail "cannot read SHA256SUMS"
    sums_text="${sums_text%x}"
    [ "${#sums_text}" -eq "$size" ] || fail "SHA256SUMS changed or holds bytes the snapshot cannot keep; not using it"
    lines="$(printf '%s' "$sums_text" | wc -l)"
    [ "$lines" -ge 1 ] && [ "$lines" -le "$MAX_SUMS_LINES" ] || fail "SHA256SUMS has $lines lines (allowed 1..$MAX_SUMS_LINES)"
    [ "${sums_text: -1}" = $'\n' ] || fail "SHA256SUMS must end with a newline"
    local listed="" line path
    brain_hash=""
    while IFS= read -r line; do
        [[ "$line" =~ ^([0-9a-f]{64})\ \ ([A-Za-z0-9_][A-Za-z0-9._-]*(/[A-Za-z0-9_][A-Za-z0-9._-]*)*)$ ]] || fail "malformed SHA256SUMS line: $line"
        path="${BASH_REMATCH[2]}"
        [ "$path" != "bin/brain" ] || brain_hash="${BASH_REMATCH[1]}"
        case "/$path/" in */../* | */./*) fail "unsafe path in SHA256SUMS: $path" ;; esac
        [ "$(stat -c %s -- "$bundle/$path")" -le "$MAX_FILE_BYTES" ] || fail "file too large: $path"
        listed="$listed$path
"
    done < <(printf '%s' "$sums_text")
    [ "$(printf '%s' "$listed" | LC_ALL=C sort)" = "$(printf '%s\n' "${EXPECTED_FILES[@]}" | grep -vxF SHA256SUMS | LC_ALL=C sort)" ] \
        || fail "SHA256SUMS must list each bundle file exactly once"
    printf '%s' "$sums_text" | (cd -- "$bundle" && sha256sum --check --strict --quiet -) >&2 || fail "checksum mismatch: the bundle is corrupted or incomplete; download it again"
    [ -n "$brain_hash" ] || fail "SHA256SUMS does not list bin/brain"
}

verify_bundle
printf 'install-linux: bundle verified (%s)\n' "$bundle" >&2
# Run a private copy, not the bundle path: copy into a fresh 0700 directory and
# check the copy against the hash from the SHA256SUMS snapshot, so a later
# change to the bundle cannot change what runs.
stage="$(mktemp -d "${TMPDIR:-/tmp}/brain-ed-install.XXXXXXXX")" || fail "cannot create a private staging directory"
trap 'rm -rf -- "$stage"' EXIT
chmod 700 -- "$stage"
cp -- "$bundle/bin/brain" "$stage/brain" || fail "cannot stage bin/brain"
[ "$(sha256sum -- "$stage/brain" | cut -c1-64)" = "$brain_hash" ] || fail "bin/brain changed after verification; not running it"
chmod 700 -- "$stage/brain"
binary="$stage/brain"
if [ "$uninstall" -eq 1 ]; then
    if [ "$dry_run" -eq 1 ]; then
        "$binary" ui --remove-shortcut --dry-run
    else
        "$binary" ui --remove-shortcut
    fi
    printf 'install-linux: data under %s/.brain was kept\n' "$HOME" >&2
elif [ "$dry_run" -eq 1 ]; then
    "$binary" ui --install-shortcut --dry-run
    printf 'install-linux: dry run: no installation changes (the private temporary copy is removed on exit)\n' >&2
else
    "$binary" ui --install-shortcut
    printf 'install-linux: done. Optional steps (agents, archive, model) stay off until you run them yourself.\n' >&2
fi
