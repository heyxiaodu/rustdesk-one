# Provenance of the vendored libsodium archives (`libs/libsodium-sys-cross-stub/msvc/`)

These four files are **byte-identical copies of upstream prebuilt archives**. They
are not built by this repository, not patched, and not stripped. They exist so
that a **native Windows (MSVC) build** of this fork can link libsodium without
any environment variable, because `repos/rustdesk/Cargo.toml` patches
`libsodium-sys` for *every* host with `libs/libsodium-sys-cross-stub`, which
replaces upstream's native-Windows code path
(`#[cfg(windows)] fn make_libsodium()` — "We don't build anything on windows, we
simply linked to precompiled libs.").

## Source

| Item | Value |
| --- | --- |
| Crate | `libsodium-sys` |
| Version | `0.2.7` |
| Origin | crates.io registry checkout (downloaded by cargo, pinned by `Cargo.lock`) |
| Local path | `$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/libsodium-sys-0.2.7` |
| Repo upstream | `https://github.com/sodiumoxide/sodiumoxide.git` |
| Vendored on | 2026-10-02 (task-89) |

## File mapping

`cmp` compares each vendored file against the registry file; `sha256` is of the
vendored file (identical to the source's).

| Vendored file | Upstream source file | Bytes | sha256 | `cmp` |
| --- | --- | ---: | --- | --- |
| `msvc/x64/sodium.lib` | `msvc/x64/Release/v142/libsodium.lib` | 1,828,500 | `f6fa6ef5cb0793c69f8e8dea7ebc279b51ed5cdcd82de5d7461e98cb7691aae8` | identical |
| `msvc/x64/debug/sodium.lib` | `msvc/x64/Debug/v142/libsodium.lib` | 2,483,818 | `84d69f1d2ed34247c68f31b9a30706481fcd67dbb2a3ffbb866d1d06b8814397` | identical |
| `msvc/Win32/sodium.lib` | `msvc/Win32/Release/v142/libsodium.lib` | 1,922,854 | `76a59feb83f412112b5e2fe97ccc90af5acc42e67252ef734cd3d42a9689f6d4` | identical |
| `msvc/Win32/debug/sodium.lib` | `msvc/Win32/Debug/v142/libsodium.lib` | 2,011,018 | `c5d3651f5344840fa8720a7fa184cec11b8eba4a72e2d07455bdda36b1637ad4` | identical |

Release and Debug are **different** files (they differ from byte 33 onward), so
they are kept apart exactly as upstream ships them.

## Why the files are renamed to `sodium.lib`

* This fork's `src/sodium_bindings.rs` carries 605 extern blocks with
  `#[cfg_attr(target_env = "msvc", link(name = "sodium", kind = "static"))]`
  (first at `src/sodium_bindings.rs:270`). rustc therefore resolves the archive
  as `sodium.lib` on an MSVC target.
* Upstream's `src/sodium_bindings.rs` carries **no** link attribute: upstream's
  `build.rs` emits `cargo:rustc-link-lib=static=libsodium` and keeps the file
  named `libsodium.lib`.
* Hence the same archive content must be named `sodium.lib` for this fork, and
  only this fork's naming is used in `build.rs` (`const LINK_NAME`).

## Which file `build.rs` picks

`build.rs` mirrors upstream's `get_lib_dir()` / `is_release_profile()`:

| Target arch (`CARGO_CFG_TARGET_ARCH`) | `PROFILE=release` | any other profile |
| --- | --- | --- |
| `x86_64` | `msvc/x64/sodium.lib` | `msvc/x64/debug/sodium.lib` |
| `x86` | `msvc/Win32/sodium.lib` | `msvc/Win32/debug/sodium.lib` |

Any other arch (e.g. `aarch64`) has no archive here: `build.rs` emits a
`cargo:warning` and fails with an explicit `panic!` rather than linking
something unrelated. A release directory is never silently substituted for a
debug one without a warning.

## Licenses

| Artifact | License | Text |
| --- | --- | --- |
| The `libsodium-sys` crate (bindings, build script) | MIT OR Apache-2.0 | `LICENSE-MIT`, `LICENSE-APACHE` (copied here, identical to upstream) |
| The compiled library inside `*.lib` (libsodium 1.0.18, as bundled by the crate) | ISC | `LICENSE-ISC-libsodium` (copy of the crate's `libsodium/LICENSE`) |

`build.rs` links with libsodium 1.0.18 unless a caller overrides it with
`NERV_SODIUM_LIB_DIR` / `SODIUM_LIB_DIR`.

## How to re-derive / re-verify

```sh
SRC="$HOME/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsodium-sys-0.2.7"
cd /vol2/@appdata/deepseek.harness/home/NERVDesk/repos/rustdesk/libs/libsodium-sys-cross-stub
cmp msvc/x64/sodium.lib        "$SRC/msvc/x64/Release/v142/libsodium.lib"      && echo identical
cmp msvc/x64/debug/sodium.lib  "$SRC/msvc/x64/Debug/v142/libsodium.lib"        && echo identical
cmp msvc/Win32/sodium.lib      "$SRC/msvc/Win32/Release/v142/libsodium.lib"    && echo identical
cmp msvc/Win32/debug/sodium.lib "$SRC/msvc/Win32/Debug/v142/libsodium.lib"     && echo identical
sha256sum msvc/x64/sodium.lib msvc/x64/debug/sodium.lib msvc/Win32/sodium.lib msvc/Win32/debug/sodium.lib
```

Boundary: the `sha256` values above come from the same artifact the cargo
registry served (a local cache); they were **not** cross-checked against an
independent publisher hash, so they prove local integrity and traceability, not
the authenticity of the upstream release.
