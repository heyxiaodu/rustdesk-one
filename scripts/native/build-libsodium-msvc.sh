#!/usr/bin/env bash
#
# NERV Desk — rebuild the cross-compiled libsodium static library for
# x86_64-pc-windows-msvc (COFF/PE archive `sodium.lib`).
#
# WHY THIS EXISTS
# ---------------
# The cross-build consumes a hand-rolled `sodium.lib`.  That archive used to be
# produced ad hoc, with no script in the tree, so it could only be reproduced by
# reading prose.  This script is the reproduction.
#
# THE ONE FLAG THAT MATTERS:  -DSODIUM_STATIC
# -------------------------------------------
# Without it, `sodium/export.h:16-24` compiles every `SODIUM_EXPORT` under
# `_MSC_VER` as `__declspec(dllimport)`.  For a *static* archive that is wrong,
# and it is catastrophic for DATA symbols: `randombytes_sysrandom_implementation`
# is a struct (`D`).  A dllimport reference to it becomes "load a pointer from an
# import slot", so once the linker aliases the reference to the struct itself it
# loads the struct's first field (`const char *name`) and dereferences that as an
# implementation pointer.  Result, deterministically, at run time under Wine:
#
#   wine: Unhandled page fault on read access to FFFFFFFFFFFFFFFF
#         at address 0000000142975C8A
#
# `llvm-symbolizer-19` resolves that address to `randombytes_implementation_name`.
# libsodium's own MSVC project files agree this define is required:
#   builds/msvc/vs20XX/libsodium/libsodium.props:27
#   builds/msvc/vs20XX/libsodium/libsodium.import.props:19
#
# With `-DSODIUM_STATIC` the new archive's `__imp_*` references drop to exactly
# the ten that are genuine Windows API imports (critical sections, Sleep,
# VirtualAlloc family) — checked by this script.
#
# USAGE
#   scripts/native/build-libsodium-msvc.sh [--jobs N] [--out DIR] [--keep-src]
#
# OPTIONS
#   --jobs N     parallel compile jobs (default: nproc)
#   --out DIR    output directory (default: $NERV_SODIUM_LIB_DIR, else the
#                default analysis/build-env/nervdesk-sodium-msvc)
#   --keep-src   do not delete the extracted source tree on success
#
# EXIT CODES
#   0  success
#   1  prerequisite missing (toolchain / include tree)
#   2  reserved (was: download/checksum failure)
#   3  compile or archive failure
#   4  post-build symbol verification failed

set -euo pipefail

NERV_REPO_ROOT="${NERV_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
NERV_BE="${NERV_BE:-$(cd "$NERV_REPO_ROOT/../.." && pwd)}"
NERV_SHIM="${NERV_SHIM:-$NERV_REPO_ROOT/scripts/msvc-shim}"

# The libsodium source comes from the tree the `libsodium-sys` crate vendors --
# see "SOURCE OF TRUTH" below for why the upstream 1.0.18 tarball is the WRONG
# source despite the version string both trees carry.  Pinned to the version in
# repos/rustdesk/Cargo.lock.
SODIUM_SYS_VERSION="${NERV_SODIUM_SYS_VERSION:-0.2.7}"

# Expected post-build invariants.  These are the acceptance criteria for the
# fix described above; a build that violates them must not be installed.
EXPECTED_MEMBERS=106
EXPECTED_IMP_SYMBOLS=10
IMP_ALLOWLIST='^__imp_(Enter|Initialize|Leave)CriticalSection$|^__imp_Sleep$|^__imp_Virtual(Alloc|Free|Lock|Protect|Unlock)$|^__imp_GetSystemInfo$'

JOBS="$(nproc 2>/dev/null || echo 4)"
OUT_DIR="${NERV_SODIUM_LIB_DIR:-$NERV_BE/analysis/build-env/nervdesk-sodium-msvc}"
KEEP_SRC=0

while [ $# -gt 0 ]; do
    case "$1" in
        --jobs)
            JOBS="$2"
            shift 2
            ;;
        --out)
            OUT_DIR="$2"
            shift 2
            ;;
        --keep-src)
            KEEP_SRC=1
            shift
            ;;
        -h | --help)
            sed -n '3,52p' "${BASH_SOURCE[0]}"
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

CLANG_WRAP="${NERV_SHIM:-$NERV_SHIM}/clang-wrap.sh"
TARGET_TRIPLE="x86_64-pc-windows-msvc"
LLVM_AR="${NERV_LLVM_AR:-/usr/bin/llvm-ar-19}"
LLVM_NM="${NERV_LLVM_NM:-/usr/bin/llvm-nm-19}"
LLVM_OBJDUMP="${NERV_LLVM_OBJDUMP:-/usr/bin/llvm-objdump-19}"

fail() {
    echo "ERROR: $*" >&2
    exit "${2:-3}"
}
note() { printf '\n=== %s ===\n' "$*"; }

note "prerequisites"
[ -f "$CLANG_WRAP" ] || fail "missing required file: $CLANG_WRAP" 1
for t in "$LLVM_AR" "$LLVM_NM" "$LLVM_OBJDUMP" curl sha256sum tar find xargs; do
    command -v "$t" >/dev/null || fail "$t not available" 1
done
echo "clang wrapper : $CLANG_WRAP"
echo "output dir    : $OUT_DIR"
echo "jobs          : $JOBS"

# ---------------------------------------------------------------------------
note "locate libsodium source (libsodium-sys ${SODIUM_SYS_VERSION} vendored tree)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/nerv-sodium.XXXXXX")"
cleanup() {
    if [ "$KEEP_SRC" -eq 1 ]; then
        echo "source kept at: $WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

# SOURCE OF TRUTH
# ---------------
# NOT the upstream libsodium-1.0.18.tar.gz.  The archive this script must
# reproduce was built from the libsodium tree that the `libsodium-sys` crate
# vendors, and that tree is NOT upstream 1.0.18 even though its
# builds/msvc/version.h still advertises 10.3.  Upstream 1.0.18 has no
# `fe25519_mul32`/`fe25519_notsquare`/`fe25519_sqmul` anywhere, while the shipped
# sodium.lib's crypto_core_ed25519_ref10_ed25519_ref10.obj defines them and has
# no `chi25519` (which upstream 1.0.18 does have).  Building from the upstream
# tarball therefore yields 28 of 106 members that differ from the shipped
# archive.  The crate tree is used instead, and its identity is fingerprinted
# below so a wrong source cannot be built silently.
find_registry_src() {
    local root base
    for base in "${CARGO_HOME:-$HOME/.cargo}/registry/src" "$HOME/.cargo/registry/src"; do
        [ -d "$base" ] || continue
        # shellcheck disable=SC2086
        for root in $base/*/libsodium-sys-${SODIUM_SYS_VERSION}; do
            if [ -d "$root/libsodium" ]; then
                printf '%s\n' "$root/libsodium"
                return 0
            fi
        done
    done
    return 1
}

if [ -n "${NERV_SODIUM_SRC:-}" ]; then
    SRC="$NERV_SODIUM_SRC"
    [ -d "$SRC" ] || fail "NERV_SODIUM_SRC=$SRC is not a directory" 1
    echo "source: NERV_SODIUM_SRC=$SRC"
elif SRC="$(find_registry_src)"; then
    echo "source: crate registry libsodium-sys-${SODIUM_SYS_VERSION} ($SRC)"
else
    echo "ERROR: no libsodium-sys-${SODIUM_SYS_VERSION} source tree found." >&2
    echo "       The crate bundles the libsodium source the shipped archive was" >&2
    echo "       built from; fetch it with 'cargo fetch' from repos/rustdesk, then" >&2
    echo "       re-run.  Set NERV_SODIUM_SRC to override." >&2
    fail "libsodium-sys source tree not found" 1
fi

SRC_INCLUDE="$SRC/src/libsodium/include"
[ -f "$SRC_INCLUDE/sodium/export.h" ] || fail "sodium/export.h missing in $SRC" 1
[ -f "$SRC_INCLUDE/sodium/core.h" ] || fail "sodium/core.h missing in $SRC" 1

# --- provenance fingerprint -------------------------------------------------
# Three cheap checks that together pin the tree identity.  The middle one is the
# discriminator between the libsodium-sys tree and upstream 1.0.18; it exists
# because that difference was previously mis-documented as "libsodium 1.0.18"
# and silently produced a non-matching archive.
N_C_SRC=$(find "$SRC/src/libsodium" -name '*.c' | wc -l | tr -d ' ')
[ "$N_C_SRC" -eq "$EXPECTED_MEMBERS" ] \
    || fail "expected $EXPECTED_MEMBERS .c sources in $SRC/src/libsodium, found $N_C_SRC" 1
ED25519="$SRC/src/libsodium/crypto_core/ed25519/ref10/ed25519_ref10.c"
if ! grep -q 'fe25519_notsquare' "$ED25519" 2>/dev/null; then
    echo "ERROR: $ED25519 lacks fe25519_notsquare." >&2
    echo "       This is NOT the tree the shipped sodium.lib was built from --" >&2
    echo "       most likely the upstream 1.0.18 tarball instead of the" >&2
    echo "       libsodium-sys-${SODIUM_SYS_VERSION} vendored source.  Refusing to build." >&2
    exit 1
fi
if grep -q 'chi25519' "$ED25519" 2>/dev/null; then
    echo "ERROR: $ED25519 defines chi25519, which the shipped archive does not." >&2
    echo "       Wrong source tree.  Refusing to build." >&2
    exit 1
fi
echo "provenance: $N_C_SRC sources, ed25519_ref10 fingerprint OK"

# version.h is generated by autotools upstream; MSVC ships a template.  Install
# it into a private overlay include dir so the pristine source stays untouched
# (the registry tree must not be written to).  It must be on the include path:
# src/libsodium/sodium/version.c:2 does `#include "version.h"` and fails
# otherwise.  The overlay is keyed on the `sodium/` subdir, so the directory
# added below is "$OVERLAY/sodium".
OVERLAY="$WORK/overlay"
mkdir -p "$OVERLAY/sodium"
cp "$SRC/builds/msvc/version.h" "$OVERLAY/sodium/version.h" || fail "cannot install version.h overlay" 1

# ---------------------------------------------------------------------------
note "compile sources with -DSODIUM_STATIC"
OBJ="$WORK/obj"
mkdir -p "$OBJ"

# libsodium's translation units `#include` their own private sibling headers by
# bare filename (e.g. crypto_hash/sha256/cp/hash_sha256_cp.c does
# `#include "crypto_hash_sha256.h"`).  autotools handles this by adding every
# directory under src/libsodium to the include path.  We reproduce that: for
# each directory in the tree — and each of its parent/grandparent levels, so
# `crypto_hash_sha256.h` is visible from src/libsodium/include/sodium too — emit
# an -I.  A correct build needs on the order of 90 of these for 1.0.18.
INCLUDE_ARGS_FILE="$WORK/inc.args"
{
    printf '%s\n' "-I$SRC_INCLUDE" "-I$OVERLAY/sodium"
    find "$SRC/src/libsodium" -type d | while read -r d; do
        printf '%s\n' "-I$d"
        printf '%s\n' "-I$(dirname "$d")"
        printf '%s\n' "-I$(dirname "$(dirname "$d")")"
    done
} | sort -u >"$INCLUDE_ARGS_FILE"
N_INC=$(wc -l <"$INCLUDE_ARGS_FILE" | tr -d ' ')
echo "include dirs: $N_INC"

# Target is always x86_64: the wrapper adds the xwin CRT/SDK -I paths itself and
# this script passes --target explicitly, so the msvc target is never guessed.
# No -march flags: x86_64 already implies SSE2, and enabling SSSE3/AVX would
# multiply the number of `_mm_*` intrinsics needing stubs (see
# build-immintrin-stubs.sh) for negligible gain in a debug-oriented cross build.
# The include list goes in a response file because ARG_MAX is not the only
# concern — a 90-entry -I list on argv is fragile; @file is what clang expects.
# NOTE: no -fPIC here.  clang rejects it for an msvc target
# ("unsupported option '-fPIC' for target 'x86_64-pc-windows-msvc'"), and
# clang-wrap.sh only strips it from argv -- anything inside a response file
# bypasses that filter, so it must simply not be written.  A static COFF archive
# does not need PIC anyway.
#
# OPTIMIZATION: deliberately no -O flag, i.e. clang's default -O0.  This is not
# an oversight.  The archive currently installed in
# analysis/build-env/nervdesk-sodium-msvc/lib/sodium.lib -- the one the verified
# rustdesk.exe/librustdesk.dll were linked against -- was built at -O0, and this
# script exists to reproduce *that* artifact, not to improve on it.  Verified
# empirically: the same translation unit compiled at -O0 is byte-identical to the
# shipped archive member apart from the 2-byte COFF TimeDateStamp, whereas -O2
# yields an object roughly half the size whose function set differs.  Changing
# the optimization level changes which `_mm_*` intrinsics the archive leaves
# undefined, so it is a linker-visible change and must not be made silently.
CFLAGS_FILE="$WORK/cflags.rsp"
{
    printf '%s\n' "--target=$TARGET_TRIPLE"
    printf '%s\n' "-ffunction-sections" "-fdata-sections" "-DSODIUM_STATIC"
    cat "$INCLUDE_ARGS_FILE"
} >"$CFLAGS_FILE"

find "$SRC/src/libsodium" -name '*.c' -print0 >"$WORK/files.z"
N_SRC=$(tr -cd '\0' <"$WORK/files.z" | wc -c)
echo "sources: $N_SRC"
[ "$N_SRC" -gt 0 ] || fail "no .c sources found under $SRC/src/libsodium" 1

echo "compiling with $JOBS jobs ..."
start=$(date +%s)
# Object names reproduce the shipped archive's convention exactly: the source
# path relative to src/libsodium with '/' replaced by '_'.  So
# crypto_aead/aes256gcm/aesni/aead_aes256gcm_aesni.c becomes the member
# crypto_aead_aes256gcm_aesni_aead_aes256gcm_aesni.obj, and `llvm-ar t` output is
# directly comparable against the shipped archive.  Member names do not affect
# linking, but matching them keeps the reproduction verifiable, and unlike bare
# basenames it cannot collide (libsodium has no basename collisions today, but
# flattening removes the failure mode entirely).
# shellcheck disable=SC2016
xargs -0 -P "$JOBS" -I{} bash -c '
    c="$1"; obj="$2"; wrapper="$3"; nameroot="$4"; rsp="$5"
    rel="${c#$nameroot/}"
    out="$obj/$(printf "%s" "${rel%.c}" | tr "/" "_").obj"
    if ! "$wrapper" "@$rsp" -c "$c" -o "$out" 2>"$out.log"; then
        touch "$out.failed"
    else
        rm -f "$out.log"
    fi
' _ {} "$OBJ" "$CLANG_WRAP" "$SRC/src/libsodium" "$CFLAGS_FILE" <"$WORK/files.z" || true
end=$(date +%s)

FAILED=$(find "$OBJ" -name '*.failed' | wc -l)
N_OBJ=$(find "$OBJ" -name '*.obj' | wc -l)
echo "objects: $N_OBJ   failed: $FAILED   ($((end - start))s)"
if [ "$FAILED" -ne 0 ]; then
    echo "--- first failing translation unit ---" >&2
    first=$(find "$OBJ" -name '*.failed' | head -1)
    sed -n '1,40p' "${first%.failed}.log" >&2
    fail "compilation failed in $FAILED translation unit(s)" 3
fi
[ "$N_OBJ" -eq "$EXPECTED_MEMBERS" ] || fail "expected $EXPECTED_MEMBERS objects, got $N_OBJ" 3

# ---------------------------------------------------------------------------
note "archive -> sodium.lib"
STAGE="$WORK/stage"
mkdir -p "$STAGE"
find "$OBJ" -name '*.obj' -print0 | sort -z | xargs -0 "$LLVM_AR" rcs "$STAGE/sodium.lib" \
    || fail "llvm-ar failed" 3

got_members=$("$LLVM_AR" t "$STAGE/sodium.lib" 2>/dev/null | tr -d '\r' | wc -l)
echo "members: $got_members (expected $EXPECTED_MEMBERS)"
[ "$got_members" -eq "$EXPECTED_MEMBERS" ] || fail "member count mismatch" 4

note "verify __imp_ references (must be exactly the $EXPECTED_IMP_SYMBOLS Windows APIs)"
"$LLVM_NM" "$STAGE/sodium.lib" 2>/dev/null | tr -d '\r' | grep '__imp' | awk '{print $NF}' | sort -u >"$WORK/imp.txt" || true
[ -s "$WORK/imp.txt" ] && sed 's/^/  /' "$WORK/imp.txt"
got_imp=$(wc -l <"$WORK/imp.txt" | tr -d ' ')
echo "count: $got_imp (expected $EXPECTED_IMP_SYMBOLS)"
if [ "$got_imp" -ne "$EXPECTED_IMP_SYMBOLS" ]; then
    echo "ERROR: SODIUM_STATIC did not take effect, or the source changed." >&2
    echo "       Unresolved __imp_ symbols belong to libsodium itself and will" >&2
    echo "       reproduce the 0xFFFFFFFFFFFFFFFF page fault at run time." >&2
    exit 4
fi
if grep -Ev "$IMP_ALLOWLIST" "$WORK/imp.txt" >/dev/null; then
    echo "ERROR: unexpected __imp_ symbol(s) that are not Windows APIs:" >&2
    grep -Ev "$IMP_ALLOWLIST" "$WORK/imp.txt" | sed 's/^/  /' >&2
    exit 4
fi
echo "all __imp_ references are Windows API imports: OK"

note "verify format and key definitions"
# Dump the symbol table and objdump header to files FIRST.  Under `set -o
# pipefail` (line 50) a `cmd | grep -q PAT` pipeline is a landmine: grep -q exits
# on the first match, the producer takes SIGPIPE (141) while still writing, and
# pipefail propagates 141 as the pipeline status -- so the `if` sees failure even
# though the match succeeded.  Grepping a regular file has no upstream process to
# kill, so the -q form is safe here.
"$LLVM_NM" --defined-only "$STAGE/sodium.lib" 2>/dev/null | tr -d '\r' >"$WORK/defined.txt" || true
"$LLVM_OBJDUMP" -f "$STAGE/sodium.lib" 2>/dev/null >"$WORK/objdump.txt" || true

if grep -qi 'coff' "$WORK/objdump.txt"; then
    echo "archive format: COFF (correct)"
else
    echo "WARN: could not confirm COFF archive format" >&2
fi
for sym in sodium_init randombytes_buf randombytes_implementation_name randombytes_sysrandom_implementation; do
    if grep -q " $sym\$" "$WORK/defined.txt"; then
        echo "  defined: $sym"
    else
        fail "expected symbol not defined: $sym" 4
    fi
done

# ---------------------------------------------------------------------------
# Link-compatibility gate: every `_mm_*` intrinsic the archive leaves UNDEFINED
# must be provided by the immintrin stub object that scripts/msvc-shim/link.sh
# appends to the link line.  This is the build-time guard for the defect that
# produced `lld-link-19: error: undefined symbol: _mm_slli_epi64 ... referenced by
# liblibsodium_sys-*.rlib(crypto_pwhash_argon2_argon2-fill-block-ssse3.obj)`.
# clang's MSVC headers do not provide every intrinsic, so the stub is what makes
# this archive linkable at all; a mismatch is a guaranteed link failure, so it is
# checked here rather than discovered 3 minutes into a cargo build.
if [ -n "${NERV_IMMINTRIN_STUBS_O:-}" ] && [ -f "${NERV_IMMINTRIN_STUBS_O}" ]; then
    note "verify _mm_* stub coverage against $NERV_IMMINTRIN_STUBS_O"
    "$LLVM_NM" "$STAGE/sodium.lib" 2>/dev/null | tr -d '\r' \
        | awk '$1 == "U" || $2 == "U" { print $NF }' | grep '^_mm' | sort -u >"$WORK/mm_need.txt" || true
    "$LLVM_NM" --defined-only "${NERV_IMMINTRIN_STUBS_O}" 2>/dev/null | tr -d '\r' \
        | awk '{ print $NF }' | grep '^_mm' | sort -u >"$WORK/mm_have.txt" || true
    n_need=$(wc -l <"$WORK/mm_need.txt" | tr -d ' ')
    n_have=$(wc -l <"$WORK/mm_have.txt" | tr -d ' ')
    echo "intrinsics needed: $n_need   provided by stub: $n_have"
    if [ "$n_need" -gt 0 ] && [ "$n_have" -gt 0 ]; then
        missing="$WORK/mm_missing.txt"
        comm -23 "$WORK/mm_need.txt" "$WORK/mm_have.txt" >"$missing"
        if [ -s "$missing" ]; then
            n_missing=$(wc -l <"$missing" | tr -d ' ')
            echo "ERROR: the archive needs $n_missing intrinsic(s) the stub does not provide:" >&2
            sed 's/^/  /' "$missing" >&2
            echo "       Extend analysis/build-env/stubs/immintrin_stubs.c and rebuild it" >&2
            echo "       with scripts/native/build-immintrin-stubs.sh, or the link will fail." >&2
            exit 4
        fi
        echo "all needed intrinsics are covered by the stub: OK"
    else
        echo "WARN: could not read one of the symbol lists; skipping coverage check" >&2
    fi
else
    echo "NOTE: NERV_IMMINTRIN_STUBS_O unset or missing; skipping _mm_* coverage check"
fi

note "install"
mkdir -p "$OUT_DIR/lib" "$OUT_DIR/include"
if [ -f "$OUT_DIR/lib/sodium.lib" ]; then
    bak="$OUT_DIR/lib/sodium.lib.bak-$(date -u +%Y%m%dT%H%M%SZ)"
    cp -p "$OUT_DIR/lib/sodium.lib" "$bak"
    echo "previous archive backed up: $bak"
fi
cp "$STAGE/sodium.lib" "$OUT_DIR/lib/sodium.lib"
# Headers the Rust stub may consume.  version.h comes from the overlay so the
# tree is self-contained even without autotools.
rm -rf "$OUT_DIR/include/sodium"
mkdir -p "$OUT_DIR/include"
cp -R "$SRC_INCLUDE/sodium" "$OUT_DIR/include/sodium"
cp "$OVERLAY/sodium/version.h" "$OUT_DIR/include/sodium/version.h"

echo
echo "DONE"
echo "  archive : $OUT_DIR/lib/sodium.lib ($(stat -c%s "$OUT_DIR/lib/sodium.lib") B, $got_members members)"
echo "  include : $OUT_DIR/include/sodium"
echo
echo "NEXT: this archive is now on disk but a stale rlib may still be cached."
echo "      To be sure the new archive is linked, delete ALL FOUR groups:"
echo "        target/x86_64-pc-windows-msvc/debug/deps/liblibsodium_sys-*"
echo "        target/x86_64-pc-windows-msvc/debug/.fingerprint/libsodium-sys-*"
echo "        target/x86_64-pc-windows-msvc/debug/{rustdesk.exe,librustdesk.dll,service.exe,naming.exe}"
echo "        target/x86_64-pc-windows-msvc/debug/deps/{rustdesk.exe,rustdesk.pdb,"
echo "            librustdesk.dll,librustdesk.dll.lib,librustdesk.pdb,"
echo "            service.exe,service.pdb,naming.exe,naming.pdb}"
echo "      The last group is required: deleting only the final artefacts leaves"
echo "      cargo hard-linking them back from deps/ (nlink stays 2)."
echo "      Verify with:"
echo "        llvm-nm-19 --undefined-only target/x86_64-pc-windows-msvc/debug/deps/liblibsodium_sys-*.rlib | grep '^__imp_'"
echo "      which must list only the 10 Windows APIs."
echo "      See scripts/native/README.md (rlib bundle trap)."
