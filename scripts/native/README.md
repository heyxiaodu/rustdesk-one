# native/ — cross-compiled native libraries for the Windows x64 build

> **中文摘要**：本目录两个脚本用于重建 Windows x64 (MSVC) 交叉构建所依赖的两个
> 手工制作的原生产物 —— `sodium.lib` 与 `immintrin_stubs.o`。产物落在仓库之外
> (`analysis/build-env/`)。**最重要的一条**：`libsodium-sys` 用
> `#[link(kind = "static")]`（`bundle` 默认 true），rustc 会把 `sodium.lib`
> 拷进 rlib，而 cargo 的 fingerprint 不追踪该外部 `.lib`；换掉磁盘上的
> `sodium.lib` 后必须手动删除 rlib/fingerprint/最终产物**以及 `deps/` 下的同名产物**，
> 否则构建会「成功」但二进制根本没变。详见下文 *The rlib bundle trap*。

These two scripts rebuild the two hand-made native inputs of the
`x86_64-pc-windows-msvc` cross-build. Both outputs live **outside** the
repository, under `analysis/build-env/`, because they are build artefacts and
not sources. The scripts only read the cargo registry and the libsodium source
tree; they never write to either.

| Script | Output |
| --- | --- |
| `build-libsodium-msvc.sh` | `analysis/build-env/nervdesk-sodium-msvc/{lib/sodium.lib, include/sodium/}` |
| `build-immintrin-stubs.sh` | `analysis/build-env/stubs/obj/immintrin_stubs.o` |

Default locations are derived from the script's own path
(`$(dirname "$BASH_SOURCE")/../..`), so the checkout stays relocatable. The
`NERV_*` variables that `repos/rustdesk/scripts/cross-msvc.env` exports
(`NERV_BE`, `NERV_XWIN`, `NERV_SHIM`, `NERV_SODIUM_LIB_DIR`,
`NERV_SODIUM_INCLUDE_DIR`, `NERV_IMMINTRIN_STUBS_O`) override every default.

Both scripts are safe to re-run: they install atomically-ish (the previous
archive is renamed to `sodium.lib.bak-<UTC timestamp>` first) and they assert
their own post-conditions rather than trusting the build.

---

## `build-libsodium-msvc.sh`

```sh
repos/rustdesk/scripts/native/build-libsodium-msvc.sh [--jobs N] [--out DIR] [--keep-src]
```

### Source of truth, and the provenance gate

The archive the cross-build consumed was **not** built from the upstream
libsodium release tarball. It was built from the libsodium source **vendored
inside the `libsodium-sys` crate** (`libsodium-sys 0.2.7`, pinned by
`Cargo.lock`), i.e. `~/.cargo/registry/src/*/libsodium-sys-0.2.7/libsodium`.
The script locates that tree via `find_registry_src()` (override with
`NERV_SODIUM_SRC`) and then **refuses to build** unless three fingerprints
match, so the upstream tarball can never be substituted silently:

1. exactly **106** `.c` files under `src/libsodium`;
2. `crypto_core/ed25519/ref10/ed25519_ref10.c` **contains** `fe25519_notsquare`;
3. the same file does **not** contain `chi25519`.

Two facts that look like they prove the source but do not: both trees ship
`builds/msvc/version.h` with `SODIUM_LIBRARY_VERSION` **10.3**, and both have
106 `.c` files. The real discriminator is the ed25519 ref10 file above — the
upstream 1.0.18 tarball lacks `fe25519_mul32` / `fe25519_notsquare` /
`fe25519_sqmul` and instead has `chi25519`, which changes 28 of 106 members.

### Flags, and why each one is there

`--target=x86_64-pc-windows-msvc -ffunction-sections -fdata-sections -DSODIUM_STATIC`

* **`-DSODIUM_STATIC` is the entire reason this script exists.** Without it,
  `sodium/export.h` makes every `SODIUM_EXPORT` a `__declspec(dllimport)`
  under `_MSC_VER`. That is merely wasteful for functions, but **wrong for
  data**: for the data symbol `randombytes_sysrandom_implementation` a
  dllimport reference is compiled as "load a pointer out of the import slot".
  The old `/ALTERNATENAME:__imp_<sym>=<sym>` repair made that slot resolve to
  the symbol itself, so the loader read the struct's *first field*
  (`const char *name`) and used it as an implementation pointer, giving a
  deterministic
  `wine: Unhandled page fault on read access to FFFFFFFFFFFFFFFF at 0000000142975C8A`.
  The libsodium MSVC project files agree that the macro is required for static
  linking: `builds/msvc/vs20XX/libsodium/libsodium.props` and
  `libsodium.import.props` define `SODIUM_STATIC` when the linkage is
  `StaticLibrary`.
* **No `-O` flag.** The shipped archive is an `-O0` build. Verified by
  probing the same source file: `-O0` → 114,671 B (byte-identical to the
  shipped member apart from the COFF timestamp), `-O1` → 64,270,
  `-O2` → 65,102, `-O3` → 65,100, `-Os` → 63,333. The optimisation level also
  changes which `_mm_*` intrinsics the archive leaves undefined, which is
  **linker-visible**: do not change it silently.
* **No `-fPIC`.** clang rejects `-fPIC` for the `*-msvc` target. Note that
  `clang-wrap.sh` strips `-fPIC` from `argv`, but **not** from a response file
  (`@file`) — flags baked into an rsp bypass its filter. A static COFF archive
  needs no PIC anyway.
* **No `-march`.** x86_64 already implies SSE2; enabling SSSE3/AVX only grows
  the set of `_mm_*` stubs that must be supplied at link time.

### Post-conditions the script asserts

* 106 objects compiled, 0 failures, 106 archive members;
* `__imp_` symbol count is exactly **10**, and all 10 match the
  Windows-API allowlist (`Enter|Initialize|LeaveCriticalSection`, `Sleep`,
  `Virtual{Alloc,Free,Lock,Protect,Unlock}`, `GetSystemInfo`) — these are
  genuine kernel32 imports, not sodium self-references;
* archive format is COFF;
* `sodium_init`, `randombytes_buf`, `randombytes_implementation_name`,
  `randombytes_sysrandom_implementation` are all defined;
* **every** `_mm_*` symbol left undefined by the archive is provided by
  `$NERV_IMMINTRIN_STUBS_O` (`intrinsics needed: 67 / provided by stub: 75`),
  otherwise it exits 4 with the missing names. This is the gate that would
  have caught the `_mm_slli_epi64` breakage before lld-link did.

### Fidelity to the shipped archive

Rebuilding from the right tree reproduces the shipped `sodium.lib`
(2,283,100 B) to 2,282,638 B: all **106/106 member names identical**,
**99/106 members byte-identical except the 2-byte COFF `TimeDateStamp`**, and
the remaining **7 differ only in an embedded absolute source path** — a
`__FILE__` string in `.rdata` (`strings -e l` shows
`.../libsodium/src/libsodium/sodium/core.c`); the `.rdata` delta is exactly the
path-length delta (e.g. 181 − 148 = 33 UTF-16 chars = 66 bytes). There is no
code or symbol-set difference.

Consequence: the archive bytes depend on **where the source tree lives**. If
path-independent (cross-machine reproducible) archives are ever wanted, add
`-ffile-prefix-map="$SRC=/libsodium"`; that was deliberately **not** done here,
because this script's job is to reproduce the shipped artefact, and adding the
map would move *further* away from it.

---

## `build-immintrin-stubs.sh`

```sh
repos/rustdesk/scripts/native/build-immintrin-stubs.sh [--check]
```

Clang's MSVC-target headers do not provide every `_mm_*` builtin that
libsodium's Argon2 SSSE3 code path uses. `_mm_slli_epi64` was missing, which
surfaced as
`lld-link-19: error: undefined symbol: _mm_slli_epi64` (referenced by
`.../crypto_pwhash_argon2_argon2-fill-block-ssse3.obj:(fill_block_with_xor)`,
446 references). This script compiles the replacements in
`analysis/build-env/stubs/immintrin_stubs.c` and asserts the object defines
exactly **77** `T` symbols and is COFF. `--check` compiles to a temporary file
and diffs the symbol set against the installed object, which proves the
installed object still matches its source.

The object must stay a **separate positional argument** at link time
(`$NERV_IMMINTRIN_STUBS_O`, appended by `link.sh`). Do **not** fold it into
`sodium.lib`: `#[link(kind = "static")]` bundles the archive into the rlib, so
the rlib would then carry its own copies of those `_mm_*` definitions and
lld-link would report duplicate symbols.

---

## The rlib bundle trap

Read this before trusting any rebuild of `sodium.lib`.

`libsodium-sys` declares `#[link(name = "sodium", kind = "static")]`. `bundle`
defaults to `true`, so rustc **copies `sodium.lib` into
`liblibsodium_sys-*.rlib`**, and cargo's fingerprint does not track that
external `.lib`. Replacing `sodium.lib` on disk therefore does **not** trigger
a rebuild.

Symptom: `./scripts/cross-build-msvc.sh --features quic` reports
`Finished dev profile in 15.77s`, but the output mtime is unchanged and
`nlink == 2` — the final artefacts were hard-linked straight back from `deps/`.
A build that "succeeds" in seconds without touching the binary is this trap,
not a cache win.

To force a real relink, delete all four groups:

```
target/x86_64-pc-windows-msvc/debug/deps/liblibsodium_sys-*
target/x86_64-pc-windows-msvc/debug/.fingerprint/libsodium-sys-*
target/x86_64-pc-windows-msvc/debug/{rustdesk.exe,librustdesk.dll,service.exe,naming.exe}
target/x86_64-pc-windows-msvc/debug/deps/{rustdesk.exe,rustdesk.pdb,
    librustdesk.dll,librustdesk.dll.lib,librustdesk.pdb,
    service.exe,service.pdb,naming.exe,naming.pdb}
```

The last group matters: deleting only the final artefacts is **not** enough,
because cargo restores them from `deps/` as hard links.

Then confirm the new archive really went in:

```sh
llvm-nm-19 --undefined-only target/x86_64-pc-windows-msvc/debug/deps/liblibsodium_sys-*.rlib | grep '^__imp_'
```

It must list only the 10 Windows APIs. Any `__imp_<sodium symbol>` here means
the stale rlib is still cached.

## Keep the `NERV_*_LIB_DIR` directories clean

`scripts/msvc-shim/link.sh` appends **every** `*.lib` it finds in each
`NERV_*_LIB_DIR` to **every** link, with no name filter. Backups created by
these scripts therefore get linked too. Keep only the real archive in each
directory and move backups elsewhere (e.g. `stray-quarantine/`); otherwise you
are silently linking an unknown mixture of archives.
