# libarchive-rs

Streaming archive reading and writing for Android file managers.

This crate replaces `unarc-rs` 0.7.2, `oxiarc` (fork at `93b17af`), and `sevenz-rust2` 0.23.0 in the [TheFileManager](https://github.com/MaxrregMustermann/TheFileManager) Android app. It covers the formats they covered between them — ZIP, 7Z, TAR and its compressed forms, the single-stream compressors, and the long tail of RAR, LZH, ACE, ARC, Zoo, ARJ, SQ, UCB, UC2, Ha, Hyp and Ice — behind one API with two properties neither of them had.

## The two properties

### Streaming is the default, not an option

[`ArchiveReader::open_entry`](libarchive/src/reader.rs:366) returns a [`std::io::Read`]. It does not materialise the entry. A 4 GiB entry in a 4 GiB archive costs 128 KiB of working memory, and the first byte arrives after one block rather than after the whole file — measured at **121 ns** for the first byte of a 64 MiB stored entry. Nothing in the crate allocates an entry's size.

```rust
use std::fs::File;
use std::io::Read;
use libarchive::{ArchiveReader, OpenOptions};

let file = File::open("archive.7z")?;
let mut reader = ArchiveReader::open(file, &OpenOptions::default())?;
for index in 0..reader.len() {
    let mut entry = reader.open_entry(index)?;
    let mut block = [0u8; 128 * 1024];
    loop {
        let read = entry.read(&mut block)?;
        if read == 0 { break }
        // process block
    }
}
```

### Entries are addressed by index, and the archive stays open

Holding the archive open across reads lets the decoder for a solid 7Z block be reused. In a solid 7Z archive every entry in a block is compressed together, so decoding entry *n* means decoding the whole block from its start. Reading entries one at a time by name therefore costs one decode per entry and is quadratic: `sevenz-rust2`'s `read_file` rebuilds a decode stack per call, so reading *n* entries of a solid block costs *n* decodes. `libarchive-rs` lets the app open by index and hold the archive open, so a solid block is decoded once. Measured on 401 entries of LZMA2 built by `7z`: **1.25 s solid against 1.29 s for the identical data stored as separate blocks** — the shared block costs nothing. `cargo test --release --test sevenz_solid -- --ignored --nocapture`, after `tools/make-corpus.sh`.

## Status

| Area | State |
|------|-------|
| ZIP read/write | ✅ Working, tested |
| 7Z read/write | ✅ Working, tested (solid in-order) |
| TAR read/write | ✅ Working, tested |
| Compressed TAR (.tar.gz, .tar.bz2, .tar.xz, .tar.zst, .tar.lz4) | ✅ Working, tested |
| Single-stream (gz, bz2, xz, zst, br, lz4) | ✅ Working, tested |
| Single-stream `.Z` (Unix compress / LZW) | ❌ Refused — no decoder is wired |
| Long tail (RAR, LZH, ACE, ARC, Zoo, ARJ, SQ, UCB, UC2, Ha, Hyp, Ice) | ❌ Eleven of twelve return `Unsupported`. UC2 opens and reads, but still materialises each entry rather than streaming it — see the module doc for why, and what is left |
| JNI bridge | ❌ Scaffolding only |
| Encrypted ZIP | ❌ Refused |
| Encrypted 7Z (AES-256) | ✅ Working |
| BCJ2 7Z folders | ❌ Refused |
| Compressed TAR (`.tar.gz`, `.tar.bz2`, `.tar.xz`, `.tar.zst`, `.tar.lz4`) | ✅ Working, tested (`.tar.br` refused) |
| Single-stream writing (gz, bz2, xz, zst, br, lz4) | ✅ Working, tested |
| Encrypted ZIP (ZipCrypto and WinZip-AES) | ✅ Reads with a password; writing not implemented |
| `.Z` (Unix compress / LZW) | ✅ Working, tested |
| BCJ2 7Z folders | ✅ Working, tested |
| ZIP write performance (5000 entries) | 169 ms (bar: 109.2 ms — see PERFORMANCE.md) |

See [SPEC.md](SPEC.md) for the full format matrix, [PERFORMANCE.md](libarchive/PERFORMANCE.md) for measured numbers, and [NOTICE.md](NOTICE.md) for attribution.

## Licence

Apache-2.0. The long-tail format readers are derived from `unarc-rs` and the 7Z support from `sevenz-rust2`, both Apache-2.0. See [NOTICE.md](NOTICE.md) for the full attribution.