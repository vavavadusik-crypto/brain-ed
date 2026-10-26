#!/usr/bin/env bash
# Shared C/C++/ar driver; symlinked by build-release.sh in its temporary directory.
set -euo pipefail
case "$(basename "$0")" in
    cc|cxx)
        mode=cc
        [[ $(basename "$0") != cxx ]] || mode=c++
        args=()
        for arg in "$@"; do
            case "$arg" in
                --target=*|-m64|-march=*) ;;
                *) args+=("$arg") ;;
            esac
        done
        exec "${ZIG:-zig}" "$mode" -target "$BRAIN_ZIG_TARGET" "${args[@]}"
        ;;
    ar) exec "${ZIG:-zig}" ar "$@" ;;
    *) exit 2 ;;
esac
