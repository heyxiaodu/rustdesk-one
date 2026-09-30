#!/bin/bash
# NERV Desk clang wrapper — forces clang-19 (apt 1:19.1.7) and injects
# --target=x86_64-pc-windows-msvc when cc-rs invokes us on a Linux host
# building for x86_64-pc-windows-msvc.
#
# Also rewrites -std=gnu++11 → -std=gnu++14 (needed for xwin STL constexpr
# expressions in webm-sys C++).
#
# Author: NERV Desk / Phase-2 windows-msvc cross-check.

set -e

# Always exec clang-19 (not whatever is on PATH — could be clang-14).
CLANG=${NERV_CLANG:-/usr/bin/clang-19}

# Compose the final argv.
ARGS=()

for arg in "$@"; do
    case "$arg" in
        -fPIC|-fpic|-fPIE|-fpie)
            # cc-rs injects these on Linux hosts; harmless on msvc target.
            ;;
        -fno-PIC|-fno-pic|-fno-PIE|-fno-pie)
            ;;
        -fstack-protector*|-fcf-protection*)
            # not supported by clang-cl on msvc target
            ;;
        -m32|-m64|-march=*|-mcpu=*|-mtune=*)
            # cc-rs may inject host arch flags; msvc target has its own.
            ;;
        -std=gnu++11)
            # webm-sys hardcodes this; gnu++11 breaks xwin yvals.
            ARGS+=("-std=gnu++14")
            ;;
        --target=*--target=*)
            # cc-rs sometimes appends --target=…twice (once from
            # --target flag and once from CARGO_CFG_TARGET_*); collapse.
            deduped="${arg#--target=}"
            deduped="${deduped#--target=}"
            ARGS+=("--target=${deduped}")
            ;;
        *)
            ARGS+=("$arg")
            ;;
    esac
done

# Inject --target=x86_64-pc-windows-msvc unless caller already set one.
has_target=0
for a in "${ARGS[@]}"; do
    case "$a" in
        --target=*) has_target=1 ;;
    esac
done

if [ "$has_target" -eq 0 ]; then
    arch="${CARGO_CFG_TARGET_ARCH:-x86_64}"
    vendor="${CARGO_CFG_TARGET_VENDOR:-pc}"
    os="${CARGO_CFG_TARGET_OS:-windows}"
    env2="${CARGO_CFG_TARGET_ENV:-msvc}"
    ARGS=("--target=${arch}-${vendor}-${os}-${env2}" "${ARGS[@]}")
fi

# Auto-inject xwin SDK include paths when targeting msvc. Both `NERV_XWIN` env
# (preferred) and a hard-coded default fallback are honored so the wrapper is
# usable for *both* cargo/cc-rs invocations (which set CARGO_CFG_TARGET_*) and
# manual cross-build invocations (which set NERV_XWIN). xwin provides the
# MSVC CRT + UCRT + Win32 API headers; without them clang's MSVC driver mode
# fails to find <stdlib.h> / <stddef.h> / etc. when target=*-windows-msvc.
target_is_msvc=0
for a in "${ARGS[@]}"; do
    case "$a" in
        --target=*-msvc) target_is_msvc=1 ;;
    esac
done

if [ "$target_is_msvc" -eq 1 ]; then
    NERV_XWIN_EFFECTIVE="${NERV_XWIN:-${HOME}/.xwin}"
    INC_ARGS=()
    for p in \
        "$NERV_XWIN_EFFECTIVE/crt/include" \
        "$NERV_XWIN_EFFECTIVE/sdk/include/um" \
        "$NERV_XWIN_EFFECTIVE/sdk/include/shared" \
        "$NERV_XWIN_EFFECTIVE/sdk/include/ucrt" \
        "$NERV_XWIN_EFFECTIVE/sdk/include/winrt"
    do
        if [ -d "$p" ]; then
            INC_ARGS+=("-I" "$p")
        fi
    done
    ARGS=("${INC_ARGS[@]}" "${ARGS[@]}")
fi

# MSVC-compat warnings to suppress: xwin CRT headers + RustDesk sources use
# patterns MSVC accepts but clang warns on. Treat these as warnings only.
ARGS+=(
    "-Wno-c++11-narrowing"
    "-Wno-deprecated-declarations"
    "-Wno-ignored-attributes"
    "-Wno-deprecated-anon-enum-enum-conversion"
    "-Wno-deprecated-enum-enum-conversion"
    "-Wno-deprecated-anon-enum-enum-conversion"
    "-Wno-unused-but-set-variable"
    "-Wno-int-conversion"
    "-Wno-pointer-to-int-cast"
    "-Wno-int-to-pointer-cast"
    "-D_WIN32_WINNT=0x0601"
    "-D_HAS_EXCEPTIONS=0"
    "-fno-exceptions"
    # MSVC-compat macro redefines: RustDesk + xwin headers reference the old
    # BSD-style stricmp/strnicmp which MSVC accepts but clang-cl treats as
    # undeclared. Forward to the underscore-prefixed MSVCRT names.
    "-Dstricmp=_stricmp"
    "-Dstrnicmp=_strnicmp"
    # mozjpeg-sys uses #ifdef HAVE_INTRIN_H to gate inclusion of <intrin.h>,
    # but its build-system sets HAVE_BITSCANFORWARD64 unconditionally so the
    # `#elif defined(HAVE_BITSCANFORWARD64)` branch uses _BitScanForward64
    # without including the declaring header. Mark the intrinsics as known
    # and include <intrin.h> on the msvc target so jcphuff.c compiles cleanly
    # in -O3 release mode (which is strict about implicit-function-declaration).
    #
    # Also define HAVE_BUILTIN_CTZL for clang so the __builtin_ctzl() branch
    # is preferred (it avoids needing the MSVC-specific intrinsics entirely).
    "-DHAVE_INTRIN_H"
    "-DHAVE_BITSCANFORWARD64"
    "-DHAVE_BITSCANFORWARD"
    "-DHAVE_BUILTIN_CTZL"
    # zstd-sys's build.rs defines ZSTDxxx_VISIBILITY with an empty value
    # (e.g. `config.define("ZSTDERRORLIB_VISIBILITY", Some(""))`). On the clang
    # command line, `-DZSTDERRORLIB_VISIBILITY=` becomes `#define
    # ZSTDERRORLIB_VISIBILITY 1`, and zstd_errors.h concatenates
    # `ZSTDERRORLIB_API ZSTDERRORLIB_VISIBLE → ZSTDERRORLIB_VISIBILITY`,
    # producing invalid syntax `1 ZSTD_ErrorCode ...`. Override the broken
    # empty-value defines with the proper GNU visibility attribute.
    "-DZSTDLIB_VISIBILITY=__attribute__((visibility(\"default\")))"
    "-DZSTDERRORLIB_VISIBILITY=__attribute__((visibility(\"default\")))"
    "-DZDICTLIB_VISIBILITY=__attribute__((visibility(\"default\")))"
)

exec "$CLANG" "${ARGS[@]}"
