//! Writing 7Z, and proving the result with a tool that is not this crate.
//!
//! The claim is not "libarchive writes 7Z" but "libarchive writes 7Z that something else can
//! open", so every archive here is round-tripped two ways: read back by this crate, and listed
//! and extracted by the `7z` binary on this machine, which is the tool that wrote upstream's own
//! test corpus. An archive that only this crate can read would prove nothing about the format.
//!
//! `7z` is not guaranteed to exist, so the tool half of each test is skipped when it does not;
//! the crate's own half always runs.

use std::io::{Cursor, Read};
use std::path::Path;
use std::process::Command;

use libarchive::{ArchiveReader, ArchiveWriter, Format, NewEntry, OpenOptions, WriteOptions};

/// Whether the `7z` binary is here to check our work.
fn seven_z() -> bool {
    std::process::Command::new("7z")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Where the fixtures for the `7z` half go.
///
/// `CARGO_TARGET_TMPDIR` is per-test-run and under the build directory, which is what the project
/// requires of scratch space; `/tmp` is a small tmpfs on this machine and is not to be used.
fn scratch(name: &str) -> std::path::PathBuf {
    let base = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(name)
}

/// Writes `entries` into a 7Z and returns the archive's bytes.
fn write(entries: &[(&str, Vec<u8>)], options: &WriteOptions) -> Vec<u8> {
    let sink = Cursor::new(Vec::new());
    let mut writer =
        ArchiveWriter::create(sink, Format::SevenZ, options).expect("a 7Z writer");
    for (name, contents) in entries {
        writer
            .write_entry_from_slice(&NewEntry::file(*name), contents)
            .expect("write an entry");
    }
    writer.finish_into().expect("finish the archive").into_inner()
}

/// Nothing, which is what a directory or a symlink is written from.
fn crate_empty() -> std::io::Empty {
    std::io::empty()
}

/// Reads every entry of `bytes` back as (name, contents).
fn read_back(bytes: Vec<u8>) -> Vec<(String, Vec<u8>)> {
    let mut reader =
        ArchiveReader::open(Cursor::new(bytes), &OpenOptions::default()).expect("read our own 7Z");
    let mut entries = Vec::new();
    for index in 0..reader.len() {
        let name = reader.entry(index).expect("entry").name.clone();
        let declared = reader.entry(index).expect("entry").size;
        let mut contents = Vec::new();
        reader
            .open_entry(index)
            .expect("open an entry")
            .read_to_end(&mut contents)
            .expect("read an entry");
        // The size the directory recorded against the size that came out. A writer that got this
        // wrong would still read back here, because the reader stops at the entry's declared size,
        // so the two are compared rather than assumed.
        assert_eq!(
            declared,
            Some(contents.len() as u64),
            "{name} is listed with size {declared:?} and produced {} bytes",
            contents.len()
        );
        entries.push((name, contents));
    }
    entries
}

/// Both sides sorted by name, because order is not part of what is being compared: names, sizes
/// and contents are. `7z` lists the entries of an archive in the order the *writer* emitted them,
/// and this one emits stream-less entries where the caller put them, so the two walks do not agree
/// on order — only on content.
fn by_name(mut entries: Vec<(String, Vec<u8>)>) -> Vec<(String, Vec<u8>)> {
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

/// How many folders `7z` says the archive has, which is the only authority on whether a 7Z is
/// solid.
///
/// The flag passed to the writer and the folders in the file are different claims, and a fixture
/// that quietly came out as the other one would make both tests pass while measuring nothing.
/// `7z l -slt` reports a `Block` line per entry, so the distinct count is the folder count.
fn folders(path: &Path) -> Option<usize> {
    let output = Command::new("7z").arg("l").arg("-slt").arg(path).output().ok()?;
    let listing = String::from_utf8_lossy(&output.stdout).to_string();
    let mut blocks: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("Block = "))
        .collect();
    blocks.sort_unstable();
    blocks.dedup();
    Some(blocks.len())
}

/// Extracts with the real `7z` into a fresh directory, returning what it found as (name, contents).
///
/// Extracting to a directory rather than splitting stdout is deliberate: `7z x -so` interleaves the
/// entries' bytes on stdout and their names on stderr in a format that changes with the tool's
/// version, and a test that depends on parsing that is testing 7z's pretty-printer. What matters is
/// the archive, so the archive is consumed the way a user would consume it.
fn extract_with_seven_z(path: &Path, target: &Path) -> Option<Vec<(String, Vec<u8>)>> {
    let status = Command::new("7z")
        .arg("x")
        .arg(path)
        .arg(format!("-o{}", target.display()))
        .arg("-y")
        .output()
        .ok()?;
    assert!(status.status.success(), "7z refused the archive: {status:?}");
    let mut entries = Vec::new();
    collect_files(target, target, &mut entries);
    Some(entries)
}

/// Every file under `root`, named relative to it and in sorted order so the comparison does not
/// depend on the order a directory walk happens to produce.
fn collect_files(root: &Path, directory: &Path, into: &mut Vec<(String, Vec<u8>)>) {
    let Ok(read) = std::fs::read_dir(directory) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = read
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_files(root, &path, into);
        } else if let Ok(name) = path.strip_prefix(root) {
            let contents = std::fs::read(&path).unwrap_or_default();
            into.push((name.to_string_lossy().replace('\\', "/"), contents));
        }
    }
}

/// A corpus with enough variety to catch the boring mistakes: text that compresses, an empty
/// entry, a directory, and an entry with a non-trivial name.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let text: Vec<u8> = "libarchive writes 7Z and the 7z tool reads it back\n"
        .repeat(64)
        .into_bytes();
    vec![
        ("readme.txt", text),
        ("empty.txt", Vec::new()),
        ("sub/notes.bin", (0..512).map(|index| index as u8).collect()),
        ("sub/nested", Vec::new()),
    ]
}

/// A non-solid archive round-trips through both readers.
#[test]
fn a_non_solid_seven_z_round_trips_through_this_crate() {
    let bytes = write(&corpus(), &WriteOptions::default());
    let entries = read_back(bytes);
    assert_eq!(entries.len(), 4, "every entry should come back");
    for (name, contents) in &entries {
        let wanted = corpus()
            .iter()
            .find(|(other, _)| other == name)
            .map(|(_, bytes)| bytes.clone())
            .expect("the entry was one we wrote");
        assert_eq!(contents, &wanted, "{name} came back different");
    }
}

/// A solid archive round-trips too, which is the shape that needs the folder held open until the
/// last entry closes.
#[test]
fn a_solid_seven_z_round_trips_through_this_crate() {
    let options = WriteOptions::default().with_solid(true);
    let bytes = write(&corpus(), &options);
    let entries = read_back(bytes);
    assert_eq!(entries.len(), 4, "every entry should come back");
    for (name, contents) in &entries {
        let wanted = corpus()
            .iter()
            .find(|(other, _)| other == name)
            .map(|(_, bytes)| bytes.clone())
            .expect("the entry was one we wrote");
        assert_eq!(contents, &wanted, "{name} came back different");
    }
}

/// The `7z` binary reads what this crate wrote, non-solid.
#[test]
fn seven_z_can_read_a_non_solid_archive_this_crate_wrote() {
    if !seven_z() {
        eprintln!("skipping: 7z is not installed");
        return;
    }
    let bytes = write(&corpus(), &WriteOptions::default());
    let archive = scratch("libarchive-nonsolid.7z");
    let target = scratch("libarchive-nonsolid-out");
    std::fs::write(&archive, &bytes).expect("write the fixture");
    let _ = std::fs::remove_dir_all(&target);
    let from_tool = by_name(extract_with_seven_z(&archive, &target).expect("7z extracts the archive"));
    let from_crate = by_name(read_back(bytes));
    assert_eq!(
        from_tool.len(),
        from_crate.len(),
        "7z extracted a different number of entries: {from_tool:?}"
    );
    for ((tool_name, tool_bytes), (our_name, our_bytes)) in from_tool.iter().zip(from_crate.iter()) {
        assert_eq!(tool_name, our_name, "the names differ");
        assert_eq!(tool_bytes, our_bytes, "{our_name} came back different bytes");
    }
    if let Some(folders) = folders(&archive) {
        assert!(
            folders > 1,
            "the non-solid archive came out as {folders} folder(s), so this test measured a solid one"
        );
    }
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(&target);
}

/// The `7z` binary reads what this crate wrote, solid.
#[test]
fn seven_z_can_read_a_solid_archive_this_crate_wrote() {
    if !seven_z() {
        eprintln!("skipping: 7z is not installed");
        return;
    }
    let options = WriteOptions::default().with_solid(true);
    let bytes = write(&corpus(), &options);
    let archive = scratch("libarchive-solid.7z");
    let target = scratch("libarchive-solid-out");
    std::fs::write(&archive, &bytes).expect("write the fixture");
    let _ = std::fs::remove_dir_all(&target);
    let from_tool = by_name(extract_with_seven_z(&archive, &target).expect("7z extracts the archive"));
    let from_crate = by_name(read_back(bytes));
    assert_eq!(
        from_tool.len(),
        from_crate.len(),
        "7z extracted a different number of entries: {from_tool:?}"
    );
    for ((tool_name, tool_bytes), (our_name, our_bytes)) in from_tool.iter().zip(from_crate.iter()) {
        assert_eq!(tool_name, our_name, "the names differ");
        assert_eq!(tool_bytes, our_bytes, "{our_name} came back different bytes");
    }
    if let Some(folders) = folders(&archive) {
        assert_eq!(
            folders, 1,
            "the solid archive came out as {folders} folders, so this test measured a non-solid one"
        );
    }
}

/// A directory entry round-trips, which is the shape a file manager writes when it compresses a
/// folder rather than a file list.
///
/// It exercises a different path in the vendored writer: `push_archive_entry` decides an entry has
/// no stream from the entry itself rather than from the reader, so passing `None` has to be right.
#[test]
fn a_directory_entry_round_trips_as_a_directory() {
    let sink = Cursor::new(Vec::new());
    let mut writer =
        ArchiveWriter::create(sink, Format::SevenZ, &WriteOptions::default()).expect("a writer");
    writer
        .write_entry(&NewEntry::directory("folder"), &mut crate_empty())
        .expect("write a directory");
    writer
        .write_entry_from_slice(&NewEntry::file("folder/inside.txt"), b"contents")
        .expect("write a file");
    let bytes = writer.finish_into().expect("finish").into_inner();

    let entries = read_back(bytes.clone());
    let directory = entries
        .iter()
        .find(|(name, _)| name == "folder")
        .expect("the directory should come back");
    assert!(directory.1.is_empty(), "a directory has no contents");
    let inside = entries
        .iter()
        .find(|(name, _)| name == "folder/inside.txt")
        .expect("the file inside it should come back");
    assert_eq!(inside.1, b"contents");

    // And the archive is readable by the tool, which is the claim that matters.
    if !seven_z() {
        return;
    }
    let archive = scratch("libarchive-directory.7z");
    let target = scratch("libarchive-directory-out");
    std::fs::write(&archive, &bytes).expect("write the fixture");
    let _ = std::fs::remove_dir_all(&target);
    let from_tool = by_name(extract_with_seven_z(&archive, &target).expect("7z extracts it"));
    let names: Vec<&str> = from_tool.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names.contains(&"folder/inside.txt"),
        "7z did not find the file inside the directory: {names:?}"
    );
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(&target);
}

/// An archive with no entries at all is still a valid, empty 7Z.
#[test]
fn an_archive_with_no_entries_is_a_valid_empty_seven_z() {
    let sink = Cursor::new(Vec::new());
    let mut writer =
        ArchiveWriter::create(sink, Format::SevenZ, &WriteOptions::default()).expect("a writer");
    let bytes = writer.finish_into().expect("finish").into_inner();
    assert!(!bytes.is_empty(), "an empty 7Z still has a header");
    let entries = read_back(bytes);
    assert!(entries.is_empty(), "and it holds no entries");
}
