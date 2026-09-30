#!/bin/bash
# NERV Desk MSVC linker wrapper — translates rustc-msvc linker argv into
# lld-link-19 argv. Rustc already passes most MSVC-style flags (when it
# detects our wrapper as MSVC-flavored). We:
#   - drop rustc-only flags (-C*, -L dependency=..., --extern NAME=PATH, .so)
#   - drop host-platform artifacts (Linux ELF .so leaked through proc-macros)
#   - expand brace-glob patterns for libstd/libcore/liballoc/*.rlib
#   - add /MACHINE:X64 (lld-link-19 default is x86 but explicit is safer)
#   - resolve lib names via case-insensitive search in NERV_XWIN lib dirs
#   - forward MSVC-style flags (/OUT:, /DEF:, /DLL, /IMPLIB:, /DEBUG, ...)
#
# Author: NERV Desk / Phase-2 windows-msvc cross-check.

set -e

LLD_LINK=${NERV_LLD_LINK:-lld-link-19}

# Pre-populate SEARCH_DIRS with the xwin SDK lib paths so resolve_lib() can
# find libs even when rustc does NOT pass any -L native=... (e.g. /defaultlib:
# from the cmdline, which is processed in the main loop, hits resolve_lib
# before /LIBPATH: directives are appended).
# nervdesk: bake the default xwin path in so link.sh works even when the
# calling shell didn't export NERV_XWIN (cargo's per-target env inheritance
# is unreliable across the rustc subprocess boundary).
DEFAULT_XWIN="${HOME}/.xwin"
NERV_XWIN="${NERV_XWIN:-$DEFAULT_XWIN}"
SEARCH_DIRS="${NERV_XWIN}/sdk/lib/um/x86_64|${NERV_XWIN}/sdk/lib/ucrt/x86_64|${NERV_XWIN}/crt/lib/x86_64"

# Capture stderr to NERV_LINK_LOG if set.
if [ -n "$NERV_LINK_LOG" ]; then
    exec 2>>"$NERV_LINK_LOG"
fi

{
    echo "=== link.sh pid=$$ argv_count=$# at $(date -Iseconds 2>/dev/null || date) ==="
} >>"$NERV_LINK_LOG" 2>/dev/null || true

# Debug: log the final command if NERV_LINK_VERBOSE set. Must run AFTER CMD
# is populated; see end of script for the actual log emit.
if [ -n "$NERV_LINK_VERBOSE" ]; then
    NERV_LINK_VERBOSE_LOG="$NERV_LINK_LOG"
fi

# We compose the final command as an array (bash preserves quoting reliably).
CMD=("$LLD_LINK")

# Track what we've seen.
HAS_MACHINE=0
HAS_SUBSYSTEM=0
HAS_OUT=0
HAS_DLL=0

# resolve_lib NAME → find NAME.lib (case-insensitive) in NERV_XWIN lib dirs.
resolve_lib() {
    local name="$1"
    local base="${name%.lib}"
    [ "$base" = "$name" ] && base="$name"   # not a .lib → just search
    for d in $(echo "$SEARCH_DIRS" | tr '|' ' '); do
        [ -z "$d" ] && continue
        [ ! -d "$d" ] && continue
        # Try exact match first
        if [ -e "$d/$name" ]; then
            echo "$d/$name"
            return 0
        fi
        # Case-insensitive find
        local hit
        hit=$(find "$d" -maxdepth 1 -iname "$name" 2>/dev/null | head -1)
        if [ -n "$hit" ]; then
            echo "$hit"
            return 0
        fi
    done
    echo "$name"   # best-effort
    return 0
}

i=1
n=$#
while [ "$i" -le "$n" ]; do
    arg="${!i}"
    case "$arg" in
        # Skip pure rustc-native flags lld-link doesn't accept.
        --help|--version|-V|-v|--color=*|--message-format=*|--error-format=*)
            ;;
        # -C VALUE: rustc compiler flag pair; consume next arg too.
        -C)
            i=$((i + 1))
            ;;
        -C*)
            ;;
        # -L KEY=DIR: rustc search-path; extract DIR for /LIBPATH:.
        -L)
            i=$((i + 1))
            next="${!i}"
            SEARCH_DIRS="${SEARCH_DIRS:+$SEARCH_DIRS|}$next"
            ;;
        -L*)
            next="${arg#-L}"
            SEARCH_DIRS="${SEARCH_DIRS:+$SEARCH_DIRS|}$next"
            ;;
        # /LIBPATH:DIR: already MSVC-style; record search dir.
        /LIBPATH:*)
            next="${arg#/LIBPATH:}"
            SEARCH_DIRS="${SEARCH_DIRS:+$SEARCH_DIRS|}$next"
            CMD+=("$arg")
            ;;
        # -l NAME: consume next arg too.
        -l)
            i=$((i + 1))
            next="${!i}"
            CMD+=("/DEFAULTLIB:$next")
            ;;
        -l*)
            CMD+=("/DEFAULTLIB:${arg#-l}")
            ;;
        # --extern NAME=PATH: lld-link doesn't recognize. PATH is a real input
        # file. Drop the --extern prefix, emit PATH as positional.
        # If PATH ends in .so (Linux ELF proc-macro artifact), drop entirely.
        --extern)
            i=$((i + 1))
            next="${!i}"
            real="${next#*=}"
            case "$real" in
                *.so) ;;
                *) CMD+=("$real") ;;
            esac
            ;;
        --extern=*)
            real="${arg#--extern=}"
            real="${real#*=}"
            case "$real" in
                *.so) ;;
                *) CMD+=("$real") ;;
            esac
            ;;
        # Glob patterns: expand and emit matching files.
        *'*'*|*'?'*|*'['*)
            pat="${arg#\"}"
            pat="${pat%\"}"
            if [ -n "$pat" ]; then
                # shellcheck disable=SC2046
                set -- $(eval "echo $pat" 2>/dev/null) || true
                for tok in "$@"; do
                    if [ -e "$tok" ]; then
                        CMD+=("$tok")
                    fi
                done
            fi
            ;;
        # .so positional: HOST (Linux ELF) artifact leaked into target link —
        # drop silently.
        *.so)
            ;;
        # /OUT:, /DEF:, /IMPLIB:, /DEBUG*, /DLL, /PDB*, /SUBSYSTEM:*: pass through
        # unchanged. These are MSVC-style flags rustc emits.
        /OUT:*|/DEF:*|/IMPLIB:*|/DLL|/NOLOGO|/DEBUG|/DEBUG:*|/PDB*|/SUBSYSTEM:*|/OPT:*)
            case "$arg" in
                /OUT:*) HAS_OUT=1 ;;
                /SUBSYSTEM:*) HAS_SUBSYSTEM=1 ;;
                /DLL) HAS_DLL=1 ;;
            esac
            CMD+=("$arg")
            ;;
        # /MACHINE:*: pass through but track.
        /MACHINE:*)
            HAS_MACHINE=1
            CMD+=("$arg")
            ;;
        # /defaultlib:*: resolve the lib name via SEARCH_DIRS so case-sensitive
        # ext4 lookup misses (e.g. libcmt.lib vs LIBCMT.lib) don't break link.
        /defaultlib:*)
            libname="${arg#/defaultlib:}.lib"
            resolved=$(resolve_lib "$libname")
            CMD+=("/defaultlib:${resolved##*/}")
            ;;
        # /NXCOMPAT, /GUARD:*, /SAFESEH, /LARGEADDRESSAWARE, /DYNAMICBASE: pass through.
        /NXCOMPAT|/GUARD:*|/SAFESEH|/LARGEADDRESSAWARE|/DYNAMICBASE)
            CMD+=("$arg")
            ;;
        # *.lib positional: case-insensitive resolve via SEARCH_DIRS.
        *.lib)
            resolved=$(resolve_lib "$arg")
            CMD+=("$resolved")
            ;;
        # *.a positional: same.
        *.a)
            resolved=$(resolve_lib "$arg")
            CMD+=("$resolved")
            ;;
        # Everything else: pass through as a positional arg.
        *)
            CMD+=("$arg")
            ;;
    esac
    i=$((i + 1))
done

# nervdesk: reproducible-build timestamps. /Brepro makes lld-link write a hash
# of the executable into the COFF TimeDateStamp field instead of the wall-clock
# time, so two consecutive links of identical inputs are byte-identical. This is
# a general flag: it is appended unconditionally here next to the other always-on
# flags (there is no A/B switch, no build-profile branch, and no env gate around
# it) because round-5's acceptance criterion is exactly "two full builds produce
# the same sha256 for all four artifacts".
CMD+=("/Brepro")

# If /MACHINE: was not passed by rustc, default to X64.
if [ "$HAS_MACHINE" -eq 0 ]; then
    CMD+=("/MACHINE:X64")
fi

# Append default Windows SDK lib search dirs so kernel32 / advapi32 / etc.
# resolve via case-insensitive lookup. These are appended LAST so they have
# lower priority than rustc's explicit -L native=... paths.
for d in \
    "$NERV_XWIN/sdk/lib/um/x86_64" \
    "$NERV_XWIN/sdk/lib/ucrt/x86_64" \
    "$NERV_XWIN/crt/lib/x86_64" \
    "$NERV_XWIN/sdk/lib/shared/x86_64"; do
    [ -d "$d" ] && CMD+=("/LIBPATH:$d")
done

# nervdesk: ensure native cross-built static libs are passed as positional
# args to lld-link. Cargo's rustc-link-lib=static=vpx produces only /LIBPATH:
# entries; lld-link will NOT load archives unless they're given as positional
# .lib files. Each directory comes from the environment (scripts/cross-msvc.env
# exports the NERV_*_LIB_DIR variables), with a workspace-relative fallback so
# the shim still works when cargo is invoked without that env file.
NERV_BE_EFFECTIVE="${NERV_BE:-$(cd "$(dirname "$0")/../../../.." 2>/dev/null && pwd || printf '%s' "$PWD")}"
for libdir in \
    "${NERV_LIBVPX_DIR:-$NERV_BE_EFFECTIVE/analysis/build-env/nervdesk-vpx-msvc/lib}" \
    "${NERV_LIBAOM_DIR:-$NERV_BE_EFFECTIVE/analysis/build-env/nervdesk-aom-msvc/lib}" \
    "${NERV_LIBYUV_DIR:-$NERV_BE_EFFECTIVE/analysis/build-env/nervdesk-yuv-msvc/lib}" \
    "${NERV_OPUS_LIB_DIR:-$NERV_BE_EFFECTIVE/analysis/build-env/nervdesk-opus-msvc/lib}" \
    "${NERV_SODIUM_LIB_DIR:-$NERV_BE_EFFECTIVE/analysis/build-env/nervdesk-sodium-msvc}/lib" \
    "${NERV_MACHINEUID_LIB_DIR:-$NERV_BE_EFFECTIVE/analysis/build-env/nervdesk-machineuid-msvc/lib}"; do
    [ -n "$libdir" ] || continue
    if [ -d "$libdir" ]; then
        # Append each .lib file as positional so lld-link actually loads them.
        for libfile in "$libdir"/*.lib; do
            [ -e "$libfile" ] && CMD+=("$libfile")
        done
    fi
done

# nervdesk: append immintrin_stubs.o as a STANDALONE positional arg (not via
# sodium.lib). Reason: when libsodium_sys's build.rs emits
# rustc-link-lib=static=sodium (or similar) AND its #[link(name="sodium",
# kind="static")] attrs trigger rustc to decompose sodium.lib and embed the
# .obj files into liblibsodium_sys.rlib. If immintrin_stubs.o is inside
# sodium.lib, it ALSO gets embedded into liblibsodium_sys.rlib. When the
# consumer (rustdesk) --extern's that rlib AND we also append sodium.lib as
# positional, the same _mm_* symbols appear in both — duplicate-symbol
# errors.
#
# By keeping immintrin_stubs.o OUT of sodium.lib and appending it directly
# here as a .o file (not .lib), lld-link pulls in only the symbols it needs
# from this object without re-decomposing another archive.
IMMINTRIN_STUBS_O="${NERV_IMMINTRIN_STUBS_O:-$NERV_BE_EFFECTIVE/analysis/build-env/stubs/obj/immintrin_stubs.o}"
if [ -f "$IMMINTRIN_STUBS_O" ]; then
    CMD+=("$IMMINTRIN_STUBS_O")
fi

# Emit final CMD to log (must run AFTER all CMD+= calls above).
if [ -n "$NERV_LINK_VERBOSE_LOG" ]; then
    {
        echo "--- NERV_LINK_VERBOSE: CMD=${CMD[*]} ---"
    } >>"$NERV_LINK_VERBOSE_LOG" 2>/dev/null || true
fi

exec "${CMD[@]}"
