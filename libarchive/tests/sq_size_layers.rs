//! J-0038: where does the SQ entry size go, measured at each layer.
//!
//! An SQ archive streams the right bytes but lists the entry as size 0. This test prints the
//! size as seen by every layer on the host, in the order the value travels:
//!
//!   1. the SQ reader (`unarc_rs::sq`) — the header fields the format actually has;
//!   2. the unified entry (`unarc_rs::unified::ArchiveEntry::original_size`);
//!   3. the libarchive entry (`libarchive::Entry::size`) — the exact value `jni/src/lib.rs`
//!      reads at line 213 and turns into the metadata slot at line 279;
//!   4. the bytes actually produced when the entry is streamed.
//!
//! The JNI layer itself (`metadata_array`) needs a JVM and is proved on the emulator by
//! `tools/bridge-proof.sh`; the mapping it performs is `entry.size.unwrap_or(0)`, so the
//! libarchive layer is the last one this test can print directly.
//!
//! Reproduce with:
//!
//! ```text
//! CARGO_TARGET_DIR=/home/user/cargo-target-sable cargo test -p libarchive --offline \
//!     --test sq_size_layers -- --nocapture
//! ```

use std::io::{Cursor, Read};

use libarchive::{ArchiveReader, Format, OpenOptions};

/// The corpus entry, byte-identical to `vendor/unarc-rs/tests/qqq/license.sq` (verified with
/// `cmp`), and to `libarchive/tests/data/legacy/sq/license.sq`.
const SQ_BYTES: &[u8] = include_bytes!("../../vendor/unarc-rs/tests/qqq/license.sq");

/// What the archive's own ground truth says the entry hashes to, and how long the plaintext is.
/// Independently checked: `sha256sum vendor/unarc-rs/LICENSE` prints exactly this digest.
const GROUND_TRUTH_SHA256: &str =
    "c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4";
const PLAINTEXT_LEN: usize = 11_357;

#[test]
fn the_size_at_every_layer() {
    // Layer 1: the SQ reader. The header struct is the whole of what the format records.
    let mut sq_archive = unarc_rs::sq::sq_archive::SqArchive::new(Cursor::new(SQ_BYTES)).unwrap();
    let sq_header = sq_archive.get_next_entry().unwrap().unwrap();
    println!("layer 1, unarc_rs::sq::header::Header");
    println!("  name              = {:?}", sq_header.name);
    println!("  checksum          = {:#06x}", sq_header.checksum);
    println!("  header fields     = name, checksum  (no size field exists in the struct)");
    let sq_streamed = sq_archive.read(&sq_header).unwrap();
    println!("  bytes streamed    = {}", sq_streamed.len());

    // Layer 2: the unified entry, which is what the bridge's backend is built on.
    let mut unified = unarc_rs::unified::UnifiedArchive::open_with_format(
        Cursor::new(SQ_BYTES),
        unarc_rs::unified::ArchiveFormat::Sq,
    )
    .unwrap();
    let unified_entry = unified.next_entry().unwrap().unwrap();
    println!("layer 2, unarc_rs::unified::ArchiveEntry");
    println!("  name              = {:?}", unified_entry.name());
    println!("  original_size()   = {}", unified_entry.original_size());
    println!("  compressed_size() = {}", unified_entry.compressed_size());

    // Layer 3: the libarchive entry — the value `jni/src/lib.rs` reads and maps.
    let reader = ArchiveReader::open_as(Cursor::new(SQ_BYTES), Format::Sq, &OpenOptions::default())
        .unwrap();
    let entries = reader.entries();
    println!("layer 3, libarchive::Entry (what jni/src/lib.rs:213,279 reads)");
    for entry in entries {
        println!("  name              = {:?}", entry.name);
        println!("  size              = {:?}", entry.size);
        println!("  jni metadata slot = \"{}\"", entry.size.unwrap_or(0));
    }

    // Layer 4: the bytes actually produced, checked against ground truth.
    let mut reader = ArchiveReader::open_as(Cursor::new(SQ_BYTES), Format::Sq, &OpenOptions::default())
        .unwrap();
    let mut stream = reader.open_entry(0).unwrap();
    let mut streamed = Vec::new();
    stream.read_to_end(&mut streamed).unwrap();
    println!("layer 4, bytes actually streamed through libarchive");
    println!("  streamed bytes    = {}", streamed.len());
    println!("  == plaintext len  = {}", streamed.len() == PLAINTEXT_LEN);
    println!(
        "  == LICENSE bytes  = {}",
        streamed.as_slice() == include_bytes!("../../vendor/unarc-rs/LICENSE")
    );
    println!("  ground truth sha  = {}", GROUND_TRUTH_SHA256);

    // The assertion the acceptance criterion asks for: the reported size equals the bytes
    // actually produced. Before the fix this fails at layer 2 and layer 3 (0 != 11357).
    assert_eq!(
        unified_entry.original_size() as usize,
        streamed.len(),
        "unified original_size must equal the bytes the entry streams"
    );
    assert_eq!(
        entries[0].size,
        Some(streamed.len() as u64),
        "libarchive Entry::size must equal the bytes the entry streams"
    );
}
