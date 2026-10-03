//! Cross-build-friendly libsodium-sys FFI stub.
//!
//! We vendor upstream libsodium-sys 0.2.7's bindgen-generated FFI surface
//! (`sodium_bindings.rs`) wholesale, then bypass the upstream build.rs
//! autotools path entirely via our own build.rs (sibling file). The autotools
//! path is broken for cross-compilation to MSVC because the bundled
//! config.sub/config.guess don't recognize MSVC targets.
//!
//! ## Type-name substitution
//!
//! bindgen generated `libc::c_int` etc. We rewrite every `libc::` prefix in
//! `sodium_bindings.rs` to `ffi_types::` (sed pass at stub-creation time).
//! The `ffi_types` module below re-exports `std::os::raw::*`, giving us a
//! stable local alias without pulling in the real `libc` crate (whose
//! transitive feature requirements — `align`, `extra_traits`, `std`, … — make
//! `[patch.crates-io]` substitution untenable).
//!
//! ## Link contract
//!
//! build.rs (sibling) emits `-L native=<dir>`, and the archive it points at is
//! named `sodium.lib` to match the `#[link(name = "sodium", kind = "static")]`
//! attributes this fork adds to every extern block in `sodium_bindings.rs`
//! (605 of them, gated `#[cfg_attr(target_env = "msvc", ...)]`). build.rs
//! deliberately does *not* emit `-l static=sodium`: that would make rustc
//! decompose the archive into this crate's rlib, and the same objects appended
//! again by the cross-build linker wrapper (`scripts/msvc-shim/link.sh`) would
//! then collide as duplicate symbols. On a non-MSVC target no `#[link]`
//! attribute remains, so build.rs emits `-l sodium` for the host instead.
//!
//! The archive comes from `NERV_SODIUM_LIB_DIR` (preferred) or
//! `SODIUM_LIB_DIR`. When neither yields one — the native Windows case, where
//! nothing exports those variables — build.rs falls back to the upstream
//! prebuilt libsodium shipped in `msvc/` (see `msvc/PROVENANCE.md`).

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::all)] // bindgen output; we don't control its naming

/// Local alias for the FFI types sodium_bindings.rs expects.
pub mod ffi_types {
    pub use std::os::raw::{
        c_char, c_int, c_long, c_longlong, c_schar, c_short, c_uchar, c_uint,
        c_ulong, c_ulonglong, c_void,
    };
}

// Every `extern "C" { ... }` block in sodium_bindings.rs carries
// `#[link(name = "sodium", kind = "static")]`, so rustc resolves sodium
// externs against the static sodium.lib (not as dllimport). Without the
// `#[link]` attribute, rustc would emit `__imp_<name>` references on Windows
// MSVC, which lld-link can't resolve against a static archive (which exports
// `<name>` directly).
mod sodium_bindings;
pub use sodium_bindings::*;