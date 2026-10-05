# native/ — cross-compiled native libraries for the Windows x64 build

> **中文摘要**：本目录两个脚本用于重建 Windows x64 (MSVC) 交叉构建所依赖的两个
> 手工制作的原生产物 —— `sodium.lib` 与 `immintrin_stubs.o`。产物落在仓库之外
> (`analysis/build-env/`)。**最重要的一条**：`libsodium-sys` 用
> `#[link(kind = "static")]`（`bundle` 默认 true），rustc 会把 `sodium.lib`
> 拷进 rlib，而 cargo 的 fingerprint 不追踪该外部 `.lib`；换掉磁盘上的
> `sodium.lib` 后必须手动删除 rlib/fingerprint/最终产物**以及 `deps/` 下的同名产物**，
> 否则构建会「成功」但二进制根本没变。详见下文 *The rlib bundle trap*。
> （**round-9 起**：`libsodium-sys` 不再由 `Cargo.toml` 永久 patch —— patch 只在交叉
> 构建里生效，由 `scripts/cross-msvc-patch.toml` + `--config` 注入；原生 Windows MSVC
> 作业与非 Windows CI 作业都用 registry 版 crate。见下文 *Where `libsodium-sys` comes
> from*。）

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

Both scripts are safe to re-run and both assert their own post-conditions rather
than trusting the build, but **"atomic" would be the wrong word for either of
them, and they differ from each other**:

* `build-libsodium-msvc.sh` **copies** the previous archive aside to
  `sodium.lib.bak-<UTC timestamp>` (`cp -p` — not a rename) before installing the
  new one, so a failed build of the new archive leaves the previous one in place.
* `build-immintrin-stubs.sh` compiles **straight over** the installed object: no
  temporary file, no backup, no marker. A compile failure therefore destroys the
  previous good object and the only recovery is to re-run the script. The failure
  itself is loud (exit 2), so this is a recovery cost, not a silent corruption.

One of the checks the sodium script performs is conditional; it is documented
separately below so it is not mistaken for an unconditional gate.

---

## `build-libsodium-msvc.sh`

```sh
repos/rustdesk/scripts/native/build-libsodium-msvc.sh [--jobs N] [--out DIR] [--keep-src]
```

`--out` is validated before anything is written: it must be non-empty, must not
resolve to `/`, and must resolve (after symlinks) to a directory **inside** the
project tree. The empty case used to be genuinely destructive — `--out ""` made
`$OUT_DIR/lib`, `$OUT_DIR/include` and the recursive
`rm -rf "$OUT_DIR/include/sodium"` collapse onto `/lib`, `/include` and
`/include/sodium`, and this cross-build runs as root. **There is no environment
override for this check**: it is the only place in the project where a single
argument could damage the host, so no escape hatch exists (same principle as the
`EXPECTED_*` constants — a gate the guarded party can switch off is not a gate).
If you need a scratch copy, put it in a directory inside the project tree (e.g.
`analysis/security/.tmp-f7/`) and delete it afterwards. `--jobs` must be a
positive integer.

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
  the symbol itself (that repair was deleted in round-5 — see
  "Reproducible builds and the removed `/ALTERNATENAME` shim" below), so the
  loader read the struct's *first field*
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
* the archive's **member-name set** matches the pinned 106-name baseline list
  (hard failure, exit 4). Member names are derived only from the source-tree
  layout and the naming rule, so the set is machine-independent — which is what
  makes it the one *output-side* invariant that can be asserted hard. This gate
  is **unconditional and fail-closed**: it never consults a hashing tool, so it
  holds even on a machine with neither `sha256sum` nor `openssl`. The archive's
  **sha256** and byte size are printed alongside it for human
  comparison but deliberately **not** asserted, because 7 of 106 members embed an
  absolute source path (see *Fidelity to the shipped archive*). A second,
  redundant gate cross-checks the member-set digest against the pinned
  `EXPECTED_MEMBER_SET_SHA256`; if no hashing tool exists it cannot run, and the
  script then prints an explicit `WARN: … cross-check was SKIPPED (<unavailable>)`
  — it never passes that comparison silently, and it never reports a false
  "digest out of sync" error in that situation;
* `__imp_` symbol count is exactly **10**, and all 10 match the
  Windows-API allowlist (`Enter|Initialize|LeaveCriticalSection`, `Sleep`,
  `Virtual{Alloc,Free,Lock,Protect,Unlock}`, `GetSystemInfo`) — these are
  genuine kernel32 imports, not sodium self-references;
* `sodium_init`, `randombytes_buf`, `randombytes_implementation_name`,
  `randombytes_sysrandom_implementation` are all defined;
* archive format is COFF — **a diagnostic, not a gate**. A non-COFF archive
  prints `WARN: could not confirm COFF archive format` on stderr and the build
  continues (exit 0). Only `build-immintrin-stubs.sh` fails hard on a non-COFF
  object, and it does so with **exit 3**. Do not read this line as an assertion.

### Conditional check: `_mm_*` stub coverage — this one does NOT always run

The script additionally cross-checks that **every** `_mm_*` symbol the archive
leaves undefined is provided by `$NERV_IMMINTRIN_STUBS_O`
(`intrinsics needed: 67 / provided by stub: 75`), and exits 4 listing the missing
names otherwise. That is the check which would have caught the `_mm_slli_epi64`
breakage before `lld-link` did — but it is **conditional**, because it needs the
stub object to compare against:

| `NERV_IMMINTRIN_STUBS_O` | behaviour |
| --- | --- |
| set, pointing at an existing file | the check runs |
| unset, or pointing at nothing | `NOTE: ... skipping _mm_* coverage check`; the archive is still installed and the script still exits 0 |
| set and existing, but one symbol list comes back empty | `WARN: could not read one of the symbol lists; skipping coverage check` |

`scripts/cross-msvc.env` does export this variable, but running the script does
**not** source that file — the usage line above asks for no such step — so if you
invoke the script directly, the check does not run.

**Not running this check does not mean the archive is broken.** Stub coverage is a
*link-time* property: if the archive needs an intrinsic the stub does not provide,
`lld-link` fails hard with

```
lld-link-19: error: undefined symbol: _mm_slli_epi64
```

so an uncovered intrinsic can never be silently linked into a working binary.
Skipping the check costs an earlier and clearer error message, not correctness.
Making it unconditional is deliberately not done: the stub object is built by a
different script and need not exist at all for a valid `sodium.lib`.

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
exactly **77** `T` symbols and is COFF. That count is hard-coded in the script
and is **not** overridable from the environment (the former
`NERV_IMMINTRIN_EXPECTED_T` override is gone: a guard whose threshold the
environment can set is not a guard). `--check` compiles to a temporary file and
diffs the symbol set against the installed object, which proves the installed
object still matches its source.

The object must stay a **separate positional argument** at link time
(`$NERV_IMMINTRIN_STUBS_O`, appended by `link.sh`). Do **not** fold it into
`sodium.lib`: `#[link(kind = "static")]` bundles the archive into the rlib, so
the rlib would then carry its own copies of those `_mm_*` definitions and
lld-link would report duplicate symbols.

---

## The rlib bundle trap

Read this before trusting any rebuild of `sodium.lib`.

### Where `libsodium-sys` comes from

`Cargo.toml` no longer patches the crate. `[patch.crates-io]` there carries only
`libxdo-sys`; the libsodium patch was moved out in round 9 because a manifest
patch also applied to every other build, where the stub cannot work (on
non-MSVC targets it emits a bare `-l sodium`, and those CI images ship no system
libsodium — that broke nine jobs). Today:

| Build | `libsodium-sys` | libsodium archive |
| --- | --- | --- |
| `./scripts/cross-build-msvc.sh …` | `libs/libsodium-sys-cross-stub` (injected with `--config scripts/cross-msvc-patch.toml`) | `NERV_SODIUM_LIB_DIR`, else the stub's vendored `msvc/<arch>/…` copy |
| native Windows MSVC jobs (CI) | registry crate `libsodium-sys 0.2.7` | the crate's vendored `msvc/x64/{Release,Debug}/v142/libsodium.lib`, or `SODIUM_LIB_DIR` for arm64 |
| non-Windows CI jobs | registry crate `libsodium-sys 0.2.7` | built by the crate itself (libsodium sources ship inside it, `bundle` → no `-l sodium` on the final link) |

The stub keeps whatever triggers the trap: `sodium_bindings.rs` has 605
`extern "C"` blocks, each with
`#[cfg_attr(target_env = "msvc", link(name = "sodium", kind = "static"))]`, so
on an MSVC target rustc still bundles the archive into
`liblibsodium_sys-*.rlib`. What the stub emits for that target is only the
search path and a watch, `cargo:rustc-link-search=native=<dir>` +
`cargo:rerun-if-changed=<archive>` (`libs/libsodium-sys-cross-stub/build.rs:335-338`);
it deliberately emits no `-l static=sodium`, because `scripts/msvc-shim/link.sh`
appends the archive as a positional argument and the same objects twice would
collide as duplicate symbols. **The trap below is therefore unchanged by the
move**, and the remedy still applies to cross builds.

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
`NERV_*_LIB_DIR` to **every** link, with no name filter. So any file in one of
those directories **whose name ends in `.lib`** — including a hand-made copy such
as `sodium-old.lib` — is silently linked in alongside the real archive.

The automatic backups these scripts create are **not** affected: they are named
`sodium.lib.bak-<UTC timestamp>`, which does not end in `.lib`, so the glob does
not match them. (An earlier version of this note claimed the backups "get linked
too". That was wrong — verified by expanding the glob.) Keep only the real
archive in each directory and move stray `.lib` files elsewhere (e.g.
`stray-quarantine/`); otherwise you are silently linking an unknown mixture of
archives.

## Reproducible builds and the removed `/ALTERNATENAME` shim (round-5)

Two unconditional changes were made to `scripts/msvc-shim/link.sh`; both are
motivated by the same goal — the link step must be explainable and its output
must be checkable byte-for-byte.

1. **The `/ALTERNATENAME` segment is deleted.** The shim used to append 36
   `/ALTERNATENAME:__imp_<sym>=<sym>` lines, wrapped in
   `if [ "${NERV_DISABLE_SODIUM_ALTNAME:-0}" != "1" ]` with an `else` branch
   that printed three WARN lines, plus a 24-line comment block. Once
   `-DSODIUM_STATIC` was defined, that segment was dead code: the P2-9
   experiment recorded by the team fed 475 inputs through the link and got
   **0/36 hits** on those aliases, and flipping the kill-switch no longer
   changed the output. `grep -c 'ALTERNATENAME\|NERV_DISABLE_SODIUM_ALTNAME'
   scripts/msvc-shim/link.sh` now returns **0**. If a link ever fails with
   `undefined __imp_<sodium symbol>`, the cause is a **stale `sodium.lib`** —
   rebuild it with `-DSODIUM_STATIC` and clear the rlib/fingerprint groups
   above. Do **not** reintroduce the aliases: on the data symbol
   `randombytes_sysrandom_implementation` they produced the
   `FFFFFFFFFFFFFFFF` page fault described in the flags section.

2. **`/Brepro` is now passed to lld-link.** It is appended in the
   common-flags area (immediately after the argv loop, next to the
   `/MACHINE:X64` default) — never inside a build-profile branch and never
   behind an environment gate. lld-link then stores a hash of the executable
   in the COFF `TimeDateStamp` field instead of the wall-clock time, so two
   consecutive links of identical inputs are byte-identical.

   The acceptance check for (2) requires a **real relink on both runs**:
   delete the final four artefacts in `debug/` **and** their hard links in
   `debug/deps/` (the four groups listed under "The rlib bundle trap") before
   each run. Otherwise cargo relinks nothing and the sha256 comparison is
   vacuous. Measured byte sizes and hashes are recorded in
   `analysis/wine-smoke/round5-build-baseline.md`.
