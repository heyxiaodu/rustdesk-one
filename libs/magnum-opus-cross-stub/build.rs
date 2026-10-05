use std::{
    env,
    path::{Path, PathBuf},
};

#[cfg(all(target_os = "linux", feature = "linux-pkg-config"))]
fn link_pkg_config(name: &str) -> Vec<PathBuf> {
    let lib = pkg_config::probe_library(name)
        .expect(format!(
            "unable to find '{name}' development headers with pkg-config (feature linux-pkg-config is enabled).
            try installing '{name}-dev' from your system package manager.").as_str());

    lib.include_paths
}

#[cfg(not(all(target_os = "linux", feature = "linux-pkg-config")))]
fn link_vcpkg(mut path: PathBuf, name: &str) -> PathBuf {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let mut target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    if target_arch == "x86_64" {
        target_arch = "x64".to_owned();
    } else if target_arch == "aarch64" {
        target_arch = "arm64".to_owned();
    }
    let mut target = if target_os == "macos" && target_arch == "x64" {
        "x64-osx".to_owned()
    } else if target_os == "macos" && target_arch == "arm64" {
        "arm64-osx".to_owned()
    } else if target_os == "windows" {
        format!("{}-windows-static", target_arch)
    } else {
        format!("{}-{}", target_arch, target_os)
    };
    if target_arch == "x86" {
        target = target.replace("x64", "x86");
    }
    println!("cargo:info={}", target);
    path.push("installed");
    path.push(target);
    // nervdesk: this function is only reachable from find_package(), i.e. from a
    // NATIVE build (Windows/CI with VCPKG_ROOT, or the macOS homebrew fallback).
    // The pregen branch of gen_opus() returns before find_package() is ever
    // called, so our cross path never gets here and keeps emitting the search
    // path only: there rustc must not decompose opus.lib and embed its .o files
    // into this crate's rlib, because link.sh appends the cross-built opus.lib
    // as a positional arg at the final link step. Emitting the static link
    // directive here is therefore safe and required for native builds, where
    // nothing else appends opus.lib.
    println!(
        "{}",
        format!(
            "cargo:rustc-link-lib=static={}",
            name.trim_start_matches("lib")
        )
    );
    println!(
        "{}",
        format!(
            "cargo:rustc-link-search={}",
            path.join("lib").to_str().unwrap()
        )
    );
    let include = path.join("include");
    println!("{}", format!("cargo:include={}", include.to_str().unwrap()));
    include
}

#[cfg(not(all(target_os = "linux", feature = "linux-pkg-config")))]
fn link_homebrew_m1(name: &str) -> PathBuf {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    if target_os != "macos" || target_arch != "aarch64" {
        panic!("Couldn't find VCPKG_ROOT, also can't fallback to homebrew because it's only for macos aarch64.");
    }
    let mut path = PathBuf::from("/opt/homebrew/Cellar");
    path.push(name);
    let entries = if let Ok(dir) = std::fs::read_dir(&path) {
        dir
    } else {
        panic!("Could not find package in {}. Make sure your homebrew and package {} are all installed.", path.to_str().unwrap(),&name);
    };
    let mut directories = entries
        .into_iter()
        .filter(|x| x.is_ok())
        .map(|x| x.unwrap().path())
        .filter(|x| x.is_dir())
        .collect::<Vec<_>>();
    // Find the newest version.
    directories.sort_unstable();
    if directories.is_empty() {
        panic!(
            "There's no installed version of {} in /opt/homebrew/Cellar",
            name
        );
    }
    path.push(directories.pop().unwrap());
    // nervdesk: homebrew fallback is not part of our cross-build path (that one
    // sets NERV_LIBOPUS_DIR), so only the search path is emitted here; see the
    // link_vcpkg comment above for why the cross path must not carry a static
    // link directive.
    // Add the library path.
    println!(
        "{}",
        format!(
            "cargo:rustc-link-search={}",
            path.join("lib").to_str().unwrap()
        )
    );
    // Add the include path.
    let include = path.join("include");
    println!("{}", format!("cargo:include={}", include.to_str().unwrap()));
    include
}

#[cfg(all(target_os = "linux", feature = "linux-pkg-config"))]
fn find_package(name: &str) -> Vec<PathBuf> {
    return link_pkg_config(name);
}

#[cfg(not(all(target_os = "linux", feature = "linux-pkg-config")))]
fn find_package(name: &str) -> Vec<PathBuf> {
    if let Ok(vcpkg_root) = std::env::var("VCPKG_ROOT") {
        vec![link_vcpkg(vcpkg_root.into(), name)]
    } else {
        // Try using homebrew
        vec![link_homebrew_m1(name)]
    }
}

// nervdesk: delegate to the upstream vcpkg path only when a windows-msvc build
// is driven WITHOUT NERV_LIBOPUS_DIR and vcpkg is present. Without VCPKG_ROOT,
// find_package() falls through to link_homebrew_m1(), which panics on every
// non-macos-aarch64 host; with the `linux-pkg-config` feature enabled it would
// instead probe the host with pkg-config and emit host directives for an msvc
// target. In both cases the previous behaviour (no link directive) is kept.
fn nervdesk_delegate_to_vcpkg() -> bool {
    std::env::var("VCPKG_ROOT").is_ok()
        && !cfg!(all(target_os = "linux", feature = "linux-pkg-config"))
}

fn generate_bindings(ffi_header: &Path, include_paths: &[PathBuf], ffi_rs: &Path) {
    #[derive(Debug)]
    struct ParseCallbacks;
    impl bindgen::callbacks::ParseCallbacks for ParseCallbacks {
        fn int_macro(&self, name: &str, _value: i64) -> Option<bindgen::callbacks::IntKind> {
            if name.starts_with("OPUS") {
                Some(bindgen::callbacks::IntKind::Int)
            } else {
                None
            }
        }
    }
    let mut b = bindgen::Builder::default()
        .header(ffi_header.to_str().unwrap())
        .parse_callbacks(Box::new(ParseCallbacks))
        .generate_comments(false);

    for dir in include_paths {
        b = b.clang_arg(format!("-I{}", dir.display()));
    }

    b.generate().unwrap().write_to_file(ffi_rs).unwrap();
}

fn gen_opus() {
    let src_dir = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let src_dir = Path::new(&src_dir);
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let out_dir = Path::new(&out_dir);

    let ffi_rs = out_dir.join("opus_ffi.rs");

    // NERV Desk: when a pre-generated opus_ffi.rs is shipped under
    // generated/, skip find_package + bindgen entirely. This mirrors the
    // scrap/build.rs `nervdesk_try_pregenerated` pattern. The Linux-host
    // build path uses system libopus via the linux-pkg-config feature
    // branch (which is `cfg!(target_os = "linux")` evaluated in the
    // build-script host); the Windows-msvc cross-build path will pick up
    // the pre-generated file and skip the link_pkg_config call. The
    // eventual production Windows build will use VCPKG_ROOT and
    // find_package("opus") (see the else branch below).
    let pregen = src_dir.join("generated").join("opus_ffi.rs");
    if pregen.is_file() {
        println!("cargo:rerun-if-changed={}", pregen.display());
        if let Err(e) = std::fs::copy(&pregen, &ffi_rs) {
            eprintln!(
                "nervdesk magnum-opus stub: failed to copy {} → {}: {}",
                pregen.display(),
                ffi_rs.display(),
                e
            );
            std::process::exit(1);
        }
        println!(
            "cargo:warning=nervdesk magnum-opus stub: using pre-generated opus_ffi.rs (target_os={})",
            std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_else(|_| "unknown".into())
        );

        // NERV Desk: emit explicit link directives so the Windows-msvc
        // cross-link can find opus.lib. Without this, the link fails with
        // "undefined symbol: opus_*" because find_package is skipped on the
        // cross path.
        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        if target_os == "windows" {
            // nervdesk: the cross path emits ONLY the search path (no `native=`
            // prefix, which on cross-compile adds the path to HOST library
            // search instead of TARGET library search). Do NOT emit
            // `rustc-link-lib=dylib=opus` because rustc converts `dylib=opus`
            // to `-l opus`, and lld-link then looks for `opus.lib` in the
            // LIBPATH entries — but on cross-compile to msvc, rustc's LIBPATH
            // injection may not reach lld-link correctly.
            //
            // The cross-built opus.lib is appended as a positional arg by link.sh,
            // so the final rustdesk link resolves all opus_* symbols. magnum_opus
            // itself does not need to link opus at its own compile step.
            if let Ok(p) = env::var("NERV_LIBOPUS_DIR") {
                println!("cargo:rustc-link-search={}", p);
            } else if let Ok(vcpkg_root) = env::var("VCPKG_INSTALLED_ROOT") {
                // nervdesk: fork-only arm (upstream magnum-opus has no
                // VCPKG_INSTALLED_ROOT at all). Emit the static link directive
                // that belongs with this search path: a prefix directory
                // without `-l static=opus` fails at link time with
                // "undefined symbol: opus_*" and no hint pointing here.
                // The hard-coded dynamic triplet below stays as-is on purpose
                // (registered as U4 in the task-90 report).
                println!("cargo:rustc-link-lib=static=opus");
                println!(
                    "cargo:rustc-link-search={}/x64-windows/lib",
                    vcpkg_root
                );
            } else if nervdesk_delegate_to_vcpkg() {
                // nervdesk: a native Windows / CI build (no NERV_LIBOPUS_DIR)
                // with vcpkg available — hand opus over to the upstream vcpkg
                // path, which emits both `rustc-link-lib=static=opus` and the
                // `<VCPKG_ROOT>/installed/<triplet>-windows-static/lib` search
                // path (triplet derived from CARGO_CFG_TARGET_ARCH, matching
                // CI's x64-windows-static). This branch previously hard-coded
                // the dynamic `installed/x64-windows/lib` triplet and emitted
                // no link directive at all.
                let _ = find_package("opus");
            } else {
                // nervdesk: neither our cross-build variable nor vcpkg is
                // available. Stay silent rather than warn here: this branch is
                // also taken by our cross build (cross-msvc.env sets
                // NERV_LIBAOM_DIR / NERV_LIBVPX_DIR / NERV_LIBYUV_DIR but NOT
                // NERV_LIBOPUS_DIR — the cross-built opus.lib is appended as a
                // positional arg by link.sh), so a warning would change the
                // cross-build build-script output. The card requires the cross
                // path to stay byte-identical.
            }
        } else if target_os == "ios" {
            // nervdesk: the Linux/macOS/FreeBSD arm below links the host's
            // libopus by name, but iOS has no system libopus and this arm was
            // missing entirely, so the pre-generated branch emitted NO link
            // directive at all. The iOS link then failed with:
            //   Undefined symbols for architecture arm64:
            //     _opus_decode_float / _opus_decoder_create / _opus_decoder_destroy
            //     _opus_encode_float / _opus_encoder_create / _opus_encoder_destroy
            //     _opus_strerror
            // (referenced by libmagnum_opus-*.rlib). CI installs opus through
            // vcpkg (`lukka/run-vcpkg`, triplet arm64-ios), so use the same
            // upstream vcpkg hand-over as the windows arm above; without
            // VCPKG_ROOT, find_package() would fall through to
            // link_homebrew_m1() (panics on non-macos-aarch64 hosts), so we
            // warn instead of trying.
            if nervdesk_delegate_to_vcpkg() {
                let _ = find_package("opus");
            } else {
                println!(
                    "cargo:warning=nervdesk magnum-opus stub: VCPKG_ROOT is not set; emitting no link directive for opus (target_os=ios)"
                );
            }
        } else if target_os == "linux" || target_os == "macos" || target_os == "freebsd" {
            // nervdesk (m09940): the same early-`return` trap as the other two
            // cross-stubs (scrap, libsodium-sys). Because we skip
            // `find_package("opus")` at the bottom of this function whenever the
            // pre-generated bindings exist, the `linux-pkg-config` feature path
            // never runs and NOTHING emits `-lopus`. The host link then failed
            // with:
            //   rust-lld: error: undefined symbol: opus_encoder_create
            //   rust-lld: error: undefined symbol: opus_decode_float
            //   rust-lld: error: undefined symbol: opus_strerror
            // (7 symbols: opus_{encoder,decoder}_{create,destroy},
            //  opus_{encode,decode}_float, opus_strerror)
            //
            // libopus-dev provides /usr/lib/<triple>/libopus.so (1.3.1 on this
            // host) plus the .pc file; emit the equivalent directive so the host
            // toolchain finds it. `cargo check` never caught this because check
            // does not link.
            //
            // Two environments need two different directives, exactly like the
            // `libs/scrap/build.rs` linux arm:
            //   * a distro host (our dev boxes) ships libopus.so in the default
            //     search path, so the bare name is enough;
            //   * the CI `build rustdesk linux drm x86_64` job runs inside a
            //     bionic container that removes libopus-dev ("we have libopus
            //     compiled by us") and installs opus through vcpkg instead
            //     (`VCPKG_ROOT=/opt/artifacts/vcpkg`, `--triplet x64-linux`,
            //     whose pinned triplet sets VCPKG_LIBRARY_LINKAGE static). There
            //     the bare name only resolved because `scrap` happened to emit
            //     the vcpkg `-L` for its own four libraries - a coupling we
            //     should not rely on. Hand the package over to the upstream
            //     vcpkg path when it is available, and keep the bare name
            //     otherwise. macOS/FreeBSD keep the bare name: their system
            //     libopus works today and is not what the container job links.
            if target_os == "linux" && nervdesk_delegate_to_vcpkg() {
                let _ = find_package("opus");
            } else {
                println!("cargo:rustc-link-lib=opus");
            }
        }
        return;
    }

    let includes = find_package("opus");
    let ffi_header = src_dir.join("opus_ffi.h");
    println!("rerun-if-changed={}", ffi_header.display());
    for dir in &includes {
        println!("rerun-if-changed={}", dir.display());
    }

    generate_bindings(&ffi_header, &includes, &ffi_rs);
}

fn main() {
    gen_opus()
}
