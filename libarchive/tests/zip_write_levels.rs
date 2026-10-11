//! Writing ZIP at any deflate level, and proving the result with tools that are not this crate.
//!
//! The claim is not "libarchive writes a ZIP" but "a ZIP written at any level opens in someone
//! else's tool", so every archive here is verified with `unzip -t` and `python3 -m zipfile -t`
//! (the two tools the job names) and with `7z t` when it is installed. If fewer than two of the
//! three are available the test fails, because the point is an external round trip: an archive
//! that only this crate can read would prove nothing about the format. Its own read-back is
//! checked too, but as a bonus, not as the evidence.
//!
//! The level policy is asserted as well: [`WriteOptions::deflate_level_for`] picks the fast level
//! below [`DEFLATE_LEVEL_THRESHOLD`] and the best level at or above it, the pinning builder
//! overrides both and clamps to the deflate range, and a pinned level reaches the codec (level 9
//! writes a compressible payload smaller than level 1). That the *size-based* choice reaches the
//! codec is asserted in the writer's own unit tests, which can see the level each encoder was
//! built at.

use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use libarchive::writer::{BEST_DEFLATE_LEVEL, DEFLATE_LEVEL_THRESHOLD, FAST_DEFLATE_LEVEL};
use libarchive::{ArchiveReader, ArchiveWriter, Format, NewEntry, OpenOptions, WriteOptions};

/// Where the archives for the external-tool half go.
///
/// `CARGO_TARGET_TMPDIR` is per-test-run and under the build directory, which is what the project
/// requires of scratch space; `/tmp` is a small tmpfs on this machine and is not to be used.
/// The "levels-" label is so an interrupted run cannot leak a corrupt archive under a name a
/// later test expects to be fresh.
fn scratch(name: &str) -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!("zip-levels-{name}"))
}

/// The bench's payloads, each additionally at a size the policy treats differently.
///
/// The 251-byte-cycle and the low-alphabet "recycled deflater " cycle are the two payloads the
/// benchmark measures; the log-text third is what a real file manager most often writes. All
/// three are compressible, so "deflate" actually compresses something.
fn payload_cycle(index: usize, count: usize, modulus: usize) -> Vec<u8> {
    (0..count)
        .map(|offset| ((index.wrapping_mul(31) + offset) % modulus) as u8)
        .collect()
}

fn payload_simple(index: usize, count: usize) -> Vec<u8> {
    const CYCLE: &[u8] = b"recycled deflater ";
    (0..count).map(|offset| CYCLE[(index + offset) % CYCLE.len()]).collect()
}

fn payload_log(index: usize, count: usize) -> Vec<u8> {
    let mut text = String::new();
    while text.len() < count {
        text.push_str(&format!(
            "2026-10-11 12:00:00 INFO user{} logged in from 10.0.0.{} duration_ms={} status=ok\n",
            index % 97,
            index % 251,
            index % 5000
        ));
    }
    text.truncate(count);
    text.into_bytes()
}

/// The entries every archive here holds: small, huge-small, medium and large, mostly under the
/// size threshold so `unzip -t` and friends exercise the data-descriptor path the app uses.
/// `size` is `None` for the streaming entries (data descriptor) and `Some` for one entry with a
/// plain header, so the round trip covers both header forms.
struct Entry {
    name: String,
    contents: Vec<u8>,
    declare: bool,
}

fn corpus() -> Vec<Entry> {
    let mut entries = Vec::new();
    for index in 0..8 {
        entries.push(Entry {
            name: format!("small{index}.txt"),
            contents: payload_simple(index, 512),
            declare: false,
        });
        entries.push(Entry {
            name: format!("cycle{index}.bin"),
            contents: payload_cycle(index, 512, 251),
            declare: false,
        });
        entries.push(Entry {
            name: format!("log{index}.log"),
            contents: payload_log(index, 2048),
            declare: false,
        });
    }
    entries.push(Entry {
        name: "large.bin".to_owned(),
        contents: payload_cycle(3, 16 * 1024, 251),
        declare: true,
    });
    entries
}

/// Writes every entry through the public streaming API and returns the archive's bytes.
fn write(entries: &[Entry], options: &WriteOptions) -> Vec<u8> {
    let sink = Cursor::new(Vec::new());
    let mut writer = ArchiveWriter::create(sink, Format::Zip, options).expect("a ZIP writer");
    for entry in entries {
        let mut declared = NewEntry::file(entry.name.as_str());
        if entry.declare {
            declared = declared.with_size(entry.contents.len() as u64);
        }
        writer
            .write_entry_with(&declared, &mut &entry.contents[..], options)
            .expect("write an entry");
    }
    writer.finish_into().expect("finish the archive").into_inner()
}

/// Runs every external tester that is installed against `path`, and requires at least two of them.
fn verify_externally(path: &Path) {
    let mut verified = 0u32;
    if let Ok(out) = Command::new("unzip").arg("-t").arg(path).output() {
        assert!(
            out.status.success(),
            "unzip -t refused the archive {}: {out:?}",
            path.display()
        );
        verified += 1;
    }
    let python = Command::new("python3")
        .arg("-m")
        .arg("zipfile")
        .arg("-t")
        .arg(path)
        .output()
        .expect("python3 runs");
    assert!(
        python.status.success(),
        "python3 -m zipfile -t refused the archive {}: {python:?}",
        path.display()
    );
    verified += 1;
    if let Ok(out) = Command::new("7z").arg("t").arg(path).output() {
        assert!(
            out.status.success(),
            "7z t refused the archive {}: {out:?}",
            path.display()
        );
        verified += 1;
    }
    assert!(
        verified >= 2,
        "an archive written here must round-tripped through at least two independent tools, \
         but only {verified} of unzip/python3/7z are installed"
    );
}

/// Reads the archive back with this crate and checks every byte, so the external tools are not
/// the only thing standing between a corrupt writer and a green test.
fn read_back(bytes: &[u8], entries: &[Entry]) {
    let mut reader =
        ArchiveReader::open(Cursor::new(bytes.to_vec()), &OpenOptions::default())
            .expect("our own archive opens");
    assert_eq!(reader.len(), entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let mut got = Vec::new();
        reader
            .open_entry(index)
            .expect("an entry opens")
            .read_to_end(&mut got)
            .expect("an entry reads");
        assert_eq!(got, entry.contents, "entry {} ({})", index, entry.name);
    }
}

#[test]
fn the_level_policy_chooses_by_size_and_respects_a_pinned_level() {
    let by_size = WriteOptions::default();
    assert_eq!(by_size.deflate_level_for(0), FAST_DEFLATE_LEVEL);
    assert_eq!(by_size.deflate_level_for(DEFLATE_LEVEL_THRESHOLD - 1), FAST_DEFLATE_LEVEL);
    assert_eq!(by_size.deflate_level_for(DEFLATE_LEVEL_THRESHOLD), BEST_DEFLATE_LEVEL);
    assert_eq!(by_size.deflate_level_for(1 << 20), BEST_DEFLATE_LEVEL);

    let pinned = WriteOptions::default().with_deflate_level(9);
    assert_eq!(pinned.deflate_level_for(0), 9);
    assert_eq!(pinned.deflate_level_for(1 << 30), 9);
    // Out of the deflate range is clamped rather than passed to the codec, which would refuse it.
    assert_eq!(WriteOptions::default().with_deflate_level(42).deflate_level_for(0), 9);
    assert_eq!(WriteOptions::default().with_deflate_level(0).deflate_level_for(1 << 20), 1);
}

#[test]
fn a_pinned_level_reaches_the_codec_and_compresses_at_it() {
    // Level 9 exists to be smaller than level 1; if two pinned levels produced the same bytes the
    // option would be decorative and the writer would be compressing at whatever level it felt
    // like. The payload is the log text, which is what a realistic archive holds and which deflate
    // compresses more at higher effort.
    let mut entries = Vec::new();
    for index in 0..16 {
        entries.push(Entry {
            name: format!("log{index}.log"),
            contents: payload_log(index, 16 * 1024),
            declare: false,
        });
    }
    let fast = write(&entries, &WriteOptions::default().with_deflate_level(1));
    let best = write(&entries, &WriteOptions::default().with_deflate_level(9));
    assert!(
        best.len() < fast.len(),
        "level 9 wrote {} bytes, no smaller than level 1's {}; the level never reached the codec",
        best.len(),
        fast.len()
    );
    verify_externally(&stage("pinned", &best));
}

/// Writes every entry through the public API's *default* path and returns the archive's bytes.
///
/// `from_slice` picks `write_entry_from_slice` over `write_entry`; both take their options from
/// `create` rather than naming them, which is the path the older test above did not cover.
fn write_via(entries: &[Entry], options: &WriteOptions, from_slice: bool) -> Vec<u8> {
    let sink = Cursor::new(Vec::new());
    let mut writer = ArchiveWriter::create(sink, Format::Zip, options).expect("a ZIP writer");
    for entry in entries {
        let declared = NewEntry::file(entry.name.as_str());
        if from_slice {
            writer
                .write_entry_from_slice(&declared, &entry.contents)
                .expect("write an entry");
        } else {
            writer
                .write_entry(&declared, &mut &entry.contents[..])
                .expect("write an entry");
        }
    }
    writer.finish_into().expect("finish the archive").into_inner()
}

#[test]
fn the_level_given_to_create_reaches_write_entry_and_write_entry_from_slice() {
    // The level was dropped on the path a caller writes by default: `write_entry` built a
    // `WriteOptions::default()` per entry, so pinning level 1 and level 9 at `create` produced
    // byte-identical archives through both of these calls. `a_pinned_level_reaches_the_codec...`
    // above only called `write_entry_with`, which spelled the options out, so it asserted the
    // option on the one path where it worked. Here the options come only from `create`.
    let mut entries = Vec::new();
    for index in 0..16 {
        entries.push(Entry {
            name: format!("log{index}.log"),
            contents: payload_log(index, 16 * 1024),
            declare: false,
        });
    }
    for (from_slice, label) in [(false, "write_entry"), (true, "write_entry_from_slice")] {
        let fast = write_via(
            &entries,
            &WriteOptions::default().with_deflate_level(1),
            from_slice,
        );
        let best = write_via(
            &entries,
            &WriteOptions::default().with_deflate_level(9),
            from_slice,
        );
        assert_ne!(
            fast, best,
            "{label}: level 1 and level 9 wrote byte-identical archives, so the level pinned at \
             create never reached the codec"
        );
        assert!(
            best.len() < fast.len(),
            "{label}: level 9 wrote {} bytes, no smaller than level 1's {}",
            best.len(),
            fast.len()
        );
        verify_externally(&stage(&format!("pinned-default-path-{from_slice}"), &best));
    }
}

/// Writes an archive to disk under the scratch dir for an external test.
fn stage(name: &str, bytes: &[u8]) -> PathBuf {
    let path = scratch(name);
    fs::write(&path, bytes).expect("write the archive for the external tools");
    path
}

#[test]
fn every_level_round_trips_through_unzip_and_python_zipfile() {
    let entries = corpus();
    // 0 is "stored" (no deflate at all); 1, 6 and 9 bracket the policy's two levels.
    for level in [0u32, 1, 6, 9] {
        let options = if level == 0 {
            WriteOptions::stored()
        } else {
            WriteOptions::default().with_deflate_level(level)
        };
        let bytes: Vec<u8> = std::hint::black_box(write(&entries, &options));
        let path = stage(&format!("level-{level}.zip"), &bytes);
        verify_externally(&path);
        read_back(&bytes, &entries);
    }
}

#[test]
fn the_default_policy_writes_small_entries_like_a_fast_deflater() {
    // The default's choice of level 1 for entries under the threshold must actually be the bytes
    // in the archive, not just a value the author believes in. A 512-byte undeclared entry gets
    // level 1 by default; the same entry pinned to level 6 is, on this codec, the same size or
    // bigger, so the default must not be larger.
    let mut entries = Vec::new();
    for index in 0..64 {
        entries.push(Entry {
            name: format!("small{index}.txt"),
            contents: payload_simple(index, 512),
            declare: false,
        });
    }
    let by_size = write(&entries, &WriteOptions::default());
    let at_six = write(&entries, &WriteOptions::default().with_deflate_level(6));
    assert!(
        by_size.len() <= at_six.len(),
        "the default wrote {} bytes for 512-byte entries, more than level 6's {}; \
         either the fast level was not chosen or it is no longer faster here",
        by_size.len(),
        at_six.len()
    );
}