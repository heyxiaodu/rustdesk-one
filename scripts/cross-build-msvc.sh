#!/usr/bin/env bash
# NERV Desk — Windows x86_64-msvc cross-build driver.
#
# Sources scripts/cross-msvc.env (which is what actually makes cc-rs pick the
# clang-19 wrapper instead of a bare host `clang`) and then runs cargo for the
# windows-msvc target. Any extra args are forwarded to cargo.
#
#   scripts/cross-build-msvc.sh --features quic
#   scripts/cross-build-msvc.sh --features quic --release
#   scripts/cross-build-msvc.sh --features quic -p zstd-sys
#
# NOTE: `.cargo/config.toml` `[target.<triple>]` keys are NOT exported to build
# scripts by cargo, so the env file is mandatory. See scripts/cross-msvc.env.
set -euo pipefail

NERV_REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export NERV_REPO_ROOT

export PATH="/usr/local/bin:${HOME}/.cargo/bin:${PATH}"

# shellcheck source=cross-msvc.env
set -a
. "${NERV_REPO_ROOT}/scripts/cross-msvc.env"
set +a

# The shim lives outside the repository and can be wiped by sandbox cleanup;
# fail loudly instead of silently producing ELF objects.
if [ ! -x "${NERV_SHIM}/clang-wrap.sh" ]; then
    echo "FATAL: ${NERV_SHIM}/clang-wrap.sh missing or not executable." >&2
    echo "       Set NERV_SHIM to its real location, or re-create the shim." >&2
    exit 1
fi
if [ ! -x "${NERV_SHIM}/link.sh" ]; then
    echo "FATAL: ${NERV_SHIM}/link.sh missing or not executable." >&2
    exit 1
fi

cd "${NERV_REPO_ROOT}"
exec cargo build --target=x86_64-pc-windows-msvc "$@"
