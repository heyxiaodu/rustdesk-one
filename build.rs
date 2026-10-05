// NERV Desk: dropped `#[cfg(target_os = "windows")]` so the symbol is
// always present in build.rs. main() gates the call via target_os check.
fn build_windows() {
    let file = "src/platform/windows.cc";
    let file2 = "src/platform/windows_delete_test_cert.cc";
    cc::Build::new().file(file).file(file2).compile("windows");
    println!("cargo:rustc-link-lib=WtsApi32");
    println!("cargo:rerun-if-changed={}", file);
    println!("cargo:rerun-if-changed={}", file2);
}

// NERV Desk: dropped `#[cfg(target_os = "macos")]` so the symbol is always
// present in build.rs. main() gates the call via target_os check.
fn build_mac() {
    let file = "src/platform/macos.mm";
    let mut b = cc::Build::new();
    if let Ok(os_version::OsVersion::MacOS(v)) = os_version::detect() {
        let v = v.version;
        if v.contains("10.14") {
            b.flag("-DNO_InputMonitoringAuthStatus=1");
        }
    }
    b.flag("-std=c++17").file(file).compile("macos");
    println!("cargo:rerun-if-changed={}", file);
}

#[cfg(all(target_os = "windows", feature = "inline"))]
fn build_manifest() {
    use std::io::Write;
    if std::env::var("PROFILE").unwrap() == "release" {
        let mut res = winres::WindowsResource::new();
        res.set_icon("res/icon.ico")
            .set_language(winapi::um::winnt::MAKELANGID(
                winapi::um::winnt::LANG_ENGLISH,
                winapi::um::winnt::SUBLANG_ENGLISH_US,
            ))
            .set_manifest_file("res/manifest.xml");
        match res.compile() {
            Err(e) => {
                write!(std::io::stderr(), "{}", e).unwrap();
                std::process::exit(1);
            }
            Ok(_) => {}
        }
    }
}

// bionic only exports getifaddrs()/freeifaddrs() from API 24, while the jniLibs
// are built against the API 21 sysroot (flutter/ndk_*.sh). webrtc-util calls
// them, so without this the android link fails on undefined symbols.
fn build_android_ifaddrs() {
    let file = "src/platform/android_ifaddrs.c";
    cc::Build::new().file(file).compile("android_ifaddrs");
    println!("cargo:rerun-if-changed={}", file);
}

fn install_android_deps() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    if target_os != "android" {
        return;
    }
    let mut target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    if target_arch == "x86_64" {
        target_arch = "x64".to_owned();
    } else if target_arch == "x86" {
        target_arch = "x86".to_owned();
    } else if target_arch == "aarch64" {
        target_arch = "arm64".to_owned();
    } else {
        target_arch = "arm".to_owned();
    }
    let target = format!("{}-android", target_arch);
    let vcpkg_root = std::env::var("VCPKG_ROOT").unwrap();
    let mut path: std::path::PathBuf = vcpkg_root.into();
    if let Ok(vcpkg_root) = std::env::var("VCPKG_INSTALLED_ROOT") {
        path = vcpkg_root.into();
    } else {
        path.push("installed");
    }
    path.push(target);
    println!(
        "cargo:rustc-link-search={}",
        path.join("lib").to_str().unwrap()
    );
    println!("cargo:rustc-link-lib=ndk_compat");
    println!("cargo:rustc-link-lib=c++");
    println!("cargo:rustc-link-lib=OpenSLES");
}

// NERV Desk: locate compiler-rt for the iOS link.
//
// `-lclang_rt.ios` alone is not enough: rustc drives the Apple link with
// `-nodefaultlibs`, so clang's driver adds neither compiler-rt nor its
// resource-dir lib path (observed cc argv: `-lclang_rt.ios` present, no
// `-L…/usr/lib/clang/<ver>/lib/darwin`, then `ld: library 'clang_rt.ios' not
// found`). Ask the toolchain instead of guessing, and never emit a search path
// we cannot confirm:
//   * if `xcrun` is missing or unusable, do NOT fall back to a bare `clang`
//     taken from PATH: on a host without Xcode that resolves to a non-Apple
//     clang (measured here: /usr/bin/clang is llvm-14 and its reported
//     resource dir /usr/lib/llvm-14/lib/clang/14.0.6/lib/darwin does not
//     exist), i.e. the link would get a wrong, nonexistent `-L` and no warning.
//   * if the resolved clang is not an Apple toolchain, or its resource dir has
//     no `lib/darwin`, warn and emit no search path, so a broken toolchain
//     fails loudly instead of silently pointing at the wrong directory.
//
// Every guard failure prints one stable marker so CI can grep for it:
// `no compiler-rt search path`.
fn nervdesk_ios_compiler_rt_dir() -> Option<String> {
    let clang = match std::process::Command::new("xcrun")
        .args(["--find", "clang"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => {
            println!(
                "cargo:warning=nervdesk ios: no compiler-rt search path \
                 (`xcrun --find clang` is unavailable); the iOS link may fail on ___chkstk_darwin"
            );
            return None;
        }
    };
    if clang.is_empty() {
        println!(
            "cargo:warning=nervdesk ios: no compiler-rt search path \
             (`xcrun --find clang` returned an empty path)"
        );
        return None;
    }
    if !clang.contains("Xcode.app") && !clang.contains("/Developer/Toolchains") {
        println!(
            "cargo:warning=nervdesk ios: no compiler-rt search path \
             (`xcrun` resolved a non-Apple clang at {})",
            clang
        );
        return None;
    }
    let resource_dir = match std::process::Command::new(&clang)
        .arg("--print-resource-dir")
        .output()
    {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => {
            println!(
                "cargo:warning=nervdesk ios: no compiler-rt search path \
                 (`{} --print-resource-dir` failed)",
                clang
            );
            return None;
        }
    };
    let dir = std::path::Path::new(&resource_dir)
        .join("lib")
        .join("darwin");
    if resource_dir.is_empty() || !dir.is_dir() {
        println!(
            "cargo:warning=nervdesk ios: no compiler-rt search path \
             (reported resource dir `{}` has no lib/darwin directory)",
            resource_dir
        );
        return None;
    }
    Some(dir.to_string_lossy().into_owned())
}

fn main() {
    hbb_common::gen_version();
    install_android_deps();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    if target_os == "windows" {
        #[cfg(all(target_os = "windows", feature = "inline"))]
        build_manifest();
        build_windows();
    }
    if target_os == "macos" {
        build_mac();
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
    }
    if target_os == "ios" {
        // NERV Desk: rustc drives the Apple link with `-nodefaultlibs`, so
        // clang's driver neither adds compiler-rt nor even its resource-dir
        // lib path (observed cc argv: `-lclang_rt.ios` present, no
        // `-L…/usr/lib/clang/<ver>/lib/darwin`, then
        //   ld: library 'clang_rt.ios' not found).
        // The vcpkg arm64-ios objects linked through libs/scrap
        // (aom_convolve.c.o, aom_scaled_convolve8_neon.c.o,
        // intrapred_neon.c.o, subpel_variance_neon.c.o — built for iOS 26.5
        // while we link for 10.0) call the arm64 stack-probe helper
        // `___chkstk_darwin`, which only compiler-rt provides for such an
        // old deployment target, so ask the toolchain where its builtins
        // live and link them. macOS is deliberately untouched (green today).
        if let Some(dir) = nervdesk_ios_compiler_rt_dir() {
            println!("cargo:rustc-link-search=native={}", dir);
        }
        println!("cargo:rustc-link-lib=clang_rt.ios");
    }
    if target_os == "android" {
        build_android_ifaddrs();
    }
    println!("cargo:rerun-if-changed=build.rs");
}
