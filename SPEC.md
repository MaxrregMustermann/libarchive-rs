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
crate's own reader in `formats/uc2.rs`. What differs between them is what happens *after* that.

**UCB is the one that is not really read.** It dispatches to `unarc-rs`'s Unix-compress (`Z`)
reader, which is not a UCB decoder and has no UCB fixture to try it on, so a `.ucb` file fails to
decode rather than decoding wrongly — but that is an accident of the wiring, not support. No
signature and no name hint reach it, and this document does not claim it. See the Detection
section. The table below is what the crate does with the real archives `unarc-rs` 0.7.2
ships, plus five LZH archives, because it ships no LZH fixture. An earlier version of this document
said "Refused" for all twelve and said encrypted ZIP was refused too; both were wrong, and this
table is what the code does.

Every row here is measured with `open_as`, which takes the format the caller already knows. Whether a caller *can* know it is a separate question, answered in the Detection section below: ARC and ICE are reachable by name only, and UCB is not reachable at all.

| Format | Opens | Lists | Extracts bytes matching the plaintext | Why not, where it does not |
|--------|-------|-------|--------------------------------------|---------------------------|
| RAR | Yes | 1 entry | **No** | The only fixtures are the three volumes of one multi-volume archive, and `open` is handed a single source. `unarc-rs` wants a `VolumeProvider` for the other two and there is no API here to supply one. Concatenating the volumes does not help: still `Io: Multi-volume archive requires a volume provider`. |
| LZH | Yes | 1 entry | **Yes**, all five fixtures | Fixed by J-0021: the forward-only cursor walks with `read()` not `skip()`. `unarc-rs`'s LHA decoder always worked; the listing loop was the defect. |
| ACE | Yes | Yes | **Yes** for the single-volume archives; **No** for the three-volume one (`Io`) | Same volume-provider limitation as RAR. |
| ARC | Yes | Yes | **Yes** for six plaintext fixtures; **No** for `wrongcrc16.arc` (corrupt, correctly refused `Malformed`) and the two encrypted fixtures (password not forwarded to decoder) | Fixed by J-0021: the forward-only cursor walks with `read()` not `skip()`. The listing-loop defect is resolved for readable fixtures. |
| Zoo | Yes | Yes | **Yes** for three of four; the fourth is a fixture with a deliberately wrong stored CRC-16 and is correctly refused `Malformed` | — |
| ARJ | Yes | Yes | **Yes** for five plaintext fixtures (stored + method1-4); **No** for `wrongcrc32.arj` (corrupt, correctly refused `Malformed`), the encrypted fixture (reports `Password`, no password published), and three multi-volume fixtures (need `VolumeProvider`) | Fixed by J-0021 for plaintext fixtures. The listing-loop defect is resolved for readable fixtures. |
| SQ | Yes | 1 entry | **Yes**, both fixtures extract 11,357 bytes matching plaintext | Fixed by J-0021: the forward-only cursor walks with `read()` not `skip()`, and uses the actual decompressed size rather than the listed size (which was 0). |
| UCB | **No** | — | — | **Not supported, and the row is here rather than deleted so the gap stays visible.** There is no UCB decoder in this crate. No UCB fixture exists in `unarc-rs`, in any other crate in the cargo cache, or on this machine. `formats::legacy::open` maps `Format::Ucb` onto `unarc-rs`'s `Z` reader, which is the Unix-compress decoder. A `.ucb` file will therefore fail to decode rather than decode wrongly, but it is not supported and this table does not pretend otherwise. |
| UC2 | Yes | Yes | **Yes**, all four fixtures, byte for byte | Not a carried format: `Format::Uc2` is this crate's own reader in `formats/uc2.rs`. |
| Ha | Yes | Yes | **Yes**, all three fixtures | Fixed by J-0021: the forward-only cursor walks with `read()` not `skip()`. The listing-loop defect is resolved. |
| Hyp | Yes | Yes | **Yes** for `license.hyp` and `stored.hyp` (3 entries); **No** for `invalid.hyp` (corrupt, correctly refused `Malformed`) | Fixed by J-0021 for valid fixtures. The listing-loop defect is resolved. |
| Ice | Yes | 1 entry | **Yes**, byte for byte | — |

Two things that hold for all twelve, and neither is a per-format choice:

* **Every long-tail entry is materialised in memory before the first byte comes out.**
  `LegacyCursor::open_entry` calls `unarc-rs`'s `read`, which returns a `Vec<u8>`, and wraps it in
  a `Cursor`. The API streams and the table above is measured by reading in 997-byte chunks, but the
  allocation happens up front. A 132 MB RAR entry costs 132 MB resident.
* **`OpenOptions::password` is dropped on the floor.** `formats::legacy::open` calls
  `UnifiedArchive::open_with_format` and never passes an `ArchiveOptions`, so no password reaches
  any carried decoder. `arc/license_cypted.arc` is the fixture that shows it: with and without the
  correct password (`SECRET`, published in `unarc-rs`'s own test) it fails identically. That
  archive is refused by `unarc-rs` 0.7.2 itself even with the password set, so it is a broken
  fixture as well as an unwired one. ARJ encryption is *detected* (the decoder reports `Password`)
  but cannot be *decrypted* without the password being forwarded. ACE encryption likewise cannot work
  until the options are forwarded.

The listing-loop defect (fixed by J-0021) was `formats::legacy::open` listing with `next_entry()` + `skip()` and then calling `read` on the same backend. `unarc-rs`'s `read` decodes from wherever the backend is positioned rather than seeking to the entry's data offset, so after the walk every read started at the end of the archive. The fix is a forward-only cursor that keeps a separate reading backend and advances with `next_entry()` + `read()` (never `skip()`), making sequential extraction O(n) and out-of-order reads correct (O(i) by re-opening and walking forward).

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

Detection reads signatures, not extensions. A file with no recognised signature falls through to ZIP
rather than failing, matching the app's historical behaviour: ZIP is the only format whose central
directory is found by scanning back from the end, which makes it the one that can report a useful
error for a file that is not an archive at all.

`detect` reads [`HEADER_SIZE`](libarchive/src/detect.rs) = 512 bytes and never seeks, so it works on
a pipe. `detect_with_name` takes the name the caller already has and uses it **only after every
signature has failed**, so a name can never override a format the file actually is. `ArchiveReader::open`
is `open_named(source, None, options)`.

### What each of the twelve is reachable by

Every row below is asserted against the vendored fixtures by
`libarchive/tests/detect_signatures.rs`, which prints the first twelve bytes of each fixture and the
format detection returns for it. Reproduce with:

```
CARGO_TARGET_DIR=/home/user/cargo-target-wren cargo test -p libarchive --offline --test detect_signatures -- --nocapture
```

| Format | Reached by | The bytes | The signature is |
|--------|-----------|-----------|------------------|
| RAR | content | `52 61 72 21 1a 07 …` | offset 0 |
| ARJ | content | `60 ea` | offset 0 |
| Zoo | content | `5a 4f 4f 20 32 2e 31 30 20 41 72 63 68 69 76 65 2e` = `ZOO 2.10 Archive.` | offset 0, 17 bytes (`zoo/zoo_header.rs:6`) |
| Ha | content | `48 41` = `HA` | offset 0 (`ha/header.rs:6`) — the weakest here, checked last |
| Hyp | content | `1a 48 50` or `1a 53 54` = 0x1A then `HP` or `ST` | offset 0 (`hyp/header.rs:5-6`) |
| SQ | content | `76 ff` (SQ1) or `fa ff` (SQ2) | offset 0 (`sq/header.rs`) |
| UC2 | content | `55 43 32 1a` = `UC2\x1a` | offset 0, **4** bytes (`uc2/uc2_archive.rs:40`, `ID = 0x1A324355`) |
| ACE | content | `2a 2a 41 43 45 2a 2a` = `**ACE**` | **searched for**, from offset 7 on (`ace/ace_archive.rs:76` does the same over 512 KiB) |
| LZH | content | `2d 6c 68 35 2d` = `-lh5-` | offset **2**, method byte at **5** (`delharc-0.8.0 src/header/compression.rs`) |
| **ARC** | **name only** | `1a 02`, `1a 04`, `1a 08`, `1a 09` | **none.** `0x1A` is the DOS end-of-file marker and appears in files that are not archives. `archive.arc` reaches `Format::Arc`; `archive.zip` reaches neither and does not open. |
| **ICE** | **name only** | `5d 2c 00 00` | **none.** The first four bytes are the uncompressed size — `license_lha.ice` starts `5d 2c 00 00` and what comes out of it is 11,357 = 0x2C5D bytes. `archive.ice` reaches `Format::Ice`; `archive.zip` does not open. |
| **UCB** | **not supported** | — | **No signature, no name hint, no decoder.** `unarc-rs` 0.7.2 has no `ucb` module, and `formats::legacy::open` maps `Format::Ucb` onto its `Z` reader, which is Unix compress (magic `1f 9d`) and cannot read UCB. There is no UCB fixture anywhere on this machine. `detect.rs` used to carry `UCh\x1a\0`, which matched nothing; that constant is gone, and so is the name hint, because routing a format to a decoder that cannot read it only moves the failure. |

**Ten of the twelve were unreachable before this section was written, and the reason was in
`detect.rs`, not in the readers.** Eight of the twelve signatures there were written from the
published description rather than from the bytes and matched no real archive at all — `ZOO 200`,
`SQ\x1a`, `ZZX\0`, `1f a0`, `UCh\x1a\0`, `**ACE**` at offset 0, and `-lh` at offset 0 — and ARC,
Ha and UC2 had no entry whatsoever. `detect_header` returned `Zip` for 35 of the 48 rows in
`tests/long_tail.rs`. Nine formats are now reachable from content and two more by name; UCB is the
only one that is not reachable, and it is not reachable because it cannot be read.

Two claims this document used to make, checked against the fixtures and both wrong:

* "The LZH fixtures here carry a 0x1ebb-byte SFX stub in front of the `-lh5-` header." They do not.
  `1e bb` is a level 1 LHA header's own length and checksum, which is why the method identifier is at
  offset 2 rather than 0. There was never a stub.
* "ACE writes its `**ACE**` marker at offset 8." It is at offset 7. The seven bytes before it are the
  main header's CRC-16 and size, which is also why a marker found earlier than that is refused.

### What is *not* claimed

Detection being right is a separate fact from a format working. Of the nine formats now reachable by
content, `tests/long_tail.rs` still records that only RAR, ACE, Zoo, UC2 and ICE stream bytes
matching their vendored plaintext; ARC, ARJ, Ha, Hyp, LZH and SQ reach the right reader and then
fail on this crate's listing loop. Being detectable is necessary and not sufficient, and the table
above says which is which.

## Limits (default)

- Max entry size: 4 GiB
- Max total size: 16 GiB
- Max entries: 1,000,000
- Max metadata size: 256 MiB
- Max compression ratio: 100,000:1 (checked after 64 KiB of output)

All are configurable via `OpenOptions::limits` and `WriteOptions::limits`.