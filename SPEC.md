# Format specification

This document lists every format `libarchive-rs` knows, and for each one states what it does and does not do: streaming or whole-entry, encrypted or not, and which long-tail formats are refused.

## Legend

- **Streaming**: `open_entry` returns a `Read` that pulls from the decompressor incrementally. No entry is materialised in memory.
- **Whole-entry**: The format architecture requires decompressing the entire entry (or block) before any byte is available. In this crate the entry is still returned as a `Read`, but the decoder must consume the whole compressed stream before the first output byte.
- **Solid**: Multiple entries share one compressed block. Reading entry *n* requires decoding the block from its start. In-order reads reuse the block decoder; out-of-order reads restart it.
- **Encrypted read**: The format can list encrypted entries and read them when a password is supplied.
- **Encrypted write**: The format can create encrypted archives.
- **Refused**: The format is recognised by signature but `open` returns `ErrorKind::Unsupported`.

## Core formats (gate — tested on every merge)

| Format | Read | Write | Streaming | Solid | Encrypted read | Encrypted write | Notes |
|--------|------|-------|-----------|-------|----------------|-----------------|-------|
| ZIP | ✅ | ✅ | ✅ | No | ❌ (refused) | ❌ | Deflate, Deflate64, Bzip2, LZMA, Zstd. Data descriptor (streaming) supported. |
| 7Z | ✅ | ❌ **stub** | ✅ | Yes | ✅ (AES-256-SHA256) | ❌ | Reading works: solid folders decoded in order via `BlockCursor`; BCJ2 refused. **Writing is a stub** — `SevenZWriter::create` returns `Unimplemented`. The app writes 7Z today through sevenz-rust2, so this is a functional regression until it is implemented. |
| TAR | ✅ | ✅ | ✅ | No | N/A | N/A | UStar, GNU, PAX. GNU sparse and PAX extended headers read. |
| Compressed TAR (.tar.gz, .tar.bz2, .tar.xz, .tar.zst, .tar.lz4) | ✅ | ❌ **stub** | ✅ | No | N/A | N/A | Reading works and streams: the compression layer is decoded and TAR entries stream inside it. **Writing is a stub** — `CompressedTarWriter::create` returns `Unimplemented`. |
| SingleStream (gz, bz2, xz, zst, br, lz4) | ✅ | ❌ **stub** | ✅ | No | N/A | N/A | One entry by definition. Reading works for all of these. **Writing is a stub** — `SingleStreamWriter::create` returns `Unimplemented` for every method, including the two (`XZ`, `Zstd`) an earlier version of this document wrongly claimed worked. |
| SingleStream `.Z` (Unix compress / LZW) | ❌ refused | ❌ | — | No | N/A | N/A | Recognised by `detect` and refused: no LZW decoder is wired. |

**Writing is implemented for ZIP and plain TAR only.** `writer::what_can_be_written` pins that in a
test, so a document and a test cannot both drift away from the code. It is the single largest gap
in the crate: the app creates ZIP, TAR and 7Z today, and 7Z creation is a stub.

## Long-tail formats (pending — legacy team)

These are the twelve formats in `formats::legacy::AVAILABLE`. `unarc-rs` also reads JAR, SQZ and Pack-Ice; JAR is a solid ZIP and SQZ and Pack-Ice are not yet in `Format`, so they are not claimed here.

All twelve are recognised by signature and listed in `Format`. All return `ErrorKind::Unsupported` from `formats::legacy::open` because the carried `unarc-rs` parsers have not been adapted to the streaming `EntrySource` trait yet. The app depends on `unarc-rs` for these, so this is a functional regression until the legacy team lands the adapters.

| Format | Reader type (from unarc-rs) | Streaming target | Encrypted read | Encrypted write | Status |
|--------|----------------------------|------------------|----------------|-----------------|--------|
| RAR | whole-entry (pre-parsed via `rars`) | streaming-adjacent | Yes (via `rars`) | No | Refused |
| LZH | streaming-adjacent (via `delharc`) | streaming | No | No | Refused |
| ACE | streaming-adjacent | streaming | Yes | No | Refused |
| ARC | streaming-adjacent | streaming | Yes (CRC-based) | No | Refused |
| Zoo | streaming-adjacent | streaming | No | No | Refused |
| ARJ | whole-entry (pre-parsed) | streaming-adjacent | Yes | No | Refused |
| SQ | whole-entry | streaming-adjacent | No | No | Refused |
| UCB | whole-entry | streaming-adjacent | No | No | Refused (no fixture exists) |
| UC2 | whole-entry (pre-parsed CDIR) | streaming-adjacent | No | No | Refused |
| Ha | streaming-adjacent | streaming | No | No | Refused |
| Hyp | streaming-adjacent | streaming | No | No | Refused |
| Ice | whole-entry | streaming-adjacent | No | No | Refused |

**Note on reader types**: "Streaming-adjacent" means the format's architecture supports reading entries by index with the archive held open (it has a directory or entry chain that can be walked without decompressing data). "Whole-entry" means the existing `unarc-rs` parser decompresses the entire entry into a `Vec<u8>` before returning. The `libarchive-rs` goal is to adapt every parser to return `std::io::Read` from `open_entry`; for whole-entry formats this means the first byte still waits for the full decode, but the API shape is uniform.

## Compression methods

| Method | Read | Write | Container | Notes |
|--------|------|-------|-----------|-------|
| None (store) | ✅ | ✅ | — | Pass-through |
| Deflate | ✅ | ✅ | ZIP, gzip | `flate2` with `rust_backend` (miniz_oxide) |
| Deflate64 | ✅ | ❌ | ZIP method 9 | Read-only compat |
| Bzip2 | ✅ | ✅ | ZIP, .bz2, 7Z, TAR | `bzip2` → `libbz2-rs-sys` (pure Rust) |
| LZMA (raw) | ✅ | ❌ | ZIP method 14 | `lzma-rust2` |
| LZMA2 | ❌ (raw) | ❌ | 7Z folder, XZ | Not a raw codec; driven by 7Z/XZ readers |
| PPMd | ❌ | ❌ | 7Z folder | Decoder declared, not wired |
| Zstd | ✅ | ✅ | 7Z folder, .zst, TAR | `zstd` (bundled C, static) |
| LZ4 (frame) | ✅ | ❌ | 7Z folder, .lz4, TAR | `lz4_flex` frame decoder only |
| XZ | ✅ | ✅ | .xz, TAR | `lzma-rust2` XZ container reader |
| Gzip | ✅ | ❌ | .gz, TAR | `flate2::MultiGzDecoder` (multi-member) |
| Brotli | ✅ | ❌ | .br, 7Z folder | `brotli` — no magic, trial-decode |
| Z (compress/LZW) | ❌ | ❌ | .Z, TAR | No decoder crate; carried from `unarc-rs` |
| Unknown(u16) | Listing only | ❌ | ZIP | Carried so listing still works |

## Encryption

| Format | Algorithm | Read | Write | Notes |
|--------|-----------|------|-------|-------|
| ZIP | ZipCrypto (traditional) | ❌ Refused | ❌ | |
| ZIP | WinZip-AES (AES-128/192/256) | ❌ Refused | ❌ | Crates declared (`aes`, `aes-gcm`, `sha1`, `hmac`, `pbkdf2`), not wired |
| 7Z | AES-256-SHA256 | ✅ | ❌ | `sevenz-rust2` with `aes256` feature |
| RAR | RAR encryption | Via `rars` | No | Refused until legacy adapter lands |
| ARC | CRC-based "encryption" | Via `unarc-rs` | No | Refused until legacy adapter lands |
| ARJ | ARJ encryption | Via `unarc-rs` | No | Refused until legacy adapter lands |
| ACE | ACE crypto | Via `unarc-rs` | No | Refused until legacy adapter lands |

## Detection

Detection reads signatures, not extensions. The order in `detect.rs` is: 7Z → XZ → Zstd → LZ4 → Brotli → Gzip → Bzip2 → Z → ZIP → TAR → long tail (RAR, LZH, ACE, ARC, Zoo, ARJ, SQ, UCB, UC2, Ha, Hyp, Ice). A file with no recognised signature falls through to ZIP rather than failing, matching the app's historical behaviour.

## Limits (default)

- Max entry size: 4 GiB
- Max total size: 16 GiB
- Max entries: 1,000,000
- Max metadata size: 256 MiB
- Max compression ratio: 100,000:1 (checked after 64 KiB of output)

All are configurable via `OpenOptions::limits` and `WriteOptions::limits`.