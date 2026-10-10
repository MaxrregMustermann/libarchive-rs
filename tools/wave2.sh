#!/usr/bin/env bash
# Wave 2: implementation, against a working reference implementation.
#
# Wave 1 found out what we were dealing with. The ZIP and TAR paths are now written and tested,
# so every job below has a working example to copy rather than a document to interpret.
set -euo pipefail
cd /home/user/libarchive-rs
C="python3 tools/collective.py"

new() { $C job new --team "$1" --title "$2" --acceptance "$3" | tail -1; }
assign() { $C job assign "$1" "$2" >/dev/null; echo "$1 -> $2"; }

J1=$(new core "Implement the single-stream compressed formats" "Read and write GZ, BZ2, XZ, Z, LZ4, Zstd and Brotli as single-entry archives, in libarchive/src/formats/single.rs. An entry is named after the file the app would show. Follow the pattern in libarchive/src/formats/zip.rs: detect, list, stream, never materialise. Note that compression::decode already handles Bzip2, Gzip, Brotli, Lz4, Zstd as raw codecs and refuses Xz and Lzma2 because those are containers with their own index; XZ needs the container handling. Tests: each format round-trips, listing names the single entry, a truncated stream is an error and not a panic.")
assign $J1 otis

J2=$(new core "Implement compressed TAR: tar.gz, tar.bz2, tar.xz" "Make Format::CompressedTar read. The compression wraps the whole TAR, so headers are only visible after decompressing from the start: walk it once with formats::tar::walk and hand out byte ranges within the DECOMPRESSED stream. See libarchive/src/formats/tar.rs for the walk and libarchive/src/compression.rs for the codecs. Tests: a TAR written by crate::writer::tar and then gzipped reads back with every entry intact.")
assign $J2 ken

J3=$(new container "Port the long tail from unarc-rs" "Implement RAR, LZH, ACE, ARC, Zoo, ARJ, SQ, UCB, UC2, Ha, Hyp and Ice in libarchive/src/formats/legacy.rs, carrying the parsers across from unarc-rs 0.7.2 rather than rewriting them. Read coord/notes/unarc-map.md first. unarc-rs source is at ~/.cargo/registry/src/index.crates.io-*/unarc-rs-0.7.2. Apache-2.0, so attribution in NOTICE.md is required. Each format gets an EntrySource impl like the one in formats/zip.rs. A format whose parser cannot be carried says so in mail rather than being half-implemented. Tests: a differential test against unarc-rs for at least one format, so the carried parser is known to behave the same.")
assign $J3 mira
$C job assign $J3 nina >/dev/null

J4=$(new qa "Build the differential test harness and the format corpus" "A harness under libarchive/tests/ that opens the same archive with libarchive-rs and with unarc-rs and compares the entry list and every entry contents, byte for byte. A corpus generator for the formats libarchive-rs can write. A truncation sweep that feeds every corpus file cut at every 512-byte boundary and asserts no panic. Follow coord/notes/testing-plan.md.")
assign $J4 tess

J5=$(new bridge "Write the JNI bridge" "Implement libarchive-rs/jni/src/lib.rs: the extern system functions the app calls, the error mapping to Java exceptions, and a handle type that holds an ArchiveReader open across per-file reads. Read coord/notes/bridge-contract.md for the exact signatures the Kotlin side expects. The app is at /home/user/TheFileManager and its current bridge is app/src/main/rust/src/lib.rs. Must never panic across the boundary.")
assign $J5 hank

J6=$(new qa "Fuzz the parsers" "A property test in libarchive/tests/ that mutates and truncates real archives and asserts no panic, no hang and no unbounded memory. proptest is already a dev-dependency. Prioritise the central directory parser and the TAR header walker, which take the most untrusted arithmetic.")
assign $J6 pip

MAIL=/home/user/coord/wave2-brief.txt
cat > "$MAIL" <<'MAILEOF'
Wave 1 is in coord/notes/. Read the note for your team before you start.

Since then the ZIP and TAR paths are written, tested and measured. Read
libarchive/src/formats/zip.rs and libarchive/src/writer/zip.rs before you
write anything: they are the pattern, and the three performance rules are in
their comments and are checked by tests rather than asserted in prose.

The measurements, so you know what "no regression" means:

  ZIP write, 5,000 entries    sink writes 3.00/entry   (was 32, bar 6)
                             allocated 11.6 MiB        (was 1.40 GiB, bar 43.9 MB)
  listing, 3,355 entries     2.1 ms
  first byte of a 64 MiB entry   one block, not the whole entry

Two things I want from everyone.

1. Your work goes on your own branch. Nothing reaches main until I have merged
   it and run the tests, so you cannot break anyone. But a change that does not
   compile is a change I throw away, so run cargo test -p libarchive before you
   say you are done. Use your own CARGO_TARGET_DIR under /home/user.

2. If a job turns out to be wrong, bigger than it looks, or impossible as
   stated, say so in mail and stop. A clear "this needs X first" saves a day; a
   plausible-looking partial implementation costs one. I would much rather have
   four finished formats than twelve that do not open.
MAILEOF

$C mail send --from orchestrator --to ada,grace,ken,mira,vic,tess,ralph,zara,otis,nina,hank,pip,dev \
  --subject "Wave 1 found the answers. The reference implementation is written. Wave 2 is your job." \
  --body-file "$MAIL" >/dev/null
echo "--- wave 2 dispatched ---"
