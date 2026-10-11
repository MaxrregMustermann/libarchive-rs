# Format specification

This document lists every format `libarchive-rs` knows, and for each one states what it does and does not do: streaming or whole-entry, encrypted or not, and — for the long tail — what was measured against real archives of each format.

Every claim in the long-tail table below is asserted by `libarchive/tests/long_tail.rs`, which opens each vendored fixture, lists it, streams every entry out, and compares a CRC-32 computed over those bytes with a CRC-32 computed over the plaintext. Reproduce it with:

```
CARGO_TARGET_DIR=/home/user/cargo-target-wren cargo test -p libarchive --offline --test long_tail -- --nocapture
```

The fixtures and where each one came from are in `libarchive/tests/data/legacy/PROVENANCE.md`.

## Legend

- **Streaming**: `open_entry` returns a `Read` that pulls from the decompressor incrementally. No entry is materialised in memory.
- **Whole-entry**: The format architecture requires decompressing the entire entry (or block) before any byte is available. In this crate the entry is still returned as a `Read`, but the decoder must consume the whole compressed stream before the first output byte.
- **Solid**: Multiple entries share one compressed block. Reading entry *n* requires decoding the block from its start. In-order reads reuse the block decoder; out-of-order reads restart it.
- **Encrypted read**: The format can list encrypted entries and read them when a password is supplied.
- **Encrypted write**: The format can create encrypted archives.
- **Refused**: `open` returns `ErrorKind::Unsupported`.

## Core formats (gate — tested on every merge)

| Format | Read | Write | Streaming | Solid | Encrypted read | Encrypted write | Notes |
|--------|------|-------|-----------|-------|----------------|-----------------|-------|
| ZIP | ✅ | ✅ | ✅ | No | ❌ (refused) | ❌ | Deflate, Deflate64, Bzip2, LZMA, Zstd. Data descriptor (streaming) supported. |
| 7Z | ✅ | ✅ | ✅ | Yes | ✅ (AES-256-SHA256) | ❌ | Reading works: solid folders decoded in order via `BlockCursor`, BCJ2 folders decoded through a coder-graph walk. Writing works by wrapping the vendored `ArchiveWriter`; non-solid by default (one folder per entry), solid on `WriteOptions::solid`. Round-tripped against the `7z` binary in `libarchive/tests/sevenz_write.rs`. |
| TAR | ✅ | ✅ | ✅ | No | N/A | N/A | UStar, GNU, PAX. GNU sparse and PAX extended headers read. |
| Compressed TAR (.tar.gz, .tar.bz2, .tar.xz, .tar.zst, .tar.lz4) | ✅ | ✅ | ✅ | No | N/A | N/A | Reading works and streams: the compression layer is decoded and TAR entries stream inside it. Writing works for gzip, bzip2, xz, zstd and lz4; **`.tar.br` is the one exception** and returns `Unimplemented`. A TAR needs its entry sizes declared up front — the header carries the size and there is no data descriptor to correct it afterwards. |
| SingleStream (gz, bz2, xz, zst, br, lz4) | ✅ | ✅ | ✅ | No | N/A | N/A | One entry by definition. Reading and writing both work for all six; a single-stream file holds exactly one entry and says so rather than accepting a second. |
| SingleStream `.Z` (Unix compress / LZW) | ❌ refused | ❌ | — | No | N/A | N/A | Recognised by `detect` and refused: no LZW decoder is wired. |

**Writing is implemented for ZIP, plain TAR and 7Z.** `writer::what_can_be_written` pins that in
a test, so a document and a test cannot both drift away from the code. The 7Z writer wraps the
vendored `sevenz-rust2` writer rather than reimplementing the container — the same trade the reader
makes — and it is proved by extracting what it wrote with the real `7z` binary, not by reading it
back with this crate alone. Two shapes: the default is one folder per entry, so any entry can be
extracted without decoding the ones before it; `WriteOptions::solid` puts every entry in one
folder, which compresses better and costs the whole archive in memory while it is written. An
encrypted 7Z is refused with `Unimplemented` rather than written without its encryption.

## Long-tail formats (measured, not planned)

These are the twelve formats in `formats::legacy::AVAILABLE`. `unarc-rs` also reads JAR, SQZ and
Pack-Ice; JAR is a solid ZIP and SQZ and Pack-Ice are not in `Format`, so they are not claimed here.

**None of the twelve is refused.** `open` returns a reader for all of them, not
`ErrorKind::Unsupported`. Eleven dispatch to `unarc-rs` through `formats::legacy::open`; UC2 is this
crate's own reader in `formats/uc2.rs`; UCB dispatches to `unarc-rs`'s Unix-compress (`Z`) reader,
which is not a UCB decoder and has no fixture to try it on. What differs between them is what
happens *after* that. The table below is what the crate does with the real archives `unarc-rs` 0.7.2
ships, plus five LZH archives, because it ships no LZH fixture. An earlier version of this document
said "Refused" for all twelve and said encrypted ZIP was refused too; both were wrong, and this
table is what the code does.

| Format | Opens | Lists | Extracts bytes matching the plaintext | Why not, where it does not |
|--------|-------|-------|--------------------------------------|---------------------------|
| RAR | Yes | 1 entry | **No** | The only fixtures are the three volumes of one multi-volume archive, and `open` is handed a single source. `unarc-rs` wants a `VolumeProvider` for the other two and there is no API here to supply one. Concatenating the volumes does not help: still `Io: Multi-volume archive requires a volume provider`. |
| LZH | Yes | 1 entry | **No** — `Io: unsupported compression method` | `unarc-rs`'s LHA decoder reads all five fixtures correctly on its own (asserted by `long_tail_the_backend_reads_what_our_reader_cannot`). The failure is this crate's listing loop, which walks the backend past the entry and then reads from it. |
| ACE | Yes | Yes | **Yes** for the single-volume archives; **No** for the three-volume one (`Io`) | Same volume-provider limitation as RAR. |
| ARC | Yes | Yes | **No** — every entry fails `Malformed: unexpected end of file` | Same listing-loop defect as LZH. `unarc-rs` reads all six plaintext fixtures correctly on its own. |
| Zoo | Yes | Yes | **Yes** for three of four; the fourth is a fixture with a deliberately wrong stored CRC-16 and is correctly refused `Malformed` | — |
| ARJ | Yes | Yes | **No** — every plaintext fixture fails `Malformed: unexpected end of file` | Same listing-loop defect. `unarc-rs` reads all five method levels correctly on its own. The encrypted fixture correctly reports `Password` rather than bytes. |
| SQ | Yes | 1 entry, **with size 0** | **No** — the entry streams **0 bytes** for an 11,357-byte file, with no error at all | Same listing-loop defect, and it fails *silently*: 11,357 bytes of license text become an empty file. This is the worst row in the table. |
| UCB | Not measured | Not measured | Not measured | **There is no UCB decoder in this crate.** No UCB fixture exists in `unarc-rs`, in any other crate in the cargo cache, or on this machine. `formats::legacy::open` maps `Format::Ucb` onto `unarc-rs`'s `Z` reader, which is the Unix-compress decoder. A `.ucb` file will therefore fail to decode rather than decode wrongly, but it is not supported and this table does not pretend otherwise. |
| UC2 | Yes | Yes | **Yes**, all four fixtures, byte for byte | Not a carried format: `Format::Uc2` is this crate's own reader in `formats/uc2.rs`. |
| Ha | Yes | Yes | **No** — every entry fails `Malformed: unexpected end of file` | Same listing-loop defect. `unarc-rs` reads all three correctly on its own. |
| Hyp | Yes | Yes | **No** — every entry fails `Malformed: unexpected end of file` | Same listing-loop defect. `unarc-rs` reads the entries correctly on its own. |
| Ice | Yes | 1 entry | **Yes**, byte for byte | — |

Two things that hold for all twelve, and neither is a per-format choice:

* **Every long-tail entry is materialised in memory before the first byte comes out.**
  `LegacyReader::open_entry` calls `unarc-rs`'s `read`, which returns a `Vec<u8>`, and wraps it in
  a `Cursor`. The API streams and the table above is measured by reading in 997-byte chunks, but the
  allocation happens up front. A 132 MB RAR entry costs 132 MB resident.
* **`OpenOptions::password` is dropped on the floor.** `formats::legacy::open` calls
  `UnifiedArchive::open_with_format` and never passes an `ArchiveOptions`, so no password reaches
  any carried decoder. `arc/license_cypted.arc` is the fixture that shows it: with and without the
  correct password (`SECRET`, published in `unarc-rs`'s own test) it fails identically. That
  archive is refused by `unarc-rs` 0.7.2 itself even with the password set, so it is a broken
  fixture as well as an unwired one — but ARJ and ACE encryption cannot work until the options are
  forwarded.

The listing-loop defect is `formats::legacy::open` listing with `next_entry()` + `skip()` and then
calling `read` on the same backend. `unarc-rs`'s `read` decodes from wherever the backend is
positioned rather than seeking to the entry's data offset, so after the walk every read starts at
the end of the archive.

### Reader types

"Streaming-adjacent" means the format's architecture supports reading entries by index with the
archive held open. "Whole-entry" means the `unarc-rs` parser decompresses the entire entry into a
`Vec<u8>` before returning it — which, as noted above, is in fact what every carried format does
here, whatever its architecture could in principle support.

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
| RAR | RAR encryption | ❌ | ❌ | No encrypted RAR fixture exists to test with, and `OpenOptions::password` is never forwarded to the carried decoder (see the long-tail section), so it cannot work today. |
| ARC | CRC-based "encryption" | ❌ | ❌ | `arc/license_cypted.arc` fails with and without its published password. `unarc-rs` 0.7.2 refuses that fixture even with `SECRET` set, so the fixture is broken as well as the wiring. |
| ARJ | ARJ encryption | ❌ | ❌ | `arj/license_crypted.arj` is listed and correctly reported as `Password` rather than bytes, but no password for it is published anywhere, and none would be forwarded if there were. |
| ACE | ACE crypto | ❌ | ❌ | No encrypted ACE fixture exists, and the password would not reach the decoder. |

## Detection

Detection reads signatures, not extensions. The order in `detect.rs` is: 7Z → XZ → Zstd → LZ4 → Brotli → Gzip → Bzip2 → Z → ZIP → TAR → long tail (RAR, LZH, ACE, ARC, Zoo, ARJ, SQ, UCB, UC2, Ha, Hyp, Ice). A file with no recognised signature falls through to ZIP rather than failing, matching the app's historical behaviour.

**Of the twelve long-tail formats, two are detected.** Measured over every row in the long-tail
table (48 rows, 47 distinct fixtures), `detect_header` returns `Rar` for the three RAR rows and
`Arj` for the ten ARJ rows, and returns `Zip` for the other thirty-five — LZH, ACE, ARC, Zoo, SQ,
UC2, Ha, Hyp and Ice all fall through. So `ArchiveReader::open` (auto-detect) on a Zoo file hands
back a ZIP reader and a ZIP error rather than the right one; `open_as` with a format the caller
already knows is the working path, which is what the table above is measured with.

The reasons are specific, and all of them are in `detect.rs` rather than in the formats:

* ACE writes its `**ACE**` marker at offset 8; the constant is compared at offset 0.
* Zoo writes `ZOO .210`; the constant is `ZOO 200`.
* An SQ file starts with a four-byte prefix, not `SQ\x1a`.
* Hyp's constant is `ZZX\0`; a Hyp file starts `\x1aHP%`. Ice's constant is `\x1f\xa0`; an Ice file starts `\x5d\x2c`.
* ARC, UC2 and Ha have no signature at all.
* The LZH fixtures here carry a 0x1ebb-byte SFX stub in front of the `-lh5-` header, so the `-lh`
  check at offset 0 misses them even though the check itself is right for a bare `.lzh`.

## Limits (default)

- Max entry size: 4 GiB
- Max total size: 16 GiB
- Max entries: 1,000,000
- Max metadata size: 256 MiB
- Max compression ratio: 100,000:1 (checked after 64 KiB of output)

All are configurable via `OpenOptions::limits` and `WriteOptions::limits`.