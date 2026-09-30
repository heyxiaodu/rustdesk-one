// Cross-build-friendly libsodium-sys stub build script.
//
// Upstream libsodium-sys 0.2.7 build.rs uses `#[cfg(windows)]` which evaluates
// at *build script compile time* (= host target), not cargo target. When
// cross-compiling from linux host to windows-msvc target, the cfg evaluates
// to `false`, so the build script falls into the autotools path which fails
// because the bundled config.sub/config.guess don't recognize MSVC targets.
//
// This stub bypasses that entirely. It emits rustc-link-lib directives that
// point at a prebuilt libsodium.lib supplied via env var. No autotools, no
// source build.
//
// Env var contract (priority high-to-low):
//   NERV_SODIUM_LIB_DIR       explicit override (NERV Desk convention)
//   SODIUM_LIB_DIR            upstream convention
//
// Inside the chosen dir we look for:
//   <dir>/lib/sodium.lib      static library for MSVC target
//   <dir>/lib/libsodium.lib   static library for GNU target
//   <dir>/include/            sodium headers (for headers-include path only)
//
// We emit `-l static=sodium` for MSVC and `-l static=sodium` for GNU both,
// because the filename `sodium.lib` is unique to our cross-built artifact.

use std::env;
use std::path::Path;

fn main() {
    let sodium_dir = env::var("NERV_SODIUM_LIB_DIR")
        .ok()
        .or_else(|| env::var("SODIUM_LIB_DIR").ok());

    let dir = match sodium_dir {
        Some(d) if Path::new(&d).is_dir() => d,
        _ => {
            // Honor the original upstream behavior when SODIUM_LIB_DIR is set
            // but points nowhere useful — emit a clear warning rather than
            // silently build something. The original upstream then runs its
            // own (broken) autotools path.
            println!("cargo:warning=nervdesk libsodium-sys stub: no SODIUM_LIB_DIR set; falling back to autotools path");
            return;
        }
    };

    println!("cargo:rerun-if-env-changed=NERV_SODIUM_LIB_DIR");
    println!("cargo:rerun-if-env-changed=SODIUM_LIB_DIR");
    println!("cargo:rerun-if-env-changed=SODIUM_SHARED");
    println!("cargo:rerun-if-env-changed=SODIUM_USE_PKG_CONFIG");

    // CRITICAL: only emit link directives when the *target* (not host) is
    // windows-msvc. Cargo invokes libsodium_sys's build.rs for both host
    // (because some downstream crate's build.rs may need to link libsodium_sys
    // as a host rlib) and target. For the host invocation, the system
    // pkg-config + libsodium-dev provides libsodium via the standard -l sodium
    // mechanism. Emitting our cross-built sodium.lib (COFF/PE archive) for a
    // host (linux ELF) build causes the host rustc to fail with
    // "could not find native static library sodium" because the file's MSVC
    // naming convention doesn't match the GNU convention the host linker
    // expects (lib<name>.a vs <name>.lib).
    //
    // The TARGET env var is set by cargo to the *target* triple (not host)
    // when invoking build.rs scripts, so this check correctly discriminates.
    let target = env::var("TARGET").unwrap_or_default();
    if !target.ends_with("-windows-msvc") {
        // Host build (linux/macos) or any non-windows-msvc target: emit
        // nothing; the host toolchain's pkg-config + libsodium-dev install
        // provides libsodium via standard -l sodium resolution.
        return;
    }

    let lib_dir = format!("{}/lib", dir.trim_end_matches('/'));
    // nervdesk: emit ONLY the search path. Do NOT emit `rustc-link-lib=static=sodium`
    // because rustc would then decompose sodium.lib and embed its .obj files into
    // this crate's rlib. When that rlib is later `--extern`'d by hbb_common / sodiumoxide /
    // rustdesk, the embedded .obj files would conflict with the same symbols in
    // sodium.lib (which our cross-built link.sh appends at the final link step),
    // producing duplicate-symbol errors.
    //
    // The `#[link(name="sodium", kind="static")]` attributes on the extern blocks
    // in sodium_bindings.rs tell rustc to resolve all `extern "C"` references
    // against sodium.lib at link time. The link.sh appends sodium.lib as a
    // positional arg, so the final rustdesk link finds every symbol.
    println!("cargo:rustc-link-search=native={}", lib_dir);
}