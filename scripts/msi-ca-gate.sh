#!/bin/bash
# msi-ca-gate.sh - local MSVC-equivalent syntax gate for the MSI custom-action DLL.
#
# WHY THIS EXISTS
#   CI run #6 (job "Build msi") died on code of ours with
#     CustomActions.cpp(542,12): error C2362: initialization of 'pDisplaySeparator'
#                                is skipped by 'goto LExit'
#     CustomActions.cpp(1339,10): error C2362: initialization of 'removed'
#                                is skipped by 'goto LExit'
#   MSVC only makes that an error under /permissive- (CustomActions.vcxproj sets
#   <ConformanceMode>true</ConformanceMode>). clang reports the same condition as
#   -Wmicrosoft-goto, plus a "note: jump bypasses variable initialization" naming the
#   very declaration MSVC objects to. Promoting that warning to an error reproduces the
#   MSVC verdict locally in about a second instead of a ~50 minute CI round trip.
#
# WHAT IT IS NOT
#   scripts/msi-ca-stub/*.h are minimal stand-ins for the WiX DUtil/WcaUtil headers,
#   which exist locally only after `nuget restore` (CI does that). They declare exactly
#   the symbols res/msi/CustomActions uses. This gate therefore proves *syntax and
#   control-flow validity*, not link/ABI correctness: the CI MSVC build stays
#   authoritative, and a green gate here does not replace a real Windows build.
#
# REQUIREMENTS
#   clang++-19 (same major version the cross build pins) and the xwin MSVC CRT+SDK.
#   Default xwin location $HOME/.xwin, override with NERV_XWIN (see scripts/cross-msvc.env).
#
# USAGE
#   ./scripts/msi-ca-gate.sh                 # check every translation unit of the project
#   ./scripts/msi-ca-gate.sh path/to/x.cpp   # check selected files
#   exit 0 = every TU clean, 1 = at least one TU failed
set -u

CLANG=${CLANG:-clang++-19}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
DIR="$REPO/res/msi/CustomActions"
STUB="${NERV_MSI_STUB:-$HERE/msi-ca-stub}"
X="${NERV_XWIN:-$HOME/.xwin}"

if ! command -v "$CLANG" >/dev/null 2>&1; then
  echo "msi-ca-gate: $CLANG not found (set CLANG=...)" >&2; exit 2
fi
if [ ! -d "$X/crt/include" ] && [ ! -d "$X/crt" ]; then
  echo "msi-ca-gate: xwin root '$X' not found (set NERV_XWIN=...)" >&2; exit 2
fi
if [ ! -d "$STUB" ]; then
  echo "msi-ca-gate: WiX stub headers '$STUB' not found" >&2; exit 2
fi

INC=(-isystem "$X/crt/include" -isystem "$X/sdk/include/ucrt" -isystem "$X/sdk/include/shared" -isystem "$X/sdk/include/um")
DEFS=(-DUNICODE -D_UNICODE -D_WINDOWS -D_USRDLL -D_WINDLL -DEXAMPLECADLL_EXPORTS)
FLAGS=(--target=x86_64-pc-windows-msvc -std=c++17 -fms-extensions -fms-compatibility
       -Werror=microsoft-goto -Wno-gnu-zero-variadic-macro-arguments
       -Wno-pointer-to-int-cast -Wno-int-to-pointer-cast)

if [ "$#" -gt 0 ]; then
  FILES=("$@")
else
  FILES=("$DIR"/*.cpp)
fi

rc_all=0
for f in "${FILES[@]}"; do
  out=$("$CLANG" "${FLAGS[@]}" "${DEFS[@]}" "${INC[@]}" -I "$DIR" -I "$STUB" -fsyntax-only "$f" 2>&1)
  rc=$?
  n=$(printf '%s\n' "$out" | grep -c ': error:')
  b=$(printf '%s\n' "$out" | grep -c 'jump bypasses variable initialization')
  printf '%-24s rc=%-3s errors=%-3s bypass-init-sites=%s\n' "$(basename "$f")" "$rc" "$n" "$b"
  if [ "$rc" -ne 0 ]; then
    rc_all=1
    printf '%s\n' "$out" | grep ': error:' | head -10 | sed 's/^/    /'
    printf '%s\n' "$out" | grep 'jump bypasses variable initialization' \
      | grep -o 'CustomActions\.cpp:[0-9]*:[0-9]*: note: jump bypasses' | sort -u | sed 's/^/    => /'
  fi
done
exit $rc_all
