# NOTICE

This product includes software derived from the following Apache-2.0 licensed projects:

## unarc-rs 0.7.2

**Source**: https://github.com/m4b/unarc-rs
**License**: Apache-2.0
**Used for**: Long-tail format readers (RAR, LZH, ACE, ARC, Zoo, ARJ, SQ, UCB, UC2, Ha, Hyp, Ice, and compressed TAR variants)

The following files/modules from `unarc-rs` are carried across (or adapted) in `libarchive-rs`:
- `unarc-rs/src/ace` — ACE reader
- `unarc-rs/src/arc` — ARC reader
- `unarc-rs/src/arj` — ARJ reader
- `unarc-rs/src/bz2` — BZ2 single-stream reader
- `unarc-rs/src/gz` — GZ single-stream reader
- `unarc-rs/src/ha` — Ha reader
- `unarc-rs/src/hyp` — Hyp reader
- `unarc-rs/src/ice` — Ice reader
- `unarc-rs/src/jar` — JAR (solid ZIP) reader
- `unarc-rs/src/lha` — LZH reader (via `delharc` crate)
- `unarc-rs/src/packice` — Pack-Ice reader
- `unarc-rs/src/pi9` — Pack-Ice 9 reader
- `unarc-rs/src/rar` — RAR reader (via `rars` crate)
- `unarc-rs/src/sq` — SQ reader
- `unarc-rs/src/sqz` — SQZ reader
- `unarc-rs/src/tar` — TAR reader
- `unarc-rs/src/tar_bz2` — TBZ reader
- `unarc-rs/src/tar_gz` — TGZ reader
- `unarc-rs/src/tar_z` — TAR.Z reader
- `unarc-rs/src/uc2` — UC2 reader
- `unarc-rs/src/ucb` — UCB reader
- `unarc-rs/src/z` — Z (Unix compress) reader
- `unarc-rs/src/zoo` — Zoo reader

All carried code retains its original Apache-2.0 licence headers. The adaptation to `libarchive-rs`'s streaming `EntrySource` trait and panic-free error model is a derivative work also licensed Apache-2.0.

## sevenz-rust2 0.23.0

**Source**: https://github.com/hasenbanck/sevenz-rust2
**License**: Apache-2.0
**Used for**: 7Z container parsing, folder decoding, AES-256-SHA256 decryption, and codec chains (LZMA, LZMA2, PPMd, BZip2, Deflate, Zstd, Copy, BCJ filters)

`libarchive-rs` uses a vendored copy of `sevenz-rust2` 0.23.0 at `vendor/sevenz-rust2/`. Every difference from upstream is recorded in `vendor/sevenz-rust2/PATCHES.md` and enforced by `tools/check-vendor.sh`. The changes are:

1. `src/block_cursor.rs` — new file, a resumable folder cursor that builds a folder's decode stack once and hands entries out in order (fixes the solid 7Z quadratic decode).
2. `src/block.rs` — added `pub fn num_sub_streams(&self) -> usize`.
3. `src/time.rs` — added `pub fn to_u64(self) -> u64`.
4. `src/lib.rs` — added `pub mod block_cursor;`.
5. `Cargo.toml` — removed examples, wasm dependencies; restored dev-dependencies.

No parser, codec, CRC check, or bounds check was altered. The panic-freedom and fuzz-hardening from upstream 0.23.0 are preserved.

## oxiarc (fork at 93b17af)

**Source**: https://github.com/MaxrregMustermann/oxiarc (fork of https://github.com/akyryl/oxiarc)
**License**: Apache-2.0
**Used for**: Three performance fixes carried forward into `libarchive-rs`'s ZIP and TAR writers:

1. **Deflater reuse across entries** — `ZipWriter` holds a per-level deflater pool (`take_deflater`/`return_deflater`).
2. **Coalesced header writes** — entire ZIP local/central headers emitted in one `write_all` call.
3. **Streaming entry writers** — `add_stream` returns `impl Write`; XZ block buffer `clear()`ed not `take()`n at boundaries.

The oxiarc fork itself is derived from the upstream oxiarc crate (Apache-2.0). The three fixes are implemented in `libarchive-rs` as original code following the same patterns; they are not copied source files.

## Dependency crates (all Apache-2.0 or compatible)

The following crates are statically linked and their licences are included in the binary:

- `brotli` 9.0.0 (Apache-2.0 / MIT)
- `bzip2` 0.6.1 (Apache-2.0 / MIT) — via `libbz2-rs-sys` (pure Rust port)
- `crc32fast` 1.5.2 (Apache-2.0 / MIT)
- `deflate64` 0.1.12 (Apache-2.0 / MIT)
- `flate2` 1.1.10 (Apache-2.0 / MIT) — with `rust_backend` (miniz_oxide, Apache-2.0)
- `lz4_flex` 0.14.0 (Apache-2.0 / MIT)
- `lzma-rust2` 0.21.0 (Apache-2.0)
- `ppmd-rust` 1.5.0 (Apache-2.0 / MIT)
- `zstd` 0.14.1 (Apache-2.0 / BSD-3-Clause) — bundles zstd C source, statically linked
- `aes` 0.9.3 (Apache-2.0 / MIT)
- `aes-gcm` 0.10.3 (Apache-2.0 / MIT)
- `sha1` 0.10.7 (Apache-2.0 / MIT)
- `hmac` 0.13.0 (Apache-2.0 / MIT)
- `pbkdf2` 0.13.0 (Apache-2.0 / MIT)

All dependencies are either pure Rust or statically linked C (zstd), with no runtime system library dependencies. This satisfies the Android `cdylib` requirement that `System.loadLibrary("rust_archive")` loads a self-contained binary.

## Licence summary

`libarchive-rs` is licensed Apache-2.0. Every component it incorporates is Apache-2.0 or a permissive licence compatible with Apache-2.0 (MIT, BSD-3-Clause). The full licence text is in `LICENSE` at the workspace root.