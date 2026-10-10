# Patches to `sevenz-rust2` 0.23.0

`sevenz-rust2` is vendored here rather than depended on from crates.io. This file records
every difference from the upstream 0.23.0 release, so that the vendor can be re-derived or
audited without reading the whole crate.

The vendored source is upstream 0.23.0 verbatim except for the four changes below. Every parser,
every codec and every header field is untouched.

## Why a vendored copy at all

A solid 7Z folder is compressed as one unit, so decoding entry *n* means decoding the folder
from its start. `ArchiveReader::read_file` builds a fresh decode stack per call and does exactly
that, which makes reading *N* entries of a solid folder cost *N* decodes. Measured here: 400
entries took 15.5 s against 204 ms for the same data stored non-solid.

The fix needs a decode stack that outlives the call that built it. Upstream's
`ArchiveReader::build_decode_stack` is private, takes `&mut R`, and returns a reader that borrows
it — so a caller cannot hold one. Reaching it from outside would mean making `decoder`,
`SharedBoundedReader` and the folder-graph walkers public, which is a much larger change to a
fuzzed parser than adding one file that does the job inside the crate.

## The changes

### 1. `src/block_cursor.rs` — new file, the whole fix

A `BlockCursor<R>` that builds a folder's decode stack once and hands the folder's entries out in
order as byte ranges over that one decode. Reading a folder front to back costs one decode.

It builds the **single-stream** coder chain — LZMA, LZMA2, BZip2, Deflate, PPMd, Zstd, Copy and
BCJ filters layered on any of them, which is every folder a file manager meets. A BCJ2 folder
(four input streams, branched coder graph) is reported as unsupported rather than read wrongly;
that matches what upstream's own solid path supports, so nothing that used to open now fails.

Nothing else in the crate was changed to make this work. `BlockCursor` uses only crate-visible
items that already existed: `add_decoder`, `Block::ordered_coder_iter`, `StreamMap`'s accessors
and the crate-private fields.

### 2. `src/time.rs` and `src/block.rs` — two accessors

```rust
// time.rs
pub fn to_u64(self) -> u64 { self.0 }
// block.rs
pub fn num_sub_streams(&self) -> usize { self.num_unpack_sub_streams }
```

Both fields are crate-private upstream. `libarchive-rs` needs the folder's entry count in order
to map entries to folders — which is what lets a solid archive be read in order — and the raw
timestamp in order to convert it without adding a date dependency to a crate that goes into every
archive operation the app does. Both are getters and nothing else.

### 3. `src/block.rs` — one accessor

```rust
pub fn num_sub_streams(&self) -> usize { self.num_unpack_sub_streams }
```

The field is crate-private upstream. `libarchive-rs` needs to know how many of an archive's files
belong to each folder in order to map entries to folders, which is what lets a solid archive be
read in order. It is a getter and nothing else.

### 3. `src/lib.rs` — two lines

```diff
-pub(crate) mod decoder;
+pub mod decoder;
+pub mod block_cursor;
```

`decoder` becomes public so the crate's own documentation for the patch is reachable; nothing
outside uses it. `block_cursor` is declared.

### 4. `Cargo.toml` — optional, dev and wasm dependencies removed

```diff
-nt-time = ["dep:nt-time"]
-[dependencies.nt-time]
-[dev-dependencies.rand]
-[dev-dependencies.tempfile]
-[target.'cfg(target_arch = "wasm32")'.dependencies.js-sys]
-[target.'cfg(target_arch = "wasm32")'.dependencies.wasm-bindgen]
```

None of these is compiled into the library this project builds: `nt-time` is behind a feature
that is never enabled, the dev-dependencies belong to the crate's own test suite (which is kept
on disk under `tests/` and can be run by building this crate standalone), and the wasm target
dependencies are for a target this project does not build for. They are removed only because
resolving them offline fails on a machine whose registry cache does not contain them.

### 5. `Cargo.lock` removed

The upstream lockfile is not meaningful for a path dependency inside a workspace.

## What was *not* patched, and why it matters

`reader.rs`, `archive.rs`, `decoder.rs`, `codec/`, `encryption/`, `time.rs`,
`error.rs` and `writer.rs` are byte-for-byte upstream, and `block.rs` differs only by the one
getter above. In particular:

- `ArchiveReader::read_file` is untouched and still quadratic. This project does not call it; it
  uses `BlockCursor` instead. Leaving it alone is deliberate: it is the function the app calls
  today, so keeping its behaviour means a vendored-copy mistake cannot silently change what the
  unreplaced code path does.
- No header parser, no CRC check and no bounds check was altered. The panic-freedom and
  fuzz-hardening already in upstream 0.23.0 are all still there, and this project adds its own
  tests on top rather than relying on that alone.

## Verifying this file

    diff -r ~/.cargo/registry/src/*/sevenz-rust2-0.23.0/src vendor/sevenz-rust2/src

should report exactly two things: `Only in vendor/.../src: block_cursor.rs`, and `lib.rs`
differing. Anything else is an unrecorded change.
