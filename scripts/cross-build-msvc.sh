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

# libsodium-sys is replaced for cross builds only (see Cargo.toml and
# scripts/cross-msvc-patch.toml); the relative path inside that config is
# resolved against the working directory, so cd first.
NERV_SODIUM_PATCH="${NERV_REPO_ROOT}/scripts/cross-msvc-patch.toml"
if [ ! -f "${NERV_SODIUM_PATCH}" ]; then
    echo "FATAL: ${NERV_SODIUM_PATCH} missing; cannot cross-build libsodium-sys." >&2
    exit 1
fi

cd "${NERV_REPO_ROOT}"

# Patching libsodium-sys changes its source, so cargo rewrites Cargo.lock between
# its registry form (upstream crate, with a checksum) and a path form (our stub,
# no source). The committed lock must stay in registry form, otherwise the native
# and non-Windows CI jobs fail with `--locked`. Restore it on every exit path -
# hence no `exec` for the cargo call below.
NERV_LOCK_BAK="$(mktemp)"
cp Cargo.lock "${NERV_LOCK_BAK}"
restore_lock() {
    cp "${NERV_LOCK_BAK}" Cargo.lock
    rm -f "${NERV_LOCK_BAK}"
}
trap restore_lock EXIT

cargo build --target=x86_64-pc-windows-msvc --config "${NERV_SODIUM_PATCH}" "$@"
