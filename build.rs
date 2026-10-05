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
        let clang = std::process::Command::new("xcrun")
            .args(["--find", "clang"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "clang".to_owned());
        let resource_dir = std::process::Command::new(&clang)
            .arg("--print-resource-dir")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        match resource_dir {
            Some(dir) => println!("cargo:rustc-link-search=native={}/lib/darwin", dir),
            None => println!(
                "cargo:warning=nervdesk: cannot locate the clang resource dir; \
                 the iOS link may fail on ___chkstk_darwin"
            ),
        }
        println!("cargo:rustc-link-lib=clang_rt.ios");
    }
    if target_os == "android" {
        build_android_ifaddrs();
    }
    println!("cargo:rerun-if-changed=build.rs");
}
