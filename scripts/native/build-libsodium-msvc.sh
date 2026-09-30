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

# Recorded archive sizes in bytes.  Informational only -- do NOT turn these into
# assertions.  Seven of the 106 members embed an absolute source path in
# `.rdata` (see scripts/native/README.md, "Fidelity to the shipped archive"),
# so the byte size legitimately depends on where the source tree lives.  Both
# numbers are recorded so that a mismatch is visible rather than surprising:
#   BASELINE_SHIPPED_SIZE -- the archive the verified PE artefacts were linked
#                            against, currently installed in analysis/build-env
#   BASELINE_REBUILT_SIZE -- the same archive rebuilt from the crate tree
BASELINE_SHIPPED_SIZE=2283100
BASELINE_REBUILT_SIZE=2282638

# The exact archive member-name set of the shipped archive (sorted, LC_ALL=C).
#
# Member names are the source path relative to src/libsodium with '/' replaced
# by '_' -- a function of the *source tree layout* and the naming rule, and of
# nothing else.  That makes this list machine-independent, unlike the archive
# bytes: it is the one output-side invariant that can be asserted hard.
#
# It is pinned as an explicit list rather than only as a hash on purpose -- a
# bare hash is short but does not tell a reviewer what it pins.  The digest of
# this list is recorded alongside it for convenience, and the archive's own
# sha256 is printed (never asserted) for human comparison.
#
# Regenerate with:  llvm-ar-19 t <archive> | tr -d '\r' | LC_ALL=C sort
EXPECTED_MEMBER_SET_SHA256="2f810ad2262eddd1b06b5b3d7ff2a9b4dee8488070bf0545a6ef169497e782bd"
EXPECTED_MEMBER_SET='crypto_aead_aes256gcm_aesni_aead_aes256gcm_aesni.obj
crypto_aead_chacha20poly1305_sodium_aead_chacha20poly1305.obj
crypto_aead_xchacha20poly1305_sodium_aead_xchacha20poly1305.obj
crypto_auth_crypto_auth.obj
crypto_auth_hmacsha256_auth_hmacsha256.obj
crypto_auth_hmacsha512256_auth_hmacsha512256.obj
crypto_auth_hmacsha512_auth_hmacsha512.obj
crypto_box_crypto_box.obj
crypto_box_crypto_box_easy.obj
crypto_box_crypto_box_seal.obj
crypto_box_curve25519xchacha20poly1305_box_curve25519xchacha20poly1305.obj
crypto_box_curve25519xchacha20poly1305_box_seal_curve25519xchacha20poly1305.obj
crypto_box_curve25519xsalsa20poly1305_box_curve25519xsalsa20poly1305.obj
crypto_core_ed25519_core_ed25519.obj
crypto_core_ed25519_core_ristretto255.obj
crypto_core_ed25519_ref10_ed25519_ref10.obj
crypto_core_hchacha20_core_hchacha20.obj
crypto_core_hsalsa20_core_hsalsa20.obj
crypto_core_hsalsa20_ref2_core_hsalsa20_ref2.obj
crypto_core_salsa_ref_core_salsa_ref.obj
crypto_generichash_blake2b_generichash_blake2.obj
crypto_generichash_blake2b_ref_blake2b-compress-avx2.obj
crypto_generichash_blake2b_ref_blake2b-compress-ref.obj
crypto_generichash_blake2b_ref_blake2b-compress-sse41.obj
crypto_generichash_blake2b_ref_blake2b-compress-ssse3.obj
crypto_generichash_blake2b_ref_blake2b-ref.obj
crypto_generichash_blake2b_ref_generichash_blake2b.obj
crypto_generichash_crypto_generichash.obj
crypto_hash_crypto_hash.obj
crypto_hash_sha256_cp_hash_sha256_cp.obj
crypto_hash_sha256_hash_sha256.obj
crypto_hash_sha512_cp_hash_sha512_cp.obj
crypto_hash_sha512_hash_sha512.obj
crypto_kdf_blake2b_kdf_blake2b.obj
crypto_kdf_crypto_kdf.obj
crypto_kx_crypto_kx.obj
crypto_onetimeauth_crypto_onetimeauth.obj
crypto_onetimeauth_poly1305_donna_poly1305_donna.obj
crypto_onetimeauth_poly1305_onetimeauth_poly1305.obj
crypto_onetimeauth_poly1305_sse2_poly1305_sse2.obj
crypto_pwhash_argon2_argon2-core.obj
crypto_pwhash_argon2_argon2-encoding.obj
crypto_pwhash_argon2_argon2-fill-block-avx2.obj
crypto_pwhash_argon2_argon2-fill-block-avx512f.obj
crypto_pwhash_argon2_argon2-fill-block-ref.obj
crypto_pwhash_argon2_argon2-fill-block-ssse3.obj
crypto_pwhash_argon2_argon2.obj
crypto_pwhash_argon2_blake2b-long.obj
crypto_pwhash_argon2_pwhash_argon2i.obj
crypto_pwhash_argon2_pwhash_argon2id.obj
crypto_pwhash_crypto_pwhash.obj
crypto_pwhash_scryptsalsa208sha256_crypto_scrypt-common.obj
crypto_pwhash_scryptsalsa208sha256_nosse_pwhash_scryptsalsa208sha256_nosse.obj
crypto_pwhash_scryptsalsa208sha256_pbkdf2-sha256.obj
crypto_pwhash_scryptsalsa208sha256_pwhash_scryptsalsa208sha256.obj
crypto_pwhash_scryptsalsa208sha256_scrypt_platform.obj
crypto_pwhash_scryptsalsa208sha256_sse_pwhash_scryptsalsa208sha256_sse.obj
crypto_scalarmult_crypto_scalarmult.obj
crypto_scalarmult_curve25519_ref10_x25519_ref10.obj
crypto_scalarmult_curve25519_sandy2x_curve25519_sandy2x.obj
crypto_scalarmult_curve25519_sandy2x_fe51_invert.obj
crypto_scalarmult_curve25519_sandy2x_fe_frombytes_sandy2x.obj
crypto_scalarmult_curve25519_scalarmult_curve25519.obj
crypto_scalarmult_ed25519_ref10_scalarmult_ed25519_ref10.obj
crypto_scalarmult_ristretto255_ref10_scalarmult_ristretto255_ref10.obj
crypto_secretbox_crypto_secretbox.obj
crypto_secretbox_crypto_secretbox_easy.obj
crypto_secretbox_xchacha20poly1305_secretbox_xchacha20poly1305.obj
crypto_secretbox_xsalsa20poly1305_secretbox_xsalsa20poly1305.obj
crypto_secretstream_xchacha20poly1305_secretstream_xchacha20poly1305.obj
crypto_shorthash_crypto_shorthash.obj
crypto_shorthash_siphash24_ref_shorthash_siphash24_ref.obj
crypto_shorthash_siphash24_ref_shorthash_siphashx24_ref.obj
crypto_shorthash_siphash24_shorthash_siphash24.obj
crypto_shorthash_siphash24_shorthash_siphashx24.obj
crypto_sign_crypto_sign.obj
crypto_sign_ed25519_ref10_keypair.obj
crypto_sign_ed25519_ref10_obsolete.obj
crypto_sign_ed25519_ref10_open.obj
crypto_sign_ed25519_ref10_sign.obj
crypto_sign_ed25519_sign_ed25519.obj
crypto_stream_chacha20_dolbeau_chacha20_dolbeau-avx2.obj
crypto_stream_chacha20_dolbeau_chacha20_dolbeau-ssse3.obj
crypto_stream_chacha20_ref_chacha20_ref.obj
crypto_stream_chacha20_stream_chacha20.obj
crypto_stream_crypto_stream.obj
crypto_stream_salsa2012_ref_stream_salsa2012_ref.obj
crypto_stream_salsa2012_stream_salsa2012.obj
crypto_stream_salsa208_ref_stream_salsa208_ref.obj
crypto_stream_salsa208_stream_salsa208.obj
crypto_stream_salsa20_ref_salsa20_ref.obj
crypto_stream_salsa20_stream_salsa20.obj
crypto_stream_salsa20_xmm6_salsa20_xmm6.obj
crypto_stream_salsa20_xmm6int_salsa20_xmm6int-avx2.obj
crypto_stream_salsa20_xmm6int_salsa20_xmm6int-sse2.obj
crypto_stream_xchacha20_stream_xchacha20.obj
crypto_stream_xsalsa20_stream_xsalsa20.obj
crypto_verify_sodium_verify.obj
randombytes_internal_randombytes_internal_random.obj
randombytes_randombytes.obj
randombytes_sysrandom_randombytes_sysrandom.obj
sodium_codecs.obj
sodium_core.obj
sodium_runtime.obj
sodium_utils.obj
sodium_version.obj'

JOBS="$(nproc 2>/dev/null || echo 4)"
OUT_DIR="${NERV_SODIUM_LIB_DIR:-$NERV_BE/analysis/build-env/nervdesk-sodium-msvc}"
KEEP_SRC=0

# Declared before the argument loop: the loop validates its own operands, so it
# needs these already defined.
fail() {
    echo "ERROR: $*" >&2
    exit "${2:-3}"
}
note() { printf '\n=== %s ===\n' "$*"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --jobs)
            [ $# -ge 2 ] || fail "--jobs requires a value" 1
            JOBS="$2"
            shift 2
            ;;
        --out)
            [ $# -ge 2 ] || fail "--out requires a value" 1
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

# --- validate --out / --jobs -------------------------------------------------
# `--out` was previously taken verbatim.  `--out ""` then collapsed every
# install target onto the filesystem root: `$OUT_DIR/lib`, `$OUT_DIR/include` and
# the recursive `rm -rf "$OUT_DIR/include/sodium"` in the install section would
# all have resolved to /lib, /include and /include/sodium.  This build runs as
# root, so that was a single-argument way to damage the host.  Nothing is
# written until these checks pass.
#
# The path must resolve (after symlinks) into one of the two trees this project
# owns -- the checkout itself or the analysis/ directory beside it -- and must
# not be either root.  There is deliberately NO environment override here: this
# is the only place in the project where a single argument could damage the
# host, and an escape hatch would simply reintroduce what the check exists to
# stop.  Same principle as the EXPECTED_* constants below -- a gate the guarded
# party can switch off is not a gate.
validate_out_dir() { # <dir> -> prints the resolved directory
    local d="$1" real root_real be_real
    [ -n "$d" ] || fail "--out must not be empty (it would collapse every install target onto /)" 1
    real="$(realpath -m -- "$d" 2>/dev/null)" || fail "cannot resolve --out path: $d" 1
    [ "$real" != "/" ] || fail "--out must not be the filesystem root" 1
    root_real="$(realpath -m -- "$NERV_REPO_ROOT" 2>/dev/null)" \
        || fail "cannot resolve NERV_REPO_ROOT=$NERV_REPO_ROOT" 1
    be_real="$(realpath -m -- "$NERV_BE" 2>/dev/null)" \
        || fail "cannot resolve NERV_BE=$NERV_BE" 1
    case "$real" in
        "$root_real" | "$be_real")
            fail "--out must not be the project root itself: $real" 1
            ;;
        "$root_real"/* | "$be_real"/*) ;;
        *)
            echo "ERROR: --out must be inside $be_real" >&2
            echo "       got: $real" >&2
            echo "       (if you need a scratch copy, use a directory inside $be_real;" >&2
            echo "        there is no override for this check by design)" >&2
            exit 1
            ;;
    esac
    printf '%s\n' "$real"
}
OUT_DIR="$(validate_out_dir "$OUT_DIR")"

case "$JOBS" in
    '' | *[!0-9]*) fail "--jobs must be a positive integer (got '$JOBS')" 1 ;;
esac
[ "$JOBS" -ge 1 ] || fail "--jobs must be >= 1 (got $JOBS)" 1

CLANG_WRAP="${NERV_SHIM:-$NERV_SHIM}/clang-wrap.sh"
TARGET_TRIPLE="x86_64-pc-windows-msvc"
LLVM_AR="${NERV_LLVM_AR:-/usr/bin/llvm-ar-19}"
LLVM_NM="${NERV_LLVM_NM:-/usr/bin/llvm-nm-19}"
LLVM_OBJDUMP="${NERV_LLVM_OBJDUMP:-/usr/bin/llvm-objdump-19}"

# sha256 helpers.  NOT hard prerequisites: the tool is used only to record a
# digest for human comparison, and degrading to "<unavailable>" must never fail
# a build that is otherwise sound.
sha256_of() { # <file> -> hex digest, or "<unavailable>"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    else
        printf '<unavailable>\n'
    fi
}

note "prerequisites"
[ -f "$CLANG_WRAP" ] || fail "missing required file: $CLANG_WRAP" 1
# `curl` and `sha256sum` were required here and never used: the download /
# checksum path they guarded was removed (EXIT CODES still reserves 2 for it).
# Requiring a tool that is never called only makes the script harder to run than
# it needs to be, so both are gone from this list -- curl for good, and sha256sum
# because the fingerprint below treats it as optional with an openssl fallback.
for t in "$LLVM_AR" "$LLVM_NM" "$LLVM_OBJDUMP" tar find xargs; do
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
        # Quote the variable expansions, leave the glob metacharacter bare: an
        # unquoted $base word-splits on spaces (and would break if the registry
        # path ever contained one), while the `*` must stay a glob.
        for root in "$base"/*/libsodium-sys-"$SODIUM_SYS_VERSION"; do
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

# ---------------------------------------------------------------------------
# Artefact fingerprint.  Everything above this point pins the *input* (the
# source tree, by three heuristics).  Nothing pinned the *output*, so a wrong
# but plausible source tree could still pass every gate and install a different
# archive unnoticed.  The member-name set closes that: it is derived purely from
# the source tree layout and the member-naming rule, so it is identical on every
# build machine, and a hard assertion on it is therefore legitimate.
#
# The archive sha256 and byte size are printed but NOT asserted -- see the note
# on BASELINE_*_SIZE at the top: 7 of 106 members embed an absolute __FILE__
# path, which makes the bytes a function of where the tree lives.
note "artefact fingerprint"
"$LLVM_AR" t "$STAGE/sodium.lib" 2>/dev/null | tr -d '\r' | LC_ALL=C sort >"$WORK/member_names.txt"
printf '%s\n' "$EXPECTED_MEMBER_SET" | LC_ALL=C sort >"$WORK/member_set_expected.txt"
if ! diff -u "$WORK/member_set_expected.txt" "$WORK/member_names.txt" >"$WORK/member_set.diff" 2>&1; then
    echo "ERROR: archive member names do not match the pinned baseline set." >&2
    echo "       The set depends only on the source tree layout and the naming" >&2
    echo "       rule, so a difference means one of those changed -- or that a" >&2
    echo "       stale archive is being fingerprinted.  Refusing to install." >&2
    sed -n '1,40p' "$WORK/member_set.diff" | sed 's/^/  /' >&2
    exit 4
fi
member_set_hash=$(sha256_of "$WORK/member_names.txt")
archive_hash=$(sha256_of "$STAGE/sodium.lib")
archive_size=$(stat -c%s "$STAGE/sodium.lib")
echo "member-name set: $got_members names, identical to the pinned baseline (sha256 $member_set_hash)"
echo "archive sha256 : $archive_hash"
echo "archive size   : $archive_size B"
echo "  recorded baselines: shipped $BASELINE_SHIPPED_SIZE B / rebuilt-elsewhere $BASELINE_REBUILT_SIZE B"
echo "  (size varies with the source tree path; only the member set is asserted)"
if [ "$member_set_hash" = "<unavailable>" ]; then
    # The member-set *diff* gate at the top of this section is unconditional and
    # fail-closed: it never consults a sha256 tool, so it still holds on a
    # machine with neither sha256sum nor openssl.  The digest comparison below
    # is a second, redundant gate that catches EXPECTED_MEMBER_SET and
    # EXPECTED_MEMBER_SET_SHA256 drifting apart.  When no hashing tool exists it
    # provably cannot run -- so it says so, loudly, rather than passing silently
    # or (worse) reporting a false "out of sync" root cause.
    echo "WARN: no sha256 tool available (neither sha256sum nor openssl); the" >&2
    echo "WARN:   EXPECTED_MEMBER_SET_SHA256 cross-check was SKIPPED (<unavailable>)." >&2
    echo "WARN:   The member-name-set diff gate above DID run and passed; it is the" >&2
    echo "WARN:   authoritative check on the member set.  Only the redundant" >&2
    echo "WARN:   recorded-digest drift check is missing on this machine." >&2
elif [ "$member_set_hash" != "$EXPECTED_MEMBER_SET_SHA256" ]; then
    # Unreachable while the diff above passes, but a mismatch here means the
    # recorded digest and the recorded list have drifted apart.  Say so.
    echo "ERROR: member-set sha256 $member_set_hash != recorded $EXPECTED_MEMBER_SET_SHA256;" >&2
    echo "       EXPECTED_MEMBER_SET and EXPECTED_MEMBER_SET_SHA256 are out of sync." >&2
    exit 4
fi
if [ "$archive_hash" = "<unavailable>" ]; then
    echo "WARN: neither sha256sum nor openssl is available; archive digest not recorded (<unavailable>)" >&2
fi

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
echo "  sha256  : $archive_hash"
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
