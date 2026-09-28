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
//! build.rs (sibling) reads NERV_SODIUM_LIB_DIR (preferred) or SODIUM_LIB_DIR,
//! emits `-L native=<dir>/lib -l static=sodium`. The vendored sodium_bindings
//! declares FFI symbols but does NOT carry `#[link]` attributes, so the
//! build.rs-emitted link directives are the only path linking against the
//! prebuilt sodium.lib.

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

mod sodium_bindings;
pub use sodium_bindings::*;