#!/usr/bin/env bash
# Brain ED source installer for Linux (0.1.0 early preview).
#
# Builds the `brain` CLI from this checkout and asks the binary itself to write
# the two desktop launchers (Graph and Settings). Nothing is downloaded by this
# script beyond what `cargo build --locked` fetches for Cargo.lock; no model
# weights, no sudo, no uploads, no background services.
#
#   ./install.sh             build, then `brain ui --install-shortcut`
#   ./install.sh --dry-run   show what would happen; build nothing, change nothing
#   ./install.sh --uninstall remove the launchers; ~/.brain data is kept
#   ./install.sh --help

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: ./install.sh [--dry-run] [--uninstall] [--help]

Builds Brain ED from this source checkout and installs desktop launchers
for the current user only (Linux, XDG desktops).

  (no option)   cargo build --locked --release -p brain-cli, then
                target/release/brain ui --install-shortcut
                (copies the binary to ~/.brain/bin/brain and writes the
                "graph" and "settings" .desktop entries)
  --dry-run     print the planned commands and paths; runs no cargo build,
                downloads nothing, writes no file, starts no server
  --uninstall   brain ui --remove-shortcut: removes the launchers and icons;
                memory, settings and archives under ~/.brain are kept
  -h, --help    show this help

Not done by this script: sudo/system-wide install, model downloads
(run `brain model install` explicitly), agent config changes
(`brain init --agents`), the archive collector or its timer.
EOF
}

dry_run=0
uninstall=0
for argument in "$@"; do
    case "$argument" in
        --dry-run) dry_run=1 ;;
        --uninstall) uninstall=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            printf 'install.sh: unknown option: %s\n\n' "$argument" >&2
            usage >&2
            exit 2
            ;;
    esac
done

fail() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

# Prints a command with shell quoting, then runs it unless this is a dry run.
run() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'
    if [ "$dry_run" -eq 0 ]; then
        "$@"
    fi
}

[ "$(uname -s)" = "Linux" ] || fail "this installer supports Linux only; see docs/INSTALL.md"
[ "$(id -u)" -ne 0 ] || fail "do not run as root: Brain ED installs per user, without sudo"
case "${HOME:-}" in
    /*) ;;
    *) fail "HOME must be set to an absolute path" ;;
esac

repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
[ -f "$repo_dir/Cargo.lock" ] && [ -d "$repo_dir/brain-cli" ] \
    || fail "run install.sh from a Brain ED source checkout (Cargo.lock not found in $repo_dir)"

# Pin the target directory so an inherited CARGO_TARGET_DIR cannot move the
# build somewhere this script does not look.
target_dir="$repo_dir/target"
built_binary="$target_dir/release/brain"
stable_binary="$HOME/.brain/bin/brain"

if [ "$uninstall" -eq 1 ]; then
    if [ -x "$built_binary" ]; then
        brain="$built_binary"
    elif [ -x "$stable_binary" ]; then
        brain="$stable_binary"
    else
        fail "no brain binary found at $built_binary or $stable_binary; nothing to run --remove-shortcut with"
    fi
    if [ "$dry_run" -eq 1 ]; then
        # The CLI's own dry run only reports paths; it removes nothing.
        printf '+ %q ui --remove-shortcut --dry-run\n' "$brain"
        "$brain" ui --remove-shortcut --dry-run
        printf '\nDry run: nothing was removed. "would_remove" above lists what --uninstall would delete.\n'
        printf 'Data under %q would be kept.\n' "$HOME/.brain"
        exit 0
    fi
    run "$brain" ui --remove-shortcut
    printf '\nThe paths listed under "removed" above were deleted (an empty list means nothing was installed).\n'
    printf 'Data under %q was kept.\n' "$HOME/.brain"
    printf 'Agent integrations, if any, are removed separately: brain uninstall --dry-run\n'
    exit 0
fi

command -v cargo >/dev/null 2>&1 \
    || fail "cargo not found: install Rust via rustup (rust-toolchain.toml pins the version)"
if ! command -v python3 >/dev/null 2>&1; then
    printf 'note: python3 not found; only the optional archive collector tools need it.\n' >&2
fi

printf 'Brain ED source install (early preview)\n  source: %s\n  binary: %s\n  stable copy: %s\n\n' \
    "$repo_dir" "$built_binary" "$stable_binary"

# Build from the checkout root so rust-toolchain.toml selects the pinned
# toolchain even when install.sh is invoked from another directory.
cd -- "$repo_dir"
run cargo build --locked --release -p brain-cli \
    --manifest-path "$repo_dir/Cargo.toml" --target-dir "$target_dir"

if [ "$dry_run" -eq 1 ]; then
    printf '+ %q ui --install-shortcut\n' "$built_binary"
    if [ -x "$built_binary" ]; then
        # Read-only plan from an existing build: paths only, nothing written.
        "$built_binary" ui --install-shortcut --dry-run
    else
        printf '(no existing build; the shortcut plan is printed after a real build)\n'
    fi
    printf '\nDry run: nothing was built, downloaded or written.\n'
    exit 0
fi

[ -x "$built_binary" ] || fail "build reported success but $built_binary is missing"
run "$built_binary" ui --install-shortcut

cat <<EOF

Installed. Next steps:
  $stable_binary --help
  $stable_binary init                # create ~/.brain/brain.sqlite
  $stable_binary ui --page graph     # or use the "graph" launcher

If a desktop icon shows as untrusted, right-click it and choose
"Allow Launching". Embeddings are optional: brain model install
EOF
