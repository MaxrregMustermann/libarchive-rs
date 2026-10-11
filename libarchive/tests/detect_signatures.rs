//! What `detect` says about every long-tail format, checked against a real archive of each one.
//!
//! This is the regression test for the fact that made the long tail unreachable. `detect.rs` had
//! signatures for eight of the twelve formats that were written from the published description
//! rather than from the bytes, and not one of the eight matched a real archive: `ArchiveReader::open`
//! handed a Zoo, an ARC, a Ha, a Hyp, an ICE, an SQ, a UC2, an LZH and an ACE to the ZIP reader and
//! the user was told the file was not an archive. Every row below was `Format::Zip` before the
//! signatures were corrected; each constant in `detect.rs` now carries the `unarc-rs` source line it
//! came from.
//!
//! Two of the twelve cannot be identified from their bytes at all and say so in this table:
//!
//! * **ICE** begins with the uncompressed size in little endian. There is no signature to compare.
//! * **ARC** begins with `0x1A`, the DOS end-of-file marker, which appears in files that are not
//!   archives at all. Inventing a stronger pattern would be a guess, and a guess here is how a text
//!   file becomes a Zoo.
//!
//! Both are reachable by the name the app already has, through `detect_header_with_name` and
//! `ArchiveReader::open_named`, and this file tests that both ways: the right name opens the real
//! archive, and `.zip` does not.
//!
//! **UCB has no fixture anywhere** — not in `unarc-rs` 0.7.2, not in any other crate in the cargo
//! cache, not on this machine — and no decoder. It is the one format of the twelve with neither a
//! signature nor a name hint, and that is a decision rather than an omission.
//!
//! Reproduce with:
//!
//! ```text
//! CARGO_TARGET_DIR=/home/user/cargo-target-wren cargo test -p libarchive --offline --test detect_signatures -- --nocapture
//! ```
//!
//! The fixtures, and where each one came from, are in `tests/data/legacy/PROVENANCE.md`.

use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use libarchive::{detect_header, detect_header_with_name, ArchiveReader, Format};

/// One long-tail format and what every vendored fixture of it is detected as.
struct Row {
    format: Format,
    /// The extension that names this format, taken from the fixture's own name.
    extension: &'static str,
    /// Whether the bytes alone identify it. `false` means the name is the only thing that can.
    by_content: bool,
    /// Every vendored fixture of this format. All of them must give the same answer, or the
    /// signature is right for one file and wrong for the rest.
    fixtures: &'static [&'static str],
}

/// One row per format in `formats::legacy::AVAILABLE`.
const ROWS: &[Row] = &[
    Row {
        format: Format::Rar,
        extension: "rar",
        by_content: true,
        fixtures: &[
            "rar/multi/unarc.part1.rar",
            "rar/multi/unarc.part2.rar",
            "rar/multi/unarc.part3.rar",
        ],
    },
    Row {
        format: Format::Arj,
        extension: "arj",
        by_content: true,
        fixtures: &[
            "arj/stored.arj",
            "arj/method1.arj",
            "arj/method2.arj",
            "arj/method3.arj",
            "arj/method4.arj",
            "arj/wrongcrc32.arj",
            "arj/license_crypted.arj",
            "arj/multi/test_file.arj",
            "arj/multi/test_file.a01",
            "arj/multi/test_file.a02",
        ],
    },
    Row {
        format: Format::Zoo,
        extension: "zoo",
        by_content: true,
        fixtures: &[
            "zoo/store.zoo",
            "zoo/default.zoo",
            "zoo/high_per.zoo",
            "zoo/wrongcrc16.zoo",
        ],
    },
    Row {
        format: Format::Ha,
        extension: "ha",
        by_content: true,
        fixtures: &["ha/copy.ha", "ha/asc.ha", "ha/hsc.ha"],
    },
    Row {
        format: Format::Hyp,
        extension: "hyp",
        by_content: true,
        fixtures: &["hyp/license.hyp", "hyp/stored.hyp", "hyp/invalid.hyp"],
    },
    Row {
        format: Format::Sq,
        extension: "sq",
        by_content: true,
        fixtures: &["sq/license.sq", "sq/license.sq2"],
    },
    Row {
        format: Format::Uc2,
        extension: "uc2",
        by_content: true,
        fixtures: &[
            "uc2/fast.uc2",
            "uc2/normal.uc2",
            "uc2/stight.uc2",
            "uc2/tight.uc2",
        ],
    },
    Row {
        format: Format::Ace,
        extension: "ace",
        by_content: true,
        fixtures: &[
            "ace/license1.ace",
            "ace/license2.ace",
            "ace/multi/unarc.ace",
            "ace/multi/unarc.c00",
            "ace/multi/unarc.c01",
        ],
    },
    Row {
        format: Format::Lzh,
        extension: "lzh",
        by_content: true,
        fixtures: &[
            "lzh/lha255e_lh5.lzh",
            "lzh/lha_unix114i_h0_lh0.lzh",
            "lzh/lha_unix114i_h0_lh5.lzh",
            "lzh/lha_unix114i_h1_lh5.lzh",
            "lzh/lha_unix114i_h2_lh5.lzh",
        ],
    },
    Row {
        format: Format::Arc,
        extension: "arc",
        // `0x1A` is the DOS end-of-file marker. No signature identifies an ARC and none should.
        by_content: false,
        fixtures: &[
            "arc/store.arc",
            "arc/squashed.arc",
            "arc/crunch.arc",
            "arc/crunch2.arc",
            "arc/cpm.arc",
            "arc/wrongcrc16.arc",
            "arc/license_cypted.arc",
        ],
    },
    Row {
        format: Format::Ice,
        extension: "ice",
        // The first four bytes are the uncompressed size. There is nothing to compare.
        by_content: false,
        fixtures: &["ice/license_lha.ice"],
    },
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy")
}

fn read_fixture(relative: &str) -> Vec<u8> {
    let path = fixtures().join(relative);
    fs::read(&path).unwrap_or_else(|error| panic!("fixture {relative} is missing: {error}"))
}

/// The first twelve bytes of each fixture, printed, so the constant in `detect.rs` can be compared
/// with the bytes rather than taken on trust.
#[test]
fn every_long_tail_fixture_is_detected_as_its_own_format() {
    let mut failures = Vec::new();
    let mut checked = 0;
    for row in ROWS {
        for fixture in row.fixtures {
            let bytes = read_fixture(fixture);
            let detected = detect_header(&bytes);
            let expected = if row.by_content {
                row.format
            } else {
                // No signature can reach these two. The fall-through is what the name is for, and
                // it is asserted rather than assumed.
                Format::Zip
            };
            checked += 1;
            if detected != expected {
                failures.push(format!(
                    "{fixture}: detect says {detected:?}, expected {expected:?}; the file starts {}",
                    bytes[..12.min(bytes.len())]
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
            // And the name settles what the bytes could not, for the extension that names this
            // format. The fixture's own name is used too, because a fixture called `store.arc`
            // has to agree with the extension that identifies it.
            for name in [
                PathBuf::from(format!("archive.{}", row.extension)),
                fixtures().join(fixture),
            ] {
                let named = detect_header_with_name(&bytes, Some(&name));
                if named != row.format {
                    failures.push(format!(
                        "{fixture}: with {name:?} as a hint detect says {named:?}, expected {:?}",
                        row.format
                    ));
                }
            }
            println!(
                "{fixture:28} first 12 {:<36} content={detected:<9?} name={}",
                bytes[..12.min(bytes.len())]
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                detect_header_with_name(&bytes, Some(&fixtures().join(fixture))),
            );
        }
    }
    assert!(
        failures.is_empty(),
        "detect is wrong for:\n{}",
        failures.join("\n")
    );
    println!("checked {checked} fixtures across {} formats", ROWS.len());
}

/// A `.zip` name does not turn an Ice or an ARC into something else.
///
/// The converse of the name hint being useful: because it is only consulted when the bytes say
/// nothing, a file the caller has misnamed keeps the format its name claims rather than being
/// quietly detected as something the bytes never supported.
#[test]
fn a_zip_name_does_not_reach_the_formats_only_the_name_can_reach() {
    for row in ROWS.iter().filter(|row| !row.by_content) {
        for fixture in row.fixtures {
            let bytes = read_fixture(fixture);
            let named = detect_header_with_name(&bytes, Some(Path::new("mislabelled.zip")));
            assert_eq!(
                named,
                Format::Zip,
                "{fixture} named .zip must not become {:?}",
                row.format
            );
        }
    }
}

/// The formats content cannot identify are still reachable end to end, through the name.
///
/// `ice/license_lha.ice` streams the vendored plaintext byte for byte through `ArchiveReader::open`,
/// with the name as the only thing that got the file to the right reader. That is the whole claim
/// of this file: detection used to say `Zip`, the ZIP reader was handed it, and the user was told
/// the file was not an archive.
#[test]
fn an_ice_reachable_by_name_opens_and_streams_the_plaintext() {
    let bytes = read_fixture("ice/license_lha.ice");
    let expected = read_fixture("plaintext/license.txt");

    // An ICE has no signature, and this is why: its first four bytes are the uncompressed size.
    // The file really does start with the length of what comes out of it, which is measured here
    // rather than asserted about 0x1F 0xA0, the constant this file removed.
    let stated = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!(
        stated,
        expected.len(),
        "ICE's first four bytes are its uncompressed size"
    );

    let name = Path::new("license_lha.ice");
    let mut reader = ArchiveReader::open_named(
        Cursor::new(bytes),
        Some(name),
        &libarchive::OpenOptions::default(),
    )
    .expect("an ICE named .ice opens");
    assert_eq!(reader.format(), Format::Ice, "the name is what got it here");
    let listed: Vec<&str> = reader
        .entries()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(listed, ["compressed"]);

    let mut streamed = Vec::new();
    reader
        .open_entry(0)
        .expect("the ICE entry opens")
        .read_to_end(&mut streamed)
        .expect("the ICE entry streams");
    assert_eq!(
        crc32(&streamed),
        crc32(&expected),
        "ICE entry is {} bytes, the vendored plaintext is {}",
        streamed.len(),
        expected.len()
    );

    // The same bytes under a name that says ZIP do not open at all, rather than opening as
    // something that cannot read them.
    let mislabelled = ArchiveReader::open_named(
        Cursor::new(read_fixture("ice/license_lha.ice")),
        Some(Path::new("license_lha.zip")),
        &libarchive::OpenOptions::default(),
    );
    assert!(
        mislabelled.is_err(),
        "an ICE named .zip must not open: it is not a ZIP and says so"
    );
}

/// An ARC is reachable by name too, and this asserts as much as is true today.
///
/// Listing is asserted, byte extraction is not: every ARC entry still fails `Malformed` because
/// `formats::legacy::open` leaves the backend past the last entry before reading it (team legacy,
/// J-0013). What is decided here is that the file reaches the right reader at all — before the name
/// hint, `open` handed it to ZIP and it could not even be listed.
#[test]
fn an_arc_reachable_by_name_opens_and_lists_its_entries() {
    let bytes = read_fixture("arc/cpm.arc");
    let reader = ArchiveReader::open_named(
        Cursor::new(bytes),
        Some(Path::new("cpm.arc")),
        &libarchive::OpenOptions::default(),
    )
    .expect("an ARC named .arc opens");
    assert_eq!(reader.format(), Format::Arc);
    let listed: Vec<&str> = reader
        .entries()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(listed, ["DDTZ.COM", "READ.COM"]);

    let mislabelled = ArchiveReader::open_named(
        Cursor::new(read_fixture("arc/cpm.arc")),
        Some(Path::new("cpm.zip")),
        &libarchive::OpenOptions::default(),
    );
    assert!(mislabelled.is_err(), "an ARC named .zip must not open");
}

/// Every format the crate offers in its long tail is a row here.
///
/// UCB is the exception and it is named rather than quietly missing: there is no UCB decoder in
/// this crate and no fixture of one on this machine, so there is nothing to detect and nothing to
/// assert. `detect.rs` has no UCB signature — the one it had, `UCh\x1a\x00`, matched nothing — and
/// no name hint, because a name that routes to a decoder that cannot read the format only moves
/// the failure. `SPEC.md` says the same thing in prose.
#[test]
fn every_long_tail_format_is_a_row_here_or_is_named_with_why() {
    let measured: std::collections::HashSet<Format> = ROWS.iter().map(|row| row.format).collect();
    let available: std::collections::HashSet<Format> = libarchive::formats::legacy::AVAILABLE
        .iter()
        .copied()
        .collect();

    let mut missing: Vec<Format> = available.difference(&measured).copied().collect();
    missing.sort_by_key(|format| format!("{format:?}"));
    assert_eq!(
        missing,
        vec![Format::Ucb],
        "only UCB, which has no decoder and no fixture"
    );

    // And no name hint smuggles one back in.
    let bytes = read_fixture("ice/license_lha.ice");
    assert_eq!(
        detect_header_with_name(&bytes, Some(Path::new("x.ucb"))),
        Format::Zip
    );
}

/// Every vendored fixture is a row, and every fixture a row names is on disk.
///
/// Without this, adding a fixture and forgetting it looks exactly like a fixture that passes.
#[test]
fn every_vendored_fixture_is_a_row_and_every_referenced_one_exists() {
    let referenced: std::collections::BTreeSet<String> = ROWS
        .iter()
        .flat_map(|row| row.fixtures.iter().copied().map(str::to_owned))
        .collect();
    let mut on_disk = std::collections::BTreeSet::new();
    walk(&fixtures(), &fixtures(), &mut on_disk);
    on_disk.remove("PROVENANCE.md");
    // `plaintext/` is the ground truth the entries are compared against, not an archive. It is
    // walked by `tests/long_tail.rs`, which checks every one of those CRCs.
    on_disk.retain(|path| !path.starts_with("plaintext/"));

    let unreferenced: Vec<&String> = on_disk.difference(&referenced).collect();
    assert!(
        unreferenced.is_empty(),
        "fixtures on disk that no row uses: {unreferenced:?}"
    );
    let missing: Vec<&String> = referenced.difference(&on_disk).collect();
    assert!(
        missing.is_empty(),
        "rows naming a fixture that is not there: {missing:?}"
    );
}

fn walk(root: &Path, directory: &Path, out: &mut std::collections::BTreeSet<String>) {
    for entry in fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.insert(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// CRC-32 of bytes, computed over what came out rather than read from a header.
fn crc32(bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}
