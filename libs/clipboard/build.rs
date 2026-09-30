// NERV Desk: dropped `#[cfg(target_os = "windows")]` so the symbol is
// always present in build.rs. main() gates the call via runtime target_os.
fn build_c_impl() {
    let mut build = cc::Build::new();

    build.file("src/windows/wf_cliprdr.c");

    {
        build.flag_if_supported("-Wno-c++0x-extensions");
        build.flag_if_supported("-Wno-return-type-c-linkage");
        build.flag_if_supported("-Wno-invalid-offsetof");
        build.flag_if_supported("-Wno-unused-parameter");

        if build.get_compiler().is_like_msvc() {
            build.define("WIN32", "");
            // build.define("_AMD64_", "");
            build.flag("-Z7");
            build.flag("-GR-");
            // build.flag("-std:c++11");
        } else {
            build.flag("-fPIC");
            // build.flag("-std=c++11");
            // build.flag("-include");
            // build.flag(&confdefs_path.to_string_lossy());
        }

        build.compile("mycliprdr");
    }

    println!("cargo:rerun-if-changed=src/windows/wf_cliprdr.c");
}

fn main() {
    // NERV Desk: runtime target_os check instead of compile-time cfg.
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        build_c_impl();
    }
}
