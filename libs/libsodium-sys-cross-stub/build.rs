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

    let lib_dir = format!("{}/lib", dir.trim_end_matches('/'));
    println!("cargo:rustc-link-search=native={}", lib_dir);

    // Choose the link-lib name based on cargo's TARGET env (set by cargo when
    // running build scripts). This is the *target* triple, not host.
    let target = env::var("TARGET").unwrap_or_default();
    if target.ends_with("-windows-msvc") {
        // Our cross-built artifact is named sodium.lib for both MSVC and GNU.
        println!("cargo:rustc-link-lib=static=sodium");
    } else if target.contains("windows") {
        println!("cargo:rustc-link-lib=static=sodium");
    } else {
        // Linux/macOS host: use the same artifact filename.
        println!("cargo:rustc-link-lib=static=sodium");
    }
}