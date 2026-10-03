// Cross-build-friendly libsodium-sys stub build script.
//
// Upstream libsodium-sys 0.2.7 build.rs uses `#[cfg(windows)]` which evaluates
// at *build script compile time* (= host target), not cargo target. When
// cross-compiling from linux host to windows-msvc target, the cfg evaluates
// to `false`, so the build script falls into the autotools path which fails
// because the bundled config.sub/config.guess don't recognize MSVC targets.
//
// This stub bypasses that entirely. It emits link-search directives that point
// at a prebuilt sodium.lib supplied via env var. No autotools, no source build.
//
// Env var contract (priority high-to-low):
//   NERV_SODIUM_LIB_DIR       explicit override (NERV Desk convention)
//   SODIUM_LIB_DIR            upstream convention
//
// Inside the chosen dir we look for, in this order:
//   <dir>/lib/sodium.lib      static library for the MSVC target (NERV Desk CI)
//   <dir>/sodium.lib          same archive, when the env var *is* the lib dir
//   <dir>/lib/libsodium.lib   vcpkg / upstream name, copied to $OUT_DIR/sodium.lib
//   <dir>/libsodium.lib       same, when the env var *is* the lib dir
// The first two are used in place; the last two are copied into $OUT_DIR under
// the name rustc asks for (the source directory is never written to).
//
// Native Windows fallback (task-89)
// ---------------------------------
// Cargo.toml patches libsodium-sys for *every* host, so upstream's native
// Windows path ("We don't build anything on windows, we simply linked to
// precompiled libs.") never runs — this script does. Until task-89 that was
// fatal for a native MSVC build: with no SODIUM_LIB_DIR set this script printed
//   warning: libsodium-sys@0.2.7: nervdesk libsodium-sys stub: no SODIUM_LIB_DIR
//   set; falling back to autotools path
// and emitted no link directive at all, so the final link failed with
//   error: could not find native static library `sodium`, perhaps an -L flag is missing?
// (CI run 37019094509 x86_64 job; the workflow exports SODIUM_LIB_DIR only for
// aarch64, so the x86_64 job always took that path).
//
// We therefore ship the upstream prebuilt archives next to this script and use
// them when neither env var yields an archive:
//   msvc/x64/sodium.lib          x86_64, release profile
//   msvc/x64/debug/sodium.lib    x86_64, any other profile
//   msvc/Win32/sodium.lib        x86,    release profile
//   msvc/Win32/debug/sodium.lib  x86,    any other profile
// They are byte-identical copies of upstream's
// msvc/<arch>/{Release,Debug}/v142/libsodium.lib (see msvc/PROVENANCE.md),
// renamed to `sodium.lib` because the 605 extern blocks in sodium_bindings.rs
// carry `#[cfg_attr(target_env = "msvc", link(name = "sodium", kind = "static"))]`
// and rustc therefore asks for `sodium.lib`; upstream — whose bindings carry no
// link attribute — asks for `libsodium.lib` via
// `rustc-link-lib=static=libsodium` instead.
//
// Why the fallback is safe: scripts/msvc-shim/link.sh (the cross-build linker
// wrapper) appends libsodium positionally *because* this script deliberately
// emits no `rustc-link-lib` (see the note further down). On a native MSVC build
// that wrapper is not in play, so the archive is resolved exactly once — by the
// `#[link]` attributes at final link time — and no duplicate symbols can appear.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Name rustc asks for on an MSVC target. Upstream's own build.rs emits
/// `rustc-link-lib=static=libsodium` (name `libsodium`); this fork's vendored
/// bindings carry `link(name = "sodium")` instead, which is why the archives
/// shipped here are named `sodium.lib`.
const LINK_NAME: &str = "sodium";

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

    println!("cargo:rerun-if-env-changed=NERV_SODIUM_LIB_DIR");
    println!("cargo:rerun-if-env-changed=SODIUM_LIB_DIR");
    println!("cargo:rerun-if-env-changed=SODIUM_SHARED");
    println!("cargo:rerun-if-env-changed=SODIUM_USE_PKG_CONFIG");

    // 1) An archive supplied through the environment wins, as before.
    if let Some(dir) = env_lib_dir() {
        if emit_env_dir(&dir) {
            return;
        }
    }

    // 2) No environment archive: fall back to the vendored upstream archive.
    emit_vendored();
}

/// Reads NERV_SODIUM_LIB_DIR, then SODIUM_LIB_DIR, and normalises it.
///
/// The returned value is interpolated into a `cargo:` directive and into a
/// linker search path, so refuse anything that could forge a directive — a line
/// break would let `...\ncargo:rustc-link-arg=...` through — and canonicalise
/// the path before it is used. The trust boundary here is the operator's own
/// environment, so this is hardening, not privilege separation: it stops a
/// malformed value, not an unprivileged attacker.
fn env_lib_dir() -> Option<PathBuf> {
    let raw = env::var("NERV_SODIUM_LIB_DIR")
        .ok()
        .or_else(|| env::var("SODIUM_LIB_DIR").ok())?;

    if raw.contains('\n') || raw.contains('\r') {
        println!("cargo:warning=nervdesk libsodium-sys stub: NERV_SODIUM_LIB_DIR/SODIUM_LIB_DIR contains a line break; refusing to emit link paths");
        return None;
    }
    if !Path::new(&raw).is_dir() {
        // task-89: this used to return immediately (with a warning about the
        // autotools path), which is what broke the native MSVC build. Ignore the
        // unusable value and let the vendored fallback take over.
        println!("cargo:warning=nervdesk libsodium-sys stub: NERV_SODIUM_LIB_DIR/SODIUM_LIB_DIR={raw} is not a directory; ignoring it");
        return None;
    }
    let dir = match Path::new(&raw).canonicalize() {
        Ok(p) => p,
        Err(e) => {
            println!("cargo:warning=nervdesk libsodium-sys stub: cannot canonicalize {raw}: {e}; refusing to emit link paths");
            return None;
        }
    };
    // `trim_end_matches('/')` below turns a bare "/" into "/lib", so a root-level
    // value would search the host's own /lib. Refuse it instead of silently
    // emitting that.
    if dir.parent().is_none() {
        println!(
            "cargo:warning=nervdesk libsodium-sys stub: {} resolves to the filesystem root; refusing to emit link paths",
            dir.display()
        );
        return None;
    }
    Some(dir)
}

/// Emits the link-search path for an archive found in an env-provided directory.
///
/// Returns false when the directory holds no archive this target can use, in
/// which case the caller falls back to the vendored archive.
fn emit_env_dir(dir: &Path) -> bool {
    // Contract in place since the cross build was introduced: NERV_SODIUM_LIB_DIR
    // is an install prefix and the archive lives in <dir>/lib.
    let prefixed = dir.join("lib").join(format!("{LINK_NAME}.lib"));
    if prefixed.is_file() {
        emit_archive(&dir.join("lib"), &prefixed);
        return true;
    }

    // Upstream's own convention is the opposite: `find_libsodium_env()` emits
    // `rustc-link-search=native=<SODIUM_LIB_DIR>` directly, i.e. the env var
    // already *is* the lib dir. This is what the CI arm64 step exports
    // ($VCPKG_ROOT/installed/<triplet>/lib), and what the previous version of
    // this script could not handle (it appended another `/lib`).
    let direct = dir.join(format!("{LINK_NAME}.lib"));
    if direct.is_file() {
        emit_archive(dir, &direct);
        return true;
    }

    // vcpkg and the upstream crate both name the archive `libsodium.lib`. rustc
    // asks for `sodium.lib` (LINK_NAME), so copy it under that name into
    // $OUT_DIR — the source directory is never written to.
    let gnu_names = [
        dir.join("lib").join("libsodium.lib"),
        dir.join("libsodium.lib"),
    ];
    for gnu in gnu_names.iter() {
        if !gnu.is_file() {
            continue;
        }
        if let Some(dst) = copy_as_link_name(gnu) {
            emit_archive(&dst.0, &dst.1);
            return true;
        }
    }

    println!(
        "cargo:warning=nervdesk libsodium-sys stub: {} holds neither {LINK_NAME}.lib nor libsodium.lib (checked the directory itself and its lib/ subdirectory); ignoring it",
        dir.display()
    );
    false
}

/// Copies `src` to `$OUT_DIR/<LINK_NAME>.lib` and returns (search dir, archive).
///
/// The copy exists because rustc only accepts `<LINK_NAME>.lib`; the source file
/// is left untouched.
fn copy_as_link_name(src: &Path) -> Option<(PathBuf, PathBuf)> {
    let out_dir = match env::var("OUT_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => {
            println!(
                "cargo:warning=nervdesk libsodium-sys stub: OUT_DIR is not set, so {} cannot be copied to {LINK_NAME}.lib",
                src.display()
            );
            return None;
        }
    };
    let dst = out_dir.join(format!("{LINK_NAME}.lib"));
    match fs::copy(src, &dst) {
        Ok(_) => {
            println!(
                "cargo:warning=nervdesk libsodium-sys stub: copied {} to {} (the MSVC link attribute asks for {LINK_NAME}.lib); the source file was not modified",
                src.display(),
                dst.display()
            );
            Some((out_dir, dst))
        }
        Err(e) => {
            println!(
                "cargo:warning=nervdesk libsodium-sys stub: cannot copy {} to {}: {e}; ignoring it",
                src.display(),
                dst.display()
            );
            None
        }
    }
}

/// Emits the vendored native-Windows archive for the target architecture.
fn emit_vendored() {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let sub = match arch.as_str() {
        "x86_64" => "x64",
        "x86" => "Win32",
        other => {
            // Never pass silently: without this panic the build would fail much
            // later with an unspecific linker error (or, worse on a non-MSVC
            // host, link against something unrelated).
            println!(
                "cargo:warning=nervdesk libsodium-sys stub: no vendored libsodium archive for target arch `{other}` (only x86_64 and x86 are shipped); set NERV_SODIUM_LIB_DIR/SODIUM_LIB_DIR to a directory containing {LINK_NAME}.lib"
            );
            // Edition 2015: `panic!` with a single string literal is NOT a
            // format string, so the arch has to be passed as an argument.
            panic!(
                "nervdesk libsodium-sys stub: no vendored libsodium archive for target arch `{}`",
                other
            );
        }
    };

    let root = manifest_dir().join("msvc").join(sub);
    let release = root.join(format!("{LINK_NAME}.lib"));

    // Mirror upstream's `get_lib_dir()`, which selects Release vs Debug with
    // `env::var("PROFILE") == "release"`. MSVC objects record their
    // RuntimeLibrary, so a debug build that links the release archive can fail
    // with LNK2038 — hence both profiles are vendored.
    let profile = env::var("PROFILE").unwrap_or_default();
    let archive = if profile == "release" {
        release
    } else {
        let debug = root.join("debug").join(format!("{LINK_NAME}.lib"));
        if debug.is_file() {
            debug
        } else {
            println!(
                "cargo:warning=nervdesk libsodium-sys stub: {} is missing (profile `{profile}`); using the release archive, which may trip LNK2038 (RuntimeLibrary mismatch) when linking a debug build",
                debug.display()
            );
            release
        }
    };

    if !archive.is_file() {
        println!(
            "cargo:warning=nervdesk libsodium-sys stub: vendored archive {} is missing; the final link will fail with `could not find native static library {LINK_NAME}`",
            archive.display()
        );
        return;
    }

    // Crossing *to* MSVC with these archives cannot work: they are upstream's
    // native-Windows prebuilts. The cross build supplies NERV_SODIUM_LIB_DIR via
    // scripts/cross-msvc.env, so reaching here means that variable was lost —
    // say so instead of linking an archive built for the wrong host ABI.
    if let Ok(host) = env::var("HOST") {
        if !host.ends_with("-windows-msvc") {
            println!(
                "cargo:warning=nervdesk libsodium-sys stub: TARGET is MSVC but HOST={host}; the vendored archive is upstream's native-Windows prebuilt. The cross build is expected to provide NERV_SODIUM_LIB_DIR (scripts/cross-msvc.env does)"
            );
        }
    }

    println!(
        "cargo:warning=nervdesk libsodium-sys stub: no usable NERV_SODIUM_LIB_DIR/SODIUM_LIB_DIR; using the vendored upstream archive {}",
        archive.display()
    );
    let search_dir = archive.parent().unwrap_or(&root).to_path_buf();
    emit_archive(&search_dir, &archive);
}

/// Emits the search path plus the input declaration for one archive.
///
/// Declaring the archive as an input matters: the extern blocks in
/// sodium_bindings.rs carry `#[link(name = "sodium", kind = "static")]`, so rustc
/// *bundles* sodium.lib into liblibsodium_sys-*.rlib. Cargo's fingerprint does
/// not watch that external file, so replacing the archive on disk used to leave
/// the stale rlib in place: the next build reported "Finished dev profile in
/// ~15s" and the final binaries were hard-linked back out of deps/ unchanged,
/// silently discarding the new archive.
///
/// We emit ONLY the search path. Do NOT emit `rustc-link-lib=static=sodium`
/// because rustc would then decompose sodium.lib and embed its .obj files into
/// this crate's rlib. When that rlib is later `--extern`'d by hbb_common /
/// sodiumoxide / rustdesk, the embedded .obj files would conflict with the same
/// symbols in sodium.lib (which our cross-built link.sh appends at the final
/// link step), producing duplicate-symbol errors.
///
/// The `#[link(name="sodium", kind="static")]` attributes on the extern blocks
/// in sodium_bindings.rs tell rustc to resolve all `extern "C"` references
/// against sodium.lib at link time. The link.sh appends sodium.lib as a
/// positional arg, so the final rustdesk link finds every symbol.
fn emit_archive(search_dir: &Path, archive: &Path) {
    println!("cargo:rustc-link-search=native={}", search_dir.display());
    println!("cargo:rerun-if-changed={}", archive.display());
}

/// Cargo passes CARGO_MANIFEST_DIR both when compiling this build script and
/// when running it; prefer the run-time value so a relocated execution still
/// finds the vendored archives.
fn manifest_dir() -> PathBuf {
    env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}
