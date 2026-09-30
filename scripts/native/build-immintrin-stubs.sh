#!/usr/bin/env bash
#
# NERV Desk — rebuild analysis/build-env/stubs/obj/immintrin_stubs.o
#
# WHY THIS EXISTS
# ---------------
# When libsodium is cross-compiled for x86_64-pc-windows-msvc with clang, the
# Argon2 SSE/AVX2 code paths reference a handful of `_mm_*` intrinsics that
# clang's MSVC-target headers (from xwin) do not provide as builtins in this
# configuration.  The link then fails with:
#
#   lld-link-19: error: undefined symbol: _mm_slli_epi64
#   >>> referenced by liblibsodium_sys-*.rlib(crypto_pwhash_argon2_argon2-fill-block-ssse3.obj)
#
# `immintrin_stubs.c` supplies plain-C replacements.  This script compiles it to
# a **COFF** object with the repository's own clang wrapper (so the xwin headers
# and the msvc target triple are correct), and checks the expected symbol count.
#
# CRITICAL: the resulting .o must be passed to lld-link as a STANDALONE
# positional argument, NOT packed into sodium.lib.  If it is inside the archive,
# rustc embeds it into liblibsodium_sys-*.rlib and lld-link reports duplicate
# `_mm_*` definitions once sodium.lib is also passed.  See
# scripts/msvc-shim/link.sh (IMMINTRIN_STUBS_O) and scripts/cross-msvc.env
# (NERV_IMMINTRIN_STUBS_O).
#
# USAGE
#   scripts/native/build-immintrin-stubs.sh [--check]
#     --check   build to a temp dir and compare symbol identity with the
#               installed object instead of overwriting it.
#
# EXIT CODES
#   0  success (object present, expected symbol count)
#   1  toolchain/source missing
#   2  compile failed
#   3  symbol check failed

set -euo pipefail

NERV_REPO_ROOT="${NERV_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
NERV_BE="${NERV_BE:-$(cd "$NERV_REPO_ROOT/../.." && pwd)}"
NERV_SHIM="${NERV_SHIM:-$NERV_REPO_ROOT/scripts/msvc-shim}"

STUB_SRC="${NERV_IMMINTRIN_STUB_SRC:-$NERV_BE/analysis/build-env/stubs/immintrin_stubs.c}"
OBJ_DIR="${NERV_IMMINTRIN_OBJ_DIR:-$NERV_BE/analysis/build-env/stubs/obj}"
OBJ_OUT="$OBJ_DIR/immintrin_stubs.o"
CLANG_WRAP="$NERV_SHIM/clang-wrap.sh"
TARGET_TRIPLE="x86_64-pc-windows-msvc"

# The expected number of external (T) symbols.  Bump this — with a comment
# saying which intrinsic was added — whenever immintrin_stubs.c gains a
# function, so a silently truncated object cannot pass as green.
EXPECTED_T_SYMBOLS="${NERV_IMMINTRIN_EXPECTED_T:-77}"

CHECK_ONLY=0
case "${1:-}" in
    --check) CHECK_ONLY=1 ;;
    -h | --help)
        sed -n '3,40p' "${BASH_SOURCE[0]}"
        exit 0
        ;;
    "") ;;
    *)
        echo "unknown argument: $1" >&2
        exit 1
        ;;
esac

fail() {
    echo "ERROR: $*" >&2
    exit "${2:-2}"
}

for f in "$STUB_SRC" "$CLANG_WRAP"; do
    [ -f "$f" ] || fail "missing required file: $f" 1
done
command -v llvm-nm-19 >/dev/null || fail "llvm-nm-19 not on PATH" 1

mkdir -p "$OBJ_DIR"

build_one() { # <output path>
    local out="$1"
    echo ">> compiling $STUB_SRC -> $out"
    "$CLANG_WRAP" \
        --target="$TARGET_TRIPLE" \
        -O2 -ffunction-sections -fdata-sections \
        -c "$STUB_SRC" -o "$out"
}

count_t() { # <object>
    llvm-nm-19 --defined-only "$1" 2>/dev/null | tr -d '\r' | grep -c ' T ' || true
}

if [ "$CHECK_ONLY" -eq 1 ]; then
    tmp="${TMPDIR:-/tmp}/nerv-instrin-check.$$.o"
    trap 'rm -f "$tmp"' EXIT
    build_one "$tmp"
    new_t=$(count_t "$tmp")
    echo "fresh build : $new_t T symbols"
    if [ -f "$OBJ_OUT" ]; then
        old_t=$(count_t "$OBJ_OUT")
        echo "installed   : $old_t T symbols"
        if diff <(llvm-nm-19 --defined-only "$tmp" | tr -d '\r' | awk '$2=="T"{print $3}' | sort) \
            <(llvm-nm-19 --defined-only "$OBJ_OUT" | tr -d '\r' | awk '$2=="T"{print $3}' | sort) >/dev/null; then
            echo "symbol identity: MATCH (rebuild is reproducible)"
        else
            echo "symbol identity: DIFFERS — installed object is not what this source produces" >&2
            exit 3
        fi
    else
        echo "installed object absent; nothing to compare" >&2
    fi
    [ "$new_t" -eq "$EXPECTED_T_SYMBOLS" ] || {
        echo "symbol count $new_t != expected $EXPECTED_T_SYMBOLS" >&2
        exit 3
    }
    echo "OK"
    exit 0
fi

build_one "$OBJ_OUT"

got_t=$(count_t "$OBJ_OUT")
echo "installed: $OBJ_OUT ($(stat -c%s "$OBJ_OUT") B, $got_t T symbols)"
if [ "$got_t" -ne "$EXPECTED_T_SYMBOLS" ]; then
    echo "ERROR: T-symbol count $got_t != expected $EXPECTED_T_SYMBOLS." >&2
    echo "       If you added an intrinsic to immintrin_stubs.c, update" >&2
    echo "       NERV_IMMINTRIN_EXPECTED_T (default in this script)." >&2
    exit 3
fi

echo "-- check the object really is COFF, not ELF --"
if llvm-objdump-19 -f "$OBJ_OUT" 2>/dev/null | grep -qi 'file format coff'; then
    echo "format: COFF (correct for msvc target)"
else
    echo "ERROR: $OBJ_OUT is not a COFF object — clang-wrap.sh did not apply the msvc target." >&2
    llvm-objdump-19 -f "$OBJ_OUT" 2>&1 | head -5 >&2
    exit 3
fi

echo "DONE: $OBJ_OUT"
