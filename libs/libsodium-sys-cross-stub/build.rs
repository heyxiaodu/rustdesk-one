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
    // nervdesk (m09900): dispatch on TARGET **before** the SODIUM_LIB_DIR
    // lookup below.
    //
    // NERV_SODIUM_LIB_DIR is only exported for the cross build
    // (scripts/cross-msvc.env). On a plain host `cargo build` it is unset, so
    // the lookup takes its `_ =>` arm and returns early — which skipped the
    // host link emission entirely and left every `sodium_*` / `crypto_*` symbol
    // undefined at host link time:
    //   rust-lld: error: undefined symbol: sodium_memzero
    //   rust-lld: error: undefined symbol: crypto_sign_ed25519_open
    // `cargo check` never caught this because check does not link.
    //
    // Why we must emit it ourselves: the `#[link(name = "sodium", kind = "static")]`
    // attributes in sodium_bindings.rs are gated
    // `#[cfg_attr(target_env = "msvc", ...)]` (see m08504), so on a non-msvc
    // target no link attribute remains anywhere. Upstream libsodium-sys
    // resolves libsodium on unix via pkg-config / the system package;
    // libsodium-dev provides /usr/lib/<triple>/libsodium.so and the .pc file.
    //
    // `TARGET` is the triple cargo is *building for* (not the host triple), so
    // this check correctly discriminates host builds from the msvc cross build.
    let target = env::var("TARGET").unwrap_or_default();
    if !target.ends_with("-windows-msvc") {
        if target.contains("linux") || target.contains("darwin") || target.contains("freebsd") {
            println!("cargo:rustc-link-lib=sodium");
        }
        return;
    }

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

    // From here on, target is always *-windows-msvc: the non-msvc case returned
    // above. Emitting the cross-built sodium.lib (COFF/PE archive) for a host
    // (linux ELF) build would fail with "could not find native static library
    // sodium", because the file's MSVC naming convention does not match the GNU
    // convention the host linker expects (lib<name>.a vs <name>.lib).
    let lib_dir = format!("{}/lib", dir.trim_end_matches('/'));

    // nervdesk: declare the archive itself as an input.
    //
    // The extern blocks in sodium_bindings.rs carry
    // `#[link(name = "sodium", kind = "static")]`, so rustc *bundles*
    // sodium.lib into liblibsodium_sys-*.rlib. Cargo's fingerprint does not
    // watch that external file, so replacing the archive on disk used to leave
    // the stale rlib in place: the next build reported "Finished dev profile in
    // ~15s" and the final binaries were hard-linked back out of deps/
    // unchanged, silently discarding the new archive.
    //
    // Naming it as an input makes cargo re-run this script and rebuild the
    // crate, which re-bundles the current archive.
    let lib_file = format!("{}/sodium.lib", lib_dir);
    if Path::new(&lib_file).is_file() {
        println!("cargo:rerun-if-changed={}", lib_file);
    }
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