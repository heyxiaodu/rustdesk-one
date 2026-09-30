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
    // nervdesk: emit ONLY the search path. Do NOT emit `rustc-link-lib=static=opus`
    // because rustc would then decompose opus.lib and embed its .o files into this
    // crate's rlib. When that rlib is later `--extern`'d by scrap / hbb_common /
    // rustdesk, the embedded .o files would conflict with the same symbols in
    // opus.lib (which our cross-built link.sh appends at the final link step).
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
    // nervdesk: emit ONLY the search path (see link_vcpkg comment above).
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
        // "undefined symbol: opus_*" because find_package was skipped.
        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        if target_os == "windows" {
            // nervdesk: emit ONLY the search path (no `native=` prefix, which on
            // cross-compile adds the path to HOST library search instead of TARGET
            // library search). Do NOT emit `rustc-link-lib=dylib=opus` because
            // rustc converts `dylib=opus` to `-l opus`, and lld-link then looks for
            // `opus.lib` in the LIBPATH entries — but on cross-compile to msvc,
            // rustc's LIBPATH injection may not reach lld-link correctly.
            //
            // The cross-built opus.lib is appended as a positional arg by link.sh,
            // so the final rustdesk link resolves all opus_* symbols. magnum_opus
            // itself does not need to link opus at its own compile step.
            if let Ok(p) = env::var("NERV_LIBOPUS_DIR") {
                println!("cargo:rustc-link-search={}", p);
            } else if let Ok(vcpkg_root) = env::var("VCPKG_INSTALLED_ROOT") {
                println!(
                    "cargo:rustc-link-search={}/x64-windows/lib",
                    vcpkg_root
                );
            } else if let Ok(vcpkg_root) = env::var("VCPKG_ROOT") {
                println!(
                    "cargo:rustc-link-search={}/installed/x64-windows/lib",
                    vcpkg_root
                );
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
