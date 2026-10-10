# Patches to `sevenz-rust2` 0.23.0

`sevenz-rust2` is vendored here rather than depended on from crates.io. This file records
every difference from the upstream 0.23.0 release, so that the vendor can be re-derived or
audited without reading the whole crate.

The vendored source is upstream 0.23.0 verbatim except for the changes below. Every parser, every
codec and every header field is untouched.

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

### 2. `src/block.rs` and `src/time.rs` — two accessors

```rust
// block.rs
pub fn num_sub_streams(&self) -> usize { self.num_unpack_sub_streams }
// time.rs
pub fn to_u64(self) -> u64 { self.0 }
```

Both fields are crate-private upstream. `libarchive-rs` needs the folder's entry count in order
to map entries to folders — which is what lets a solid archive be read in order — and the raw
timestamp in order to convert it without adding a date dependency to a crate that goes into every
archive operation the app does. Both are getters and nothing else, and nothing upstream depends on
them.

### 3. `src/lib.rs` — one line

```diff
 pub mod block_cursor;
```

`block_cursor` is declared. (`decoder` is *not* made public; an earlier draft of this file claimed
it was, and it is not.)

### 4. `Cargo.toml` — examples, wasm and unused dependencies removed; dev-dependencies restored

```diff
-[[example]]                       # x6: the examples are not vendored, and a declared
-                                  #     example that does not exist fails the whole build
-[target.'cfg(target_arch = "wasm32")'.dependencies.js-sys]
-[target.'cfg(target_arch = "wasm32")'.dependencies.wasm-bindgen]
+[dev-dependencies.rand]           # the crate's own test suite is kept and run
+[dev-dependencies.tempfile]       # (see below)
```

The wasm target dependencies are for a target this project does not build for. The examples were
never vendored, and Cargo fails the whole workspace when a declared example is missing.

The dev-dependencies were removed on the grounds that the registry cache could not resolve them,
which turned out to be wrong — this workspace already depends on both crates, so they resolve
fine. They are back because upstream's own test suite is the best evidence available that the
vendored copy is not quietly broken, and it is run as part of `cargo test --workspace`.

The `nt-time` dependency and the `nt-time` feature entry were also removed; neither is compiled
into the library this project builds.

### 5. `Cargo.lock` removed

The upstream lockfile is not meaningful for a path dependency inside a workspace.

## Verifying this file

    diff -r ~/.cargo/registry/src/*/sevenz-rust2-0.23.0/src vendor/sevenz-rust2/src

should report exactly three things, and nothing else:

    Only in vendor/.../src: block_cursor.rs
    block.rs differs      # the num_sub_streams getter
    lib.rs differs        # one line, `pub mod block_cursor;`
    time.rs differs       # the to_u64 getter

Anything else in that diff is an unrecorded change and must be either reverted or added here with
a reason. `tools/check-vendor.sh` runs exactly this check and fails the build if it drifts.

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
