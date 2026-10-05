use std::{
    env, fs,
    path::{Path, PathBuf},
    println,
};

// ============================================================================
// NERV Desk: pre-generated bindgen fallback (Phase-2 windows-msvc gate)
//
// Upstream gen_vcpkg_package unconditionally calls find_package() which
// panics on non-macos-aarch64 hosts without VCPKG_ROOT. This breaks
// `cargo check --target=x86_64-pc-windows-msvc` from a Linux host.
//
// When libs/scrap/generated/{name}_ffi.rs exists, we skip bindgen and copy the
// pre-generated file to OUT_DIR. Link directives are NOT skipped; they are
// emitted per target, so every build environment gets what it needs:
//   * windows-msvc + NERV_LIB<NAME>_DIR  -> our cross build: search path only
//     (the cross-built .lib is appended as a positional arg by link.sh);
//   * windows-msvc without NERV_* but with VCPKG_ROOT -> a native Windows / CI
//     build, which is handed over to the upstream vcpkg path (find_package ->
//     link_vcpkg), emitting `rustc-link-lib=static=<lib>` + the
//     `<VCPKG_ROOT>/installed/<triplet>-windows-static/lib` search path;
//   * linux host -> link the system library by name.
//
// To re-generate the bindings on a different host:
//   bindgen --rust-target 1.75 --rustified-enum "^.*" \
//           --allowlist-X "<regex>" --output generated/{name}_ffi.rs \
//           src/bindings/{name}_ffi.h
// where --allowlist-X matches the regex below.
// ============================================================================

fn nervdesk_try_pregenerated(name: &str, generated: &str) -> Option<Vec<PathBuf>> {
    let target_os = env::var("CARGO_CFG_TARGET_OS").ok()?;
    let src_dir = env::var_os("CARGO_MANIFEST_DIR")?;
    let src_dir = Path::new(&src_dir);
    let out_dir = env::var_os("OUT_DIR")?;
    let out_dir = Path::new(&out_dir);

    let pregen = src_dir.join("generated").join(generated);
    if !pregen.is_file() {
        return None;
    }

    println!("cargo:rerun-if-changed={}", pregen.display());

    let dest = out_dir.join(generated);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).ok();
    }
    if let Err(e) = fs::copy(&pregen, &dest) {
        eprintln!(
            "nervdesk scrap stub: failed to copy {} → {}: {}",
            pregen.display(),
            dest.display(),
            e
        );
        return None;
    }
    println!(
        "cargo:warning=nervdesk scrap stub: using pre-generated {} for {} (target_os={})",
        generated, name, target_os
    );
    // nervdesk: when cross-compiling for windows-msvc, emit ONLY the search
    // path. We intentionally do NOT emit `rustc-link-lib=static=` here because
    // rustc would then DECOMPOSE the .lib archive and EMBED its .o files into
    // this crate's rlib. When that rlib is later `--extern`'d by rustdesk, the
    // embedded .o files would be linked again — producing duplicate symbols
    // with the same .lib that our cross-built link.sh appends at the final
    // link step.
    //
    // The cross-built .libs are appended as positional args by link.sh, so the
    // final rustdesk link resolves all symbols. scrap itself does not need to
    // link them at its own compile step.
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let short = name.trim_start_matches("lib");
        let env_var = format!("NERV_LIB{}_DIR", short.to_uppercase());
        println!("cargo:rerun-if-env-changed={}", env_var);
        if let Ok(lib_dir) = env::var(&env_var) {
            println!("cargo:rustc-link-search={}", lib_dir);
        } else if nervdesk_delegate_to_vcpkg() {
            // nervdesk: not our cross build (no NERV_LIB<NAME>_DIR), but a
            // native Windows / CI build where vcpkg is available. Hand the
            // package over to the upstream vcpkg path, which emits both the
            // `rustc-link-lib=static=` directive and the
            // `<VCPKG_ROOT>/installed/<triplet>-windows-static/lib` search
            // path. Bailing out here instead leaves a native Windows build with
            // no link directive at all.
            return Some(find_package(name));
        } else {
            // nervdesk: keep the previous behaviour (no link directive) rather
            // than risk find_package's link_homebrew_m1 panic, which fires on
            // every non-macos-aarch64 host without VCPKG_ROOT.
            println!(
                "cargo:warning=nervdesk scrap stub: neither {} nor VCPKG_ROOT is set; emitting no link directive for {}",
                env_var, name
            );
        }
    } else if target_os == "linux" {
        // nervdesk: on a Linux host this pre-generated branch short-circuits
        // `nervdesk_handle_package()` before it can reach `find_package()`, so
        // without this arm NO link directive is emitted at all and the final
        // rustdesk link fails with:
        //   rust-lld: undefined symbol: vpx_codec_vp8_dx / vpx_codec_vp9_dx
        //             aom_codec_av1_dx / aom_codec_av1_cx / FixedDiv_X86 ...
        // Two environments need two different directives:
        //   * a distro host (our dev boxes) ships libvpx.so / libaom.so /
        //     libyuv.so / libopus.so in the default search path, so the bare
        //     name is enough;
        //   * the CI `build rustdesk linux drm x86_64` job builds inside a
        //     bionic run-on-arch container that deliberately installs NO
        //     libvpx-dev / libaom-dev / libyuv-dev and even removes
        //     libopus-dev ("we have libopus compiled by us"), so the bare name
        //     finds nothing and the link dies with
        //       rust-lld: error: unable to find library -lopus / -lvpx /
        //                 -laom / -lyuv
        // There the four libraries exist only in vcpkg
        // (`VCPKG_ROOT=/opt/artifacts/vcpkg`, `--triplet x64-linux`, whose
        // pinned triplet sets VCPKG_LIBRARY_LINKAGE static), so hand the
        // package over to the upstream vcpkg path exactly like the Apple arm
        // below. The msvc arm above is unaffected (target_os is "windows"
        // there) and keeps using the cross-built .lib search path.
        if nervdesk_delegate_to_vcpkg() {
            return Some(find_package(name));
        }
        let short = name.trim_start_matches("lib");
        println!("cargo:rustc-link-lib={}", short);
    } else if target_os == "macos" || target_os == "ios" {
        // nervdesk: the same early-`return` trap as the Linux arm above, but for
        // the Apple targets. Because the pre-generated snapshots
        // (`generated/{vpx,aom,yuv}_ffi.rs`) make `nervdesk_handle_package()`
        // return before it can reach `find_package()`, NOTHING emitted
        // `-lvpx/-laom/-lyuv` and the final link (cdylib + the drm lib test)
        // failed with:
        //   Undefined symbols for architecture x86_64/arm64:
        //     _ABGRToARGB / _ARGBToI420 / _ARGBToI444 / _ARGBToNV12
        //     _I420ToABGR / _I420ToARGB / _I420ToRAW / _I444ToABGR
        //     vpx_codec_vp8_dx / aom_codec_av1_dx / FixedDiv_X86 ...
        // CI installs these through vcpkg (`lukka/run-vcpkg`, triplets
        // x64-osx / arm64-osx / arm64-ios), so hand the package over to the
        // upstream vcpkg path — exactly what the non-pre-generated path does.
        // Without VCPKG_ROOT we keep the previous behaviour (warning, no
        // directive): find_package() would fall through to link_homebrew_m1(),
        // which panics on any host that is not macos-aarch64.
        if nervdesk_delegate_to_vcpkg() {
            return Some(find_package(name));
        }
        println!(
            "cargo:warning=nervdesk scrap stub: VCPKG_ROOT is not set; emitting no link directive for {} (target_os={})",
            name, target_os
        );
    }
    // bindgen is skipped, so include paths are only needed by the native-vcpkg
    // hand-over above, which returns find_package()'s paths directly.
    Some(Vec::new())
}

// nervdesk: delegate to the upstream vcpkg path only when a windows-msvc build
// is driven WITHOUT our cross-build environment variables and vcpkg is present.
// Both extra conditions matter:
//   * without VCPKG_ROOT, find_package() falls through to link_homebrew_m1(),
//     which panics on every non-macos-aarch64 host;
//   * with the `linux-pkg-config` feature enabled, find_package() takes the
//     pkg_config branch (host probing) and would emit host directives for an
//     msvc target.
// In both cases the previous behaviour (warning + no link directive) is kept.
fn nervdesk_delegate_to_vcpkg() -> bool {
    env::var("VCPKG_ROOT").is_ok() && !cfg!(all(target_os = "linux", feature = "linux-pkg-config"))
}

#[cfg(all(target_os = "linux", feature = "linux-pkg-config"))]
fn link_pkg_config(name: &str) -> Vec<PathBuf> {
    // sometimes an override is needed
    let pc_name = match name {
        "libvpx" => "vpx",
        _ => name,
    };
    let lib = pkg_config::probe_library(pc_name)
        .expect(format!(
            "unable to find '{pc_name}' development headers with pkg-config (feature linux-pkg-config is enabled).
            try installing '{pc_name}-dev' from your system package manager.").as_str());

    lib.include_paths
}
#[cfg(not(all(target_os = "linux", feature = "linux-pkg-config")))]
fn link_pkg_config(_name: &str) -> Vec<PathBuf> {
    unimplemented!()
}

/// Link vcpkg package.
fn link_vcpkg(mut path: PathBuf, name: &str) -> PathBuf {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let mut target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    if target_arch == "x86_64" {
        target_arch = "x64".to_owned();
    } else if target_arch == "x86" {
        target_arch = "x86".to_owned();
    } else if target_arch == "loongarch64" {
        target_arch = "loongarch64".to_owned();
    } else if target_arch == "aarch64" {
        target_arch = "arm64".to_owned();
    } else {
        target_arch = "arm".to_owned();
    }
    let mut target = if target_os == "macos" {
        if target_arch == "x64" {
            "x64-osx".to_owned()
        } else if target_arch == "arm64" {
            "arm64-osx".to_owned()
        } else {
            format!("{}-{}", target_arch, target_os)
        }
    } else if target_os == "windows" {
        format!("{}-windows-static", target_arch)
    } else {
        format!("{}-{}", target_arch, target_os)
    };
    if target_arch == "x86" {
        target = target.replace("x64", "x86");
    }
    println!("cargo:info={}", target);
    if let Ok(vcpkg_root) = std::env::var("VCPKG_INSTALLED_ROOT") {
        path = vcpkg_root.into();
    } else {
        path.push("installed");
    }
    path.push(target);
    println!(
        "cargo:rustc-link-lib=static={}",
        name.trim_start_matches("lib")
    );
    println!(
        "cargo:rustc-link-search={}",
        path.join("lib").to_str().unwrap()
    );
    let include = path.join("include");
    println!("cargo:include={}", include.to_str().unwrap());
    include
}

/// Link homebrew package(for Mac M1).
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
    // Link the library.
    println!(
        "cargo:rustc-link-lib=static={}",
        name.trim_start_matches("lib")
    );
    // Add the library path.
    println!(
        "cargo:rustc-link-search={}",
        path.join("lib").to_str().unwrap()
    );
    // Add the include path.
    let include = path.join("include");
    println!("cargo:include={}", include.to_str().unwrap());
    include
}

/// Find package. By default, it will try to find vcpkg first, then homebrew(currently only for Mac M1).
/// If building for linux and feature "linux-pkg-config" is enabled, will try to use pkg-config
/// unless check fails (e.g. NO_PKG_CONFIG_libyuv=1)
fn find_package(name: &str) -> Vec<PathBuf> {
    let no_pkg_config_var_name = format!("NO_PKG_CONFIG_{name}");
    println!("cargo:rerun-if-env-changed={no_pkg_config_var_name}");
    if cfg!(all(target_os = "linux", feature = "linux-pkg-config"))
        && std::env::var(no_pkg_config_var_name).as_deref() != Ok("1")
    {
        link_pkg_config(name)
    } else if let Ok(vcpkg_root) = std::env::var("VCPKG_ROOT") {
        vec![link_vcpkg(vcpkg_root.into(), name)]
    } else {
        // Try using homebrew
        vec![link_homebrew_m1(name)]
    }
}

fn generate_bindings(
    ffi_header: &Path,
    include_paths: &[PathBuf],
    ffi_rs: &Path,
    exact_file: &Path,
    regex: &str,
) {
    let mut b = bindgen::builder()
        .header(ffi_header.to_str().unwrap())
        .allowlist_type(regex)
        .allowlist_var(regex)
        .allowlist_function(regex)
        .rustified_enum(regex)
        .trust_clang_mangling(false)
        .layout_tests(false) // breaks 32/64-bit compat
        .generate_comments(false); // comments have prefix /*!\

    for dir in include_paths {
        b = b.clang_arg(format!("-I{}", dir.display()));
    }

    b.generate().unwrap().write_to_file(ffi_rs).unwrap();
    fs::copy(ffi_rs, exact_file).ok(); // ignore failure
}

fn gen_vcpkg_package(package: &str, ffi_header: &str, generated: &str, regex: &str) {
    let includes = find_package(package);
    let src_dir = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let src_dir = Path::new(&src_dir);
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let out_dir = Path::new(&out_dir);

    let ffi_header = src_dir.join("src").join("bindings").join(ffi_header);
    println!("rerun-if-changed={}", ffi_header.display());
    for dir in &includes {
        println!("rerun-if-changed={}", dir.display());
    }

    let ffi_rs = out_dir.join(generated);
    let exact_file = src_dir.join("generated").join(generated);
    generate_bindings(&ffi_header, &includes, &ffi_rs, &exact_file, regex);
}

// If you have problems installing ffmpeg, you can download $VCPKG_ROOT/installed from ci
// Linux require link in hwcodec
/*
fn ffmpeg() {
    // ffmpeg
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let static_libs = vec!["avcodec", "avutil", "avformat"];
    static_libs.iter().for_each(|lib| {
        find_package(lib);
    });
    if target_os == "windows" {
        println!("cargo:rustc-link-lib=static=libmfx");
    }

    // os
    let dyn_libs: Vec<&str> = if target_os == "windows" {
        ["User32", "bcrypt", "ole32", "advapi32"].to_vec()
    } else if target_os == "linux" {
        let mut v = ["va", "va-drm", "va-x11", "vdpau", "X11", "stdc++"].to_vec();
        if target_arch == "x86_64" {
            v.push("z");
        }
        v
    } else if target_os == "macos" || target_os == "ios" {
        ["c++", "m"].to_vec()
    } else if target_os == "android" {
        ["z", "m", "android", "atomic"].to_vec()
    } else {
        panic!("unsupported os");
    };
    dyn_libs
        .iter()
        .map(|lib| println!("cargo:rustc-link-lib={}", lib))
        .count();

    if target_os == "macos" || target_os == "ios" {
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=CoreVideo");
        println!("cargo:rustc-link-lib=framework=CoreMedia");
        println!("cargo:rustc-link-lib=framework=VideoToolbox");
        println!("cargo:rustc-link-lib=framework=AVFoundation");
    }
}
*/

fn main() {
    // in this crate, these are also valid configurations
    println!("cargo:rustc-check-cfg=cfg(dxgi,quartz,x11)");

    // there is problem with cfg(target_os) in build.rs, so use our workaround
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();

    // note: all link symbol names in x86 (32-bit) are prefixed wth "_".
    // run "rustup show" to show current default toolchain, if it is stable-x86-pc-windows-msvc,
    // please install x64 toolchain by "rustup toolchain install stable-x86_64-pc-windows-msvc",
    // then set x64 to default by "rustup default stable-x86_64-pc-windows-msvc"
    let target = target_build_utils::TargetInfo::new();
    if target.unwrap().target_pointer_width() != "64" {
        // panic!("Only support 64bit system");
    }
    env::remove_var("CARGO_CFG_TARGET_FEATURE");
    env::set_var("CARGO_CFG_TARGET_FEATURE", "crt-static");

    // NERV Desk: when the corresponding libs/scrap/generated/{name}_ffi.rs
    // exists (committed for cross-build from non-Windows hosts), bindgen is
    // skipped and the pre-generated, rustc-type-checked file is copied to
    // OUT_DIR. Link directives are NOT omitted: nervdesk_try_pregenerated
    // emits them for our windows-msvc cross build (search path only), hands a
    // native Windows build over to the upstream vcpkg path
    // (find_package -> link_vcpkg) when VCPKG_ROOT is set, and links the system
    // library by name on a Linux host.
    fn nervdesk_handle_package(package: &str, ffi_header: &str, generated: &str, regex: &str) {
        if nervdesk_try_pregenerated(package, generated).is_some() {
            return;
        }
        match package {
            "libyuv" => {
                // find_package for libyuv is used to populate include paths
                // for the yuv_ffi.h bindgen invocation below; when using the
                // pre-generated file we don't need it.
                let _ = find_package("libyuv");
            }
            _ => {}
        }
        gen_vcpkg_package(package, ffi_header, generated, regex);
    }
    nervdesk_handle_package("libvpx", "vpx_ffi.h", "vpx_ffi.rs", "^[vV].*");
    nervdesk_handle_package("aom", "aom_ffi.h", "aom_ffi.rs", "^(aom|AOM|OBU|AV1).*");
    nervdesk_handle_package("libyuv", "yuv_ffi.h", "yuv_ffi.rs", ".*");
    // ffmpeg();

    if target_os == "ios" {
        // nothing
    } else if target_os == "android" {
        println!("cargo:rustc-cfg=android");
    } else if target_os == "windows" {
        // The first choice is Windows because DXGI is amazing.
        println!("cargo:rustc-cfg=dxgi");
    } else if target_os == "macos" {
        // Quartz is second because macOS is the (annoying) exception.
        println!("cargo:rustc-cfg=quartz");
    } else {
        // On other UNIX (linux, freebsd, etc.) we pray that X11 (with XCB)
        // is available. Note: upstream used `cfg!(unix)` / `cfg!(windows)`
        // here, which evaluates against the build-script host. That breaks
        // cross-compile: a Linux host building for windows-msvc still has
        // `cfg!(unix) == true`, so the x11 cfg flag would be set and the
        // x11 module would compile, then fail because libc::shmat etc.
        // are unix-only. Use the cargo TARGET variables instead.
        println!("cargo:rustc-cfg=x11");
    }
}
