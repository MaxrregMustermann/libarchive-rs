//! What the twelve long-tail formats actually do, measured rather than intended.
//!
//! This replaces `formats::long_tail_formats_dispatch_and_open`, which accepted `Ok` and `Err`
//! alike and therefore asserted nothing. Everything below is a **measurement** taken by running this
//! file, and each row is asserted exactly as recorded: when a row says an entry does not extract,
//! the test fails the day it starts extracting, because a table that silently absorbs a regression
//! is worse than no table. Flipping a row is a deliberate act with a commit message that says what
//! changed and what was measured.
//!
//! Each row checks three things, in this order:
//!
//! 1. **detect** — what `libarchive::detect_header_with_name` says the file is, given its first
//!    bytes *and* the name the app already has. Two of the twelve have no bytes to identify them
//!    with: an ICE file starts with its uncompressed length and an ARC file with the DOS
//!    end-of-file marker. Those rows carry `needs_name: true`, and the harness asserts both halves
//!    — that the name is what reaches them, and that the bytes alone still say nothing.
//! 2. **open and list** — `ArchiveReader::open_as` succeeds and lists the recorded entry names.
//! 3. **extract** — every entry is streamed out through the public `Read`, in chunks smaller than
//!    the entry, and the CRC-32 of the bytes that came out is compared with the CRC-32 of a
//!    plaintext file vendored in `tests/data/legacy/plaintext/`. The expected CRC is never written
//!    down as a constant: it is computed from the plaintext at test time, so a test that swaps the
//!    plaintext for something else fails rather than redefining "correct".
//!
//! Reproduce the whole table with:
//!
//! ```text
//! CARGO_TARGET_DIR=/home/user/cargo-target-wren cargo test -p libarchive --offline --test long_tail -- --nocapture
//! ```
//!
//! `long_tail_truth_table` prints every row it walks, measured and recorded side by side, so the
//! table can be re-derived rather than trusted. Where a row says an entry does not extract, it also
//! says why in one line, and the file that says why is not always this crate's fault: several of
//! these failures are the carried `unarc-rs` decoder's, and `long_tail_the_backend_reads_what_our_reader_cannot`
//! separates the two by reading the same archives through `unarc-rs` directly, with nothing between.
//!
//! The fixtures, and where each one came from, are in `tests/data/legacy/PROVENANCE.md`.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use libarchive::{detect_header, detect_header_with_name, ArchiveReader, ErrorKind, Format, OpenOptions};

/// The chunk size extraction is measured in.
///
/// Deliberately not a power of two and not a multiple of anything in a decoder's buffer: if the
/// reader were handing back a whole materialised entry in one go, a chunked read would still pass,
/// so the size only proves that the *interface* streams. That the long tail materialises every
/// entry in memory underneath it is recorded in `SPEC.md`, not hidden here.
const CHUNK: usize = 997;

/// One fixture, and what this crate does with it today.
struct Row {
    format: Format,
    /// Path under `tests/data/legacy/`.
    fixture: &'static str,
    /// The password to open with, when the fixture is encrypted.
    password: Option<&'static str>,
    /// What detection says this file is, for a caller that has its name.
    detected: Format,
    /// Whether only the name can reach it. The bytes of an ARC and of an ICE say nothing that any
    /// signature can compare against, and this row records that rather than pretending otherwise.
    needs_name: bool,
    /// One entry per listed entry, in listing order.
    entries: &'static [Entry],
}

struct Entry {
    name: &'static str,
    what: Outcome,
}

enum Outcome {
    /// Streams exactly this plaintext file.
    Plaintext(&'static str),
    /// Should stream `plaintext` and does not: the read fails. The kind is asserted, the message is
    /// not, because the message comes from `unarc-rs` and changes with it. `plaintext` is `None`
    /// only where nothing on this machine ever got the bytes out of the archive, so there is
    /// nothing to check them against: the multi-volume fixtures, and the encrypted ARJ, whose
    /// password is not published anywhere.
    Fails {
        plaintext: Option<&'static str>,
        kind: ErrorKind,
    },
    /// The read succeeds and produces `bytes` instead of `plaintext`, with no error at all.
    Short {
        plaintext: &'static str,
        bytes: usize,
    },
}

/// The measured table. One row per fixture, plus two extra rows for the encrypted ARC, which is
/// the only fixture here with a published password.
const TRUTH: &[Row] = &[
    // --- RAR. The only fixtures unarc-rs ships are the three volumes of one multi-volume archive.
    // The entry `unarc` spans all three, and this crate's reader is handed exactly one source, so
    // there is no way to supply the other two: `formats::legacy::open` builds no volume provider.
    Row {
        format: Format::Rar,
        fixture: "rar/multi/unarc.part1.rar",
        password: None,
        detected: Format::Rar,
        needs_name: false,
        entries: &[Entry {
            name: "unarc",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: None,
            },
        }],
    },
    Row {
        format: Format::Rar,
        fixture: "rar/multi/unarc.part2.rar",
        password: None,
        detected: Format::Rar,
        needs_name: false,
        entries: &[Entry {
            name: "unarc",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: None,
            },
        }],
    },
    Row {
        format: Format::Rar,
        fixture: "rar/multi/unarc.part3.rar",
        password: None,
        detected: Format::Rar,
        needs_name: false,
        entries: &[Entry {
            name: "unarc",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: None,
            },
        }],
    },
    // --- LZH. unarc-rs 0.7.2 ships no LZH fixture at all; these come from oxiarc-lzhuf's copy of
    // the lhasa test archives (PROVENANCE.md). `unified::ArchiveFormat::Lha` reads all five
    // correctly on its own -- see `long_tail_the_backend_reads_what_our_reader_cannot` -- so the
    // failure below is this crate's skip loop, not the LHA decoder. The files carry an SFX stub,
    // which is one reason detection does not see them either.
    Row {
        format: Format::Lzh,
        fixture: "lzh/lha255e_lh5.lzh",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "GPL-2",
            what: Outcome::Fails {
                kind: ErrorKind::Io,
                plaintext: Some("plaintext/lzh-GPL-2.txt"),
            },
        }],
    },
    Row {
        format: Format::Lzh,
        fixture: "lzh/lha_unix114i_h0_lh0.lzh",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "gpl-2.gz",
            what: Outcome::Fails {
                kind: ErrorKind::Io,
                plaintext: Some("plaintext/lzh-gpl-2.gz"),
            },
        }],
    },
    Row {
        format: Format::Lzh,
        fixture: "lzh/lha_unix114i_h0_lh5.lzh",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "gpl-2",
            what: Outcome::Fails {
                kind: ErrorKind::Io,
                plaintext: Some("plaintext/lzh-GPL-2.txt"),
            },
        }],
    },
    Row {
        format: Format::Lzh,
        fixture: "lzh/lha_unix114i_h1_lh5.lzh",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "gpl-2",
            what: Outcome::Fails {
                kind: ErrorKind::Io,
                plaintext: Some("plaintext/lzh-GPL-2.txt"),
            },
        }],
    },
    Row {
        format: Format::Lzh,
        fixture: "lzh/lha_unix114i_h2_lh5.lzh",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "gpl-2",
            what: Outcome::Fails {
                kind: ErrorKind::Io,
                plaintext: Some("plaintext/lzh-GPL-2.txt"),
            },
        }],
    },
    // --- ACE. The two single-volume archives extract byte for byte. The three-volume one cannot:
    // `unarc-rs` wants a `VolumeProvider` for it and this crate supplies none.
    Row {
        format: Format::Ace,
        fixture: "ace/license1.ace",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Plaintext("plaintext/license.txt"),
        }],
    },
    Row {
        format: Format::Ace,
        fixture: "ace/license2.ace",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Plaintext("plaintext/license.txt"),
        }],
    },
    Row {
        format: Format::Ace,
        fixture: "ace/multi/unarc.ace",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "UNARC",
            what: Outcome::Fails {
                kind: ErrorKind::Io,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Ace,
        fixture: "ace/multi/unarc.c00",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "UNARC",
            what: Outcome::Fails {
                kind: ErrorKind::Io,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Ace,
        fixture: "ace/multi/unarc.c01",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "UNARC",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    // --- ARC. `unarc-rs` reads every one of these correctly on its own; the skip loop in
    // `formats::legacy::open` walks the backend past the last entry and leaves it unreadable.
    Row {
        format: Format::Arc,
        fixture: "arc/cpm.arc",
        password: None,
        detected: Format::Arc,
        needs_name: true,
        entries: &[
            Entry {
                name: "DDTZ.COM",
                what: Outcome::Fails {
                    kind: ErrorKind::Malformed,
                    plaintext: Some("plaintext/DDTZ.COM"),
                },
            },
            Entry {
                name: "READ.COM",
                what: Outcome::Fails {
                    kind: ErrorKind::Malformed,
                    plaintext: Some("plaintext/READ.COM"),
                },
            },
        ],
    },
    Row {
        format: Format::Arc,
        fixture: "arc/store.arc",
        password: None,
        detected: Format::Arc,
        needs_name: true,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Arc,
        fixture: "arc/squashed.arc",
        password: None,
        detected: Format::Arc,
        needs_name: true,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Arc,
        fixture: "arc/crunch.arc",
        password: None,
        detected: Format::Arc,
        needs_name: true,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Arc,
        fixture: "arc/crunch2.arc",
        password: None,
        detected: Format::Arc,
        needs_name: true,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    // The stored CRC-16 is wrong in this one on purpose. The carried decoder rejects it, which is
    // the behaviour a corrupt archive should get, and this row is what pins that down.
    Row {
        format: Format::Arc,
        fixture: "arc/wrongcrc16.arc",
        password: None,
        detected: Format::Arc,
        needs_name: true,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    // --- The encrypted ARC, with and without its published password ("SECRET", from
    // unarc-rs's own arc_decompression.rs). Both fail, and they fail differently: without a
    // password this crate never asks the decoder for one at all, because `formats::legacy::open`
    // calls `open_with_format` and drops `OpenOptions::password` on the floor.
    Row {
        format: Format::Arc,
        fixture: "arc/license_cypted.arc",
        password: None,
        detected: Format::Arc,
        needs_name: true,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Arc,
        fixture: "arc/license_cypted.arc",
        password: Some("SECRET"),
        detected: Format::Arc,
        needs_name: true,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    // --- Zoo. Three of four extract byte for byte; the fourth has a deliberately wrong CRC-16 and
    // is rejected, which is the point of having it.
    Row {
        format: Format::Zoo,
        fixture: "zoo/store.zoo",
        password: None,
        detected: Format::Zoo,
        needs_name: false,
        entries: &[Entry {
            name: "license",
            what: Outcome::Plaintext("plaintext/license.txt"),
        }],
    },
    Row {
        format: Format::Zoo,
        fixture: "zoo/default.zoo",
        password: None,
        detected: Format::Zoo,
        needs_name: false,
        entries: &[Entry {
            name: "license",
            what: Outcome::Plaintext("plaintext/license.txt"),
        }],
    },
    Row {
        format: Format::Zoo,
        fixture: "zoo/high_per.zoo",
        password: None,
        detected: Format::Zoo,
        needs_name: false,
        entries: &[Entry {
            name: "license",
            what: Outcome::Plaintext("plaintext/license.txt"),
        }],
    },
    Row {
        format: Format::Zoo,
        fixture: "zoo/wrongcrc16.zoo",
        password: None,
        detected: Format::Zoo,
        needs_name: false,
        entries: &[Entry {
            name: "license",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    // --- ARJ. `unarc-rs` reads all four method levels plus the stored one on its own; our skip
    // loop is what breaks them.
    Row {
        format: Format::Arj,
        fixture: "arj/stored.arj",
        password: None,
        detected: Format::Arj,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Arj,
        fixture: "arj/method1.arj",
        password: None,
        detected: Format::Arj,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Arj,
        fixture: "arj/method2.arj",
        password: None,
        detected: Format::Arj,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Arj,
        fixture: "arj/method3.arj",
        password: None,
        detected: Format::Arj,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Arj,
        fixture: "arj/method4.arj",
        password: None,
        detected: Format::Arj,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Arj,
        fixture: "arj/wrongcrc32.arj",
        password: None,
        detected: Format::Arj,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    // Encrypted ARJ. No password for this fixture is published anywhere, so the row records what
    // happens without one: the right error, from the carried decoder, rather than garbage bytes.
    Row {
        format: Format::Arj,
        fixture: "arj/license_crypted.arj",
        password: None,
        detected: Format::Arj,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Password,
                plaintext: None,
            },
        }],
    },
    Row {
        format: Format::Arj,
        fixture: "arj/multi/test_file.arj",
        password: None,
        detected: Format::Arj,
        needs_name: false,
        entries: &[Entry {
            name: "TEST.ICY",
            what: Outcome::Fails {
                kind: ErrorKind::Io,
                plaintext: None,
            },
        }],
    },
    Row {
        format: Format::Arj,
        fixture: "arj/multi/test_file.a01",
        password: None,
        detected: Format::Arj,
        needs_name: false,
        entries: &[Entry {
            name: "TEST.ICY",
            what: Outcome::Fails {
                kind: ErrorKind::Io,
                plaintext: None,
            },
        }],
    },
    Row {
        format: Format::Arj,
        fixture: "arj/multi/test_file.a02",
        password: None,
        detected: Format::Arj,
        needs_name: false,
        entries: &[Entry {
            name: "TEST.ICY",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: None,
            },
        }],
    },
    // --- SQ. This is the worst row in the table and the reason the table exists: the entry lists
    // with size 0, and streaming it produces 0 bytes. No error, no warning -- 11,357 bytes of
    // license text silently become an empty file. `unarc-rs` reads both of these correctly.
    Row {
        format: Format::Sq,
        fixture: "sq/license.sq",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "license",
            what: Outcome::Short {
                plaintext: "plaintext/license.txt",
                bytes: 0,
            },
        }],
    },
    Row {
        format: Format::Sq,
        fixture: "sq/license.sq2",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: ".LIC",
            what: Outcome::Short {
                plaintext: "plaintext/license.txt",
                bytes: 0,
            },
        }],
    },
    // --- UCB. No fixture exists: not in unarc-rs 0.7.2, not in any other crate in the cargo
    // cache, not on this machine. Worse, `formats::legacy::open` maps `Format::Ucb` onto
    // unarc-rs's `Z` reader, which is the Unix-compress decoder. There is no UCB decoder in this
    // crate and this row is the only thing saying so.
    //
    // --- UC2. Not a carried format at all: `Format::Uc2` is handled by this crate's own reader in
    // `formats::uc2`, and it extracts all four byte for byte. `detect` calls all four ZIP.
    Row {
        format: Format::Uc2,
        fixture: "uc2/fast.uc2",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Plaintext("plaintext/license.txt"),
        }],
    },
    Row {
        format: Format::Uc2,
        fixture: "uc2/normal.uc2",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Plaintext("plaintext/license.txt"),
        }],
    },
    Row {
        format: Format::Uc2,
        fixture: "uc2/stight.uc2",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Plaintext("plaintext/license.txt"),
        }],
    },
    Row {
        format: Format::Uc2,
        fixture: "uc2/tight.uc2",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Plaintext("plaintext/license.txt"),
        }],
    },
    // --- Ha. `unarc-rs` reads all three on its own; our skip loop is what breaks them.
    Row {
        format: Format::Ha,
        fixture: "ha/copy.ha",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "license",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Ha,
        fixture: "ha/asc.ha",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "license",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Ha,
        fixture: "ha/hsc.ha",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "license",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    // --- Hyp. Same cause. `hyp/invalid.hyp` has a deliberately wrong header checksum; the carried
    // decoder rejects it, and that is the right answer for a corrupt archive.
    Row {
        format: Format::Hyp,
        fixture: "hyp/license.hyp",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "LICENSE",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/license.txt"),
            },
        }],
    },
    Row {
        format: Format::Hyp,
        fixture: "hyp/stored.hyp",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[
            Entry {
                name: "A",
                what: Outcome::Fails {
                    kind: ErrorKind::Malformed,
                    plaintext: Some("plaintext/hyp-a.txt"),
                },
            },
            Entry {
                name: "B",
                what: Outcome::Fails {
                    kind: ErrorKind::Malformed,
                    plaintext: Some("plaintext/hyp-b.txt"),
                },
            },
            Entry {
                name: "C",
                what: Outcome::Fails {
                    kind: ErrorKind::Malformed,
                    plaintext: Some("plaintext/hyp-c.txt"),
                },
            },
        ],
    },
    Row {
        format: Format::Hyp,
        fixture: "hyp/invalid.hyp",
        password: None,
        detected: Format::Zip,
        needs_name: false,
        entries: &[Entry {
            name: "A",
            what: Outcome::Fails {
                kind: ErrorKind::Malformed,
                plaintext: Some("plaintext/hyp-a.txt"),
            },
        }],
    },
    // --- Ice. An Ice archive is an executable wrapper around one LHA stream, so there is one
    // entry and its name is whatever the wrapper called it. This one extracts byte for byte.
    Row {
        format: Format::Ice,
        fixture: "ice/license_lha.ice",
        password: None,
        detected: Format::Ice,
        needs_name: true,
        entries: &[Entry {
            name: "compressed",
            what: Outcome::Plaintext("plaintext/license.txt"),
        }],
    },
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy")
}

/// One archive `unarc-rs` reads correctly when nothing sits between it and the file.
///
/// This is the other half of the table, and it is what makes the table's "why" column worth
/// anything. `formats::legacy::open` lists entries with `next_entry()` + `skip()` and then reads
/// them from the same backend, which for most of these formats leaves the backend sitting past the
/// last entry. These rows prove the fixtures are good and the bytes are reachable: the same
/// archives, decoded by the same `unarc-rs` that this crate carries, produce the vendored
/// plaintext exactly.
///
/// Deliberately excluded: the multi-volume fixtures (they need a `VolumeProvider` this crate has no
/// way to supply), the encrypted ones, and the four archives whose stored checksum is wrong on
/// purpose -- a corrupt archive is supposed to fail here too.
const BACKEND: &[(&str, unarc_rs::unified::ArchiveFormat, &[(&str, &str)])] = &[
    (
        "lzh/lha255e_lh5.lzh",
        unarc_rs::unified::ArchiveFormat::Lha,
        &[("GPL-2", "plaintext/lzh-GPL-2.txt")],
    ),
    (
        "lzh/lha_unix114i_h0_lh0.lzh",
        unarc_rs::unified::ArchiveFormat::Lha,
        &[("gpl-2.gz", "plaintext/lzh-gpl-2.gz")],
    ),
    (
        "lzh/lha_unix114i_h0_lh5.lzh",
        unarc_rs::unified::ArchiveFormat::Lha,
        &[("gpl-2", "plaintext/lzh-GPL-2.txt")],
    ),
    (
        "lzh/lha_unix114i_h1_lh5.lzh",
        unarc_rs::unified::ArchiveFormat::Lha,
        &[("gpl-2", "plaintext/lzh-GPL-2.txt")],
    ),
    (
        "lzh/lha_unix114i_h2_lh5.lzh",
        unarc_rs::unified::ArchiveFormat::Lha,
        &[("gpl-2", "plaintext/lzh-GPL-2.txt")],
    ),
    (
        "ace/license1.ace",
        unarc_rs::unified::ArchiveFormat::Ace,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "ace/license2.ace",
        unarc_rs::unified::ArchiveFormat::Ace,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "arc/store.arc",
        unarc_rs::unified::ArchiveFormat::Arc,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "arc/squashed.arc",
        unarc_rs::unified::ArchiveFormat::Arc,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "arc/crunch.arc",
        unarc_rs::unified::ArchiveFormat::Arc,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "arc/crunch2.arc",
        unarc_rs::unified::ArchiveFormat::Arc,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "arc/cpm.arc",
        unarc_rs::unified::ArchiveFormat::Arc,
        &[
            ("DDTZ.COM", "plaintext/DDTZ.COM"),
            ("READ.COM", "plaintext/READ.COM"),
        ],
    ),
    (
        "zoo/store.zoo",
        unarc_rs::unified::ArchiveFormat::Zoo,
        &[("license", "plaintext/license.txt")],
    ),
    (
        "zoo/default.zoo",
        unarc_rs::unified::ArchiveFormat::Zoo,
        &[("license", "plaintext/license.txt")],
    ),
    (
        "zoo/high_per.zoo",
        unarc_rs::unified::ArchiveFormat::Zoo,
        &[("license", "plaintext/license.txt")],
    ),
    (
        "arj/stored.arj",
        unarc_rs::unified::ArchiveFormat::Arj,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "arj/method1.arj",
        unarc_rs::unified::ArchiveFormat::Arj,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "arj/method2.arj",
        unarc_rs::unified::ArchiveFormat::Arj,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "arj/method3.arj",
        unarc_rs::unified::ArchiveFormat::Arj,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "arj/method4.arj",
        unarc_rs::unified::ArchiveFormat::Arj,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "sq/license.sq",
        unarc_rs::unified::ArchiveFormat::Sq,
        &[("license", "plaintext/license.txt")],
    ),
    (
        "sq/license.sq2",
        unarc_rs::unified::ArchiveFormat::Sq,
        &[(".LIC", "plaintext/license.txt")],
    ),
    (
        "ha/copy.ha",
        unarc_rs::unified::ArchiveFormat::Ha,
        &[("license", "plaintext/license.txt")],
    ),
    (
        "ha/asc.ha",
        unarc_rs::unified::ArchiveFormat::Ha,
        &[("license", "plaintext/license.txt")],
    ),
    (
        "ha/hsc.ha",
        unarc_rs::unified::ArchiveFormat::Ha,
        &[("license", "plaintext/license.txt")],
    ),
    (
        "hyp/license.hyp",
        unarc_rs::unified::ArchiveFormat::Hyp,
        &[("LICENSE", "plaintext/license.txt")],
    ),
    (
        "hyp/stored.hyp",
        unarc_rs::unified::ArchiveFormat::Hyp,
        &[
            ("A", "plaintext/hyp-a.txt"),
            ("B", "plaintext/hyp-b.txt"),
            ("C", "plaintext/hyp-c.txt"),
        ],
    ),
    (
        "ice/license_lha.ice",
        unarc_rs::unified::ArchiveFormat::Ice,
        &[("compressed", "plaintext/license.txt")],
    ),
];

/// The carried decoder reads these archives correctly; this crate's adapter does not.
///
/// If this test ever fails, `unarc-rs` stopped being able to read a fixture and the rows above that
/// blame this crate's adapter are no longer true. It is the control for the table.
#[test]
fn long_tail_the_backend_reads_what_our_reader_cannot() {
    let mut failures = Vec::new();
    for (fixture, format, expected) in BACKEND {
        let bytes = read_fixture(fixture);
        let mut backend = match unarc_rs::unified::UnifiedArchive::open_with_format(
            Cursor::new(bytes),
            *format,
        ) {
            Ok(backend) => backend,
            Err(error) => {
                failures.push(format!("{fixture}: unarc-rs cannot even open it: {error}"));
                continue;
            }
        };
        for (index, (name, plaintext)) in expected.iter().enumerate() {
            let header = match backend.next_entry() {
                Ok(Some(header)) => header,
                Ok(None) => {
                    failures.push(format!(
                        "{fixture}: unarc-rs listed {index} entries, expected {name} at {index}"
                    ));
                    break;
                }
                Err(error) => {
                    failures.push(format!("{fixture}: unarc-rs entry {index}: {error}"));
                    break;
                }
            };
            if header.name() != *name {
                failures.push(format!(
                    "{fixture}: entry {index} is {:?}, the table says {name:?}",
                    header.name()
                ));
                continue;
            }
            let read = backend.read(&header).unwrap_or_else(|error| {
                panic!("{fixture} entry {index}: unarc-rs cannot read it: {error}")
            });
            let expected_bytes = read_fixture(plaintext);
            if crc32(&read) != crc32(&expected_bytes) {
                failures.push(format!(
                    "{fixture} [{index}] {name}: unarc-rs read {} bytes crc {:08x}, {plaintext} is \
                     {} bytes crc {:08x}",
                    read.len(),
                    crc32(&read),
                    expected_bytes.len(),
                    crc32(&expected_bytes)
                ));
            }
        }
        println!(
            "{fixture:34} unarc-rs reads {} entries byte for byte",
            expected.len()
        );
    }
    assert!(
        failures.is_empty(),
        "the carried decoder stopped reading the fixtures:\n{}",
        failures.join("\n")
    );
}

fn read_fixture(relative: &str) -> Vec<u8> {
    let path = fixtures().join(relative);
    fs::read(&path).unwrap_or_else(|error| panic!("fixture {relative} is missing: {error}"))
}

/// CRC-32 of bytes, computed over what came out rather than read from a header.
fn crc32(bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

/// Streams one entry to the end in [`CHUNK`]-sized reads, and reports what came out.
#[derive(Debug)]
enum Streamed {
    Failed(ErrorKind),
    Bytes {
        crc: u32,
        length: usize,
        chunks: usize,
    },
}

fn stream(reader: &mut ArchiveReader<Cursor<Vec<u8>>>, index: usize) -> Streamed {
    let mut stream = match reader.open_entry(index) {
        Ok(stream) => stream,
        Err(error) => return Streamed::Failed(error.kind()),
    };
    let mut hasher = crc32fast::Hasher::new();
    let mut length = 0usize;
    let mut chunks = 0usize;
    let mut buffer = vec![0u8; CHUNK];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                hasher.update(&buffer[..read]);
                length += read;
                chunks += 1;
            }
            Err(_error) => {
                // A failure part way through still counts as a failure, but say how much came out
                // first: a decoder that emits 9,984 correct bytes and then stops is a different
                // defect from one that emits none.
                return Streamed::Failed(if length == 0 {
                    ErrorKind::Io
                } else {
                    ErrorKind::Malformed
                });
            }
        }
    }
    Streamed::Bytes {
        crc: hasher.finalize(),
        length,
        chunks,
    }
}

impl Outcome {
    /// What the row says, in words, for the printed table.
    fn recorded(&self) -> String {
        match self {
            Outcome::Plaintext(file) => format!("extracts {file}"),
            Outcome::Fails { kind, plaintext } => match plaintext {
                Some(file) => format!("should extract {file}, fails: {kind:?}"),
                None => format!("fails: {kind:?}"),
            },
            Outcome::Short { plaintext, bytes } => format!("reads {bytes} bytes, not {plaintext}"),
        }
    }
}

/// Walks the table and asserts every row, printing what it measured as it goes.
#[test]
fn long_tail_truth_table() {
    let mut failures = Vec::new();
    for row in TRUTH {
        let bytes = read_fixture(row.fixture);
        let label = match row.password {
            Some(password) => format!("{} (password {password:?})", row.fixture),
            None => row.fixture.to_owned(),
        };

        // 1. What the file is, for a caller that has both the bytes and the name.
        let name = fixtures().join(row.fixture);
        let detected = detect_header_with_name(&bytes, Some(&name));
        if detected != row.detected {
            failures.push(format!(
                "{label}: detect says {detected:?} with the file's name, the table records {:?}",
                row.detected
            ));
        }

        // And the half that is easy to forget: for the two formats the bytes cannot identify,
        // the name really is all of it. Asserted rather than assumed, because a signature for
        // either would be a guess.
        let by_content = detect_header(&bytes);
        let expected_by_content = if row.needs_name { Format::Zip } else { row.detected };
        if by_content != expected_by_content {
            failures.push(format!(
                "{label}: detect from the bytes alone says {by_content:?}, the table records \
                 {expected_by_content:?}{}",
                if row.needs_name {
                    " -- this format has no signature, so the name is the only way to it"
                } else {
                    " -- this format is reachable from the bytes and must stay so"
                }
            ));
        }

        // 2. Open and list.
        let mut options = OpenOptions::default();
        if let Some(password) = row.password {
            options = options.with_password(password);
        }
        let mut reader = match ArchiveReader::open_as(Cursor::new(bytes), row.format, &options) {
            Ok(reader) => reader,
            Err(error) => {
                failures.push(format!(
                    "{label}: open failed with {:?}: {}",
                    error.kind(),
                    error.message()
                ));
                println!(
                    "{label:34} open FAILED {:?} {}",
                    error.kind(),
                    error.message()
                );
                continue;
            }
        };
        let listed: Vec<String> = reader
            .entries()
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        let expected: Vec<String> = row
            .entries
            .iter()
            .map(|entry| entry.name.to_owned())
            .collect();
        if listed != expected {
            failures.push(format!(
                "{label}: lists {listed:?}, the table records {expected:?}"
            ));
            println!("{label:34} lists {listed:?}, expected {expected:?}");
            continue;
        }

        // 3. Extract each entry, and compare what came out with what should have.
        let mut observed = Vec::new();
        for (index, entry) in row.entries.iter().enumerate() {
            match stream(&mut reader, index) {
                Streamed::Failed(kind) => {
                    observed.push(format!("{}: fails: {kind:?}", entry.name));
                    match entry.what {
                        Outcome::Fails { kind: recorded, .. } if recorded == kind => {}
                        _ => failures.push(format!(
                            "{label} [{}] {}: fails with {kind:?}, the table records {}",
                            index,
                            entry.name,
                            entry.what.recorded()
                        )),
                    }
                }
                Streamed::Bytes { crc, length, chunks } => match entry.what {
                    Outcome::Plaintext(file) => {
                        let expected_bytes = read_fixture(file);
                        if crc != crc32(&expected_bytes) || length != expected_bytes.len() {
                            failures.push(format!(
                                "{label} [{}] {}: streamed {length} bytes crc {crc:08x}, {file} is \
                                 {} bytes crc {:08x}",
                                index,
                                entry.name,
                                expected_bytes.len(),
                                crc32(&expected_bytes)
                            ));
                        }
                        observed.push(format!(
                            "{}: {} bytes crc {crc:08x} in {chunks} chunks = {file}",
                            entry.name,
                            length
                        ));
                    }
                    Outcome::Short { plaintext, bytes: recorded_length } => {
                        let expected_bytes = read_fixture(plaintext);
                        observed.push(format!(
                            "{}: reads {length} bytes crc {crc:08x}, not {plaintext}",
                            entry.name
                        ));
                        if length != recorded_length {
                            failures.push(format!(
                                "{label} [{}] {}: reads {length} bytes, the table records {recorded_length}",
                                index, entry.name
                            ));
                        } else if length == expected_bytes.len() || crc == crc32(&expected_bytes) {
                            // It now produces the right bytes and is recorded as not doing so.
                            failures.push(format!(
                                "{label} [{}] {}: now produces {plaintext}; the table says it does not",
                                index, entry.name
                            ));
                        }
                    }
                    Outcome::Fails { kind, .. } => failures.push(format!(
                        "{label} [{}] {}: streamed {length} bytes, the table records a {kind:?} failure",
                        index, entry.name
                    )),
                },
            }
        }

        println!(
            "{label:34} detect={detected:?} (bytes alone: {by_content:?}) n={} | {}",
            listed.len(),
            observed.join(" | ")
        );
    }

    assert!(
        failures.is_empty(),
        "{} of {} recorded rows no longer describe the crate.\n\n{}\n\n\
         Run it with `-- --nocapture` to see the measured table. If a row changed because the \
         reader got better, update the table in this file and say so in the commit message; if it \
         changed because the reader got worse, that is the failure this test exists to catch.",
        failures.len(),
        TRUTH.len(),
        failures.join("\n")
    );
}

/// Every format in `formats::legacy::AVAILABLE` has rows, and nothing else does.
///
/// UCB has no fixture anywhere, so it is named here rather than in a row: a format that cannot be
/// measured has to still be visible, or the table quietly stops covering the long tail.
#[test]
fn every_long_tail_format_is_measured_or_explained() {
    let measured: HashSet<Format> = TRUTH.iter().map(|row| row.format).collect();
    let available: HashSet<Format> = libarchive::formats::legacy::AVAILABLE
        .iter()
        .copied()
        .collect();

    let mut unmeasured: Vec<Format> = available.difference(&measured).copied().collect();
    unmeasured.sort_by_key(|format| format!("{format:?}"));
    assert_eq!(
        unmeasured,
        vec![Format::Ucb],
        "every long-tail format needs a row, or a stated reason for having none"
    );

    let mut unlisted: Vec<Format> = measured.difference(&available).copied().collect();
    unlisted.sort_by_key(|format| format!("{format:?}"));
    assert!(
        unlisted.is_empty(),
        "the table measures formats the crate does not offer: {unlisted:?}"
    );
}

/// Every file vendored under `tests/data/legacy/` is used by a row, and every file a row names is
/// there.
///
/// Without this, copying a fixture in and forgetting it is the easiest way for this table to start
/// lying: a fixture nobody measures looks exactly like a fixture that passes.
#[test]
fn every_vendored_fixture_is_used_and_every_referenced_one_exists() {
    let mut on_disk = BTreeSet::new();
    walk(&fixtures(), &fixtures(), &mut on_disk);
    // The provenance document is not a fixture.
    on_disk.remove("PROVENANCE.md");

    let referenced: BTreeSet<String> = TRUTH
        .iter()
        .map(|row| row.fixture.to_owned())
        .chain(TRUTH.iter().flat_map(|row| row.entries.iter()).filter_map(
            |entry| match entry.what {
                Outcome::Plaintext(file) => Some(file.to_owned()),
                Outcome::Short { plaintext, .. } => Some(plaintext.to_owned()),
                Outcome::Fails { plaintext, .. } => plaintext.map(|file| file.to_owned()),
            },
        ))
        .collect();

    let unreferenced: Vec<&String> = on_disk.difference(&referenced).collect();
    assert!(
        unreferenced.is_empty(),
        "fixtures on disk that no row uses: {unreferenced:?}"
    );

    let missing: Vec<&String> = referenced.difference(&on_disk).collect();
    assert!(
        missing.is_empty(),
        "rows that name a fixture that is not there: {missing:?}"
    );
}

fn walk(root: &Path, directory: &Path, out: &mut BTreeSet<String>) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.insert(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}
