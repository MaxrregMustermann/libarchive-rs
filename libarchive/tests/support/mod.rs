//! Shared plumbing for `differential.rs` and `malformed.rs`: the corpus manifest, a reader for
//! our own crate, adapters for the three crates this one replaces, and the comparison that holds
//! them to the same account.
//!
//! Everything here is deliberately boring. The interesting claims live in the tests themselves:
//! this module's job is that "entry 3 differs: name a, crc 1234 != 5678" is the worst diagnostic
//! anybody has to work with.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use libarchive::entry::normalise_name;
use libarchive::{ArchiveReader, Compression, ErrorKind, Format, Limits, OpenOptions};
use serde::Deserialize;

/// One observed entry: what a reader said about one thing inside an archive.
///
/// `crc32` is computed over the bytes actually streamed out, never taken from a header — the
/// point of comparing CRCs is to compare *stored* CRCs (which the writer put in the header)
/// against *computed* ones (which the reader produced), so that a decompressor that produces
/// wrong bytes cannot agree with itself.
#[derive(Clone, Debug)]
pub struct Observed {
    pub name: String,
    pub size: Option<u64>,
    pub crc32: u32,
    pub is_dir: bool,
    pub is_encrypted: bool,
    /// The CRC the archive itself records, when the format has one and the oracle exposes it.
    pub stored_crc: Option<u32>,
    pub contents: Vec<u8>,
}

/// The manifest's record for one archive file.
#[derive(Debug, Deserialize)]
pub struct ManifestFile {
    /// Path relative to the corpus root.
    pub path: String,
    /// How to open it with this crate: a format name, or `"detect"` for auto-detection.
    pub format: String,
    /// `"gate"` records must pass; `"pending"` records assert today's honest `Err` and carry the
    /// owner who has to flip them when their reader lands.
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// The exact command or code that produced the file, so the corpus is reproducible.
    #[serde(default)]
    pub made_with: String,
    #[serde(default)]
    pub expect: Expect,
    /// Ground truth for the entries, taken from the *plaintext* the archive was made from —
    /// present when the generator knew it, `null` for harvested fixtures.
    #[serde(default)]
    pub entries: Option<Vec<ManifestEntry>>,
    /// Which oracle adapters apply: `"unarc"`, `"sevenz"`, `"oxiarc"`.
    #[serde(default)]
    pub oracles: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

fn default_status() -> String {
    "gate".to_owned()
}

#[derive(Debug, Deserialize)]
pub struct Expect {
    pub kind: String,
    #[serde(default)]
    pub error_kind: Option<String>,
}

impl Default for Expect {
    fn default() -> Expect {
        Expect { kind: "ok".to_owned(), error_kind: None }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ManifestEntry {
    pub name: String,
    pub size: u64,
    pub crc32: u32,
    #[serde(default)]
    pub dir: bool,
}

/// One record of the malformed corpus.
#[derive(Debug, Deserialize)]
pub struct MalformedFile {
    pub path: String,
    /// `"truncated"`, `"wrong_magic"`, `"oversized"`, `"corrupt"` or `"traversal"`.
    pub case: String,
    pub format: String,
    /// `"err"`: opening (and, where opening succeeds, reading every entry) must produce an `Err`.
    /// `"open_no_panic"`: whatever happens, nothing may panic — used for path-traversal archives,
    /// which are *valid* archives whose entry names escape the extraction root. Refusing those is
    /// the extraction layer's decision (coord/notes/testing-plan.md §4), not the reader's.
    pub expect: String,
    #[serde(default)]
    pub error_kind: Option<String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub files: Vec<ManifestFile>,
    #[serde(default)]
    pub malformed: Vec<MalformedFile>,
}

/// The corpus root: `libarchive/tests/corpus`.
pub fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

pub fn load_manifest() -> Manifest {
    let path = corpus_root().join("manifest.json");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("the corpus manifest at {path:?} is unreadable: {error}"));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the corpus manifest at {path:?} is not valid JSON: {error}"))
}

/// Maps the manifest's format string to `libarchive`'s `Format`. `None` means auto-detect.
pub fn parse_format(format: &str) -> Option<Format> {
    Some(match format {
        "detect" => return None,
        "zip" => Format::Zip,
        "7z" => Format::SevenZ,
        "tar" => Format::Tar,
        "tar.gz" => Format::CompressedTar(Compression::Gzip),
        "tar.bz2" => Format::CompressedTar(Compression::Bzip2),
        "tar.xz" => Format::CompressedTar(Compression::Xz),
        "tar.zst" => Format::CompressedTar(Compression::Zstd),
        "tar.lz4" => Format::CompressedTar(Compression::Lz4),
        "gz" => Format::SingleStream(Compression::Gzip),
        "bz2" => Format::SingleStream(Compression::Bzip2),
        "xz" => Format::SingleStream(Compression::Xz),
        "zst" => Format::SingleStream(Compression::Zstd),
        "lz4" => Format::SingleStream(Compression::Lz4),
        "br" => Format::SingleStream(Compression::Brotli),
        "z" => Format::SingleStream(Compression::Z),
        "rar" => Format::Rar,
        "lzh" => Format::Lzh,
        "ace" => Format::Ace,
        "arc" => Format::Arc,
        "zoo" => Format::Zoo,
        "arj" => Format::Arj,
        "sq" => Format::Sq,
        "ucb" => Format::Ucb,
        "uc2" => Format::Uc2,
        "ha" => Format::Ha,
        "hyp" => Format::Hyp,
        "ice" => Format::Ice,
        other => panic!("the manifest names a format this harness does not know: {other}"),
    })
}

/// Every format string the manifest may use, in `Format` enum order plus the compressed-TAR and
/// single-stream variants. `every_format_has_a_manifest_record` walks this.
pub fn all_format_strings() -> &'static [(&'static str, Format)] {
    &[
        ("zip", Format::Zip),
        ("7z", Format::SevenZ),
        ("tar", Format::Tar),
        ("tar.gz", Format::CompressedTar(Compression::Gzip)),
        ("tar.bz2", Format::CompressedTar(Compression::Bzip2)),
        ("tar.xz", Format::CompressedTar(Compression::Xz)),
        ("tar.zst", Format::CompressedTar(Compression::Zstd)),
        ("tar.lz4", Format::CompressedTar(Compression::Lz4)),
        ("gz", Format::SingleStream(Compression::Gzip)),
        ("bz2", Format::SingleStream(Compression::Bzip2)),
        ("xz", Format::SingleStream(Compression::Xz)),
        ("zst", Format::SingleStream(Compression::Zstd)),
        ("lz4", Format::SingleStream(Compression::Lz4)),
        ("br", Format::SingleStream(Compression::Brotli)),
        ("z", Format::SingleStream(Compression::Z)),
        ("rar", Format::Rar),
        ("lzh", Format::Lzh),
        ("ace", Format::Ace),
        ("arc", Format::Arc),
        ("zoo", Format::Zoo),
        ("arj", Format::Arj),
        ("sq", Format::Sq),
        ("ucb", Format::Ucb),
        ("uc2", Format::Uc2),
        ("ha", Format::Ha),
        ("hyp", Format::Hyp),
        ("ice", Format::Ice),
    ]
}

/// The error kind names the manifest uses, mapped to `ErrorKind`.
pub fn parse_error_kind(name: &str) -> ErrorKind {
    match name {
        "NotAnArchive" => ErrorKind::NotAnArchive,
        "Malformed" => ErrorKind::Malformed,
        "Password" => ErrorKind::Password,
        "Unsupported" => ErrorKind::Unsupported,
        "LimitExceeded" => ErrorKind::LimitExceeded,
        "Unimplemented" => ErrorKind::Unimplemented,
        "Io" => ErrorKind::Io,
        other => panic!("the manifest names an error kind this harness does not know: {other}"),
    }
}

fn crc(bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

/// Reads an archive with **this** crate, streaming every entry, and reports what it saw.
///
/// `limits` is a parameter because the oversized-header cases must run under a 1 MiB ceiling to
/// prove `LimitExceeded` rather than a 4 GiB success.
pub fn read_ours(
    path: &Path,
    format: &str,
    password: Option<&str>,
    limits: Limits,
) -> Result<Vec<Observed>, String> {
    let file = fs::File::open(path).map_err(|error| format!("opening {path:?}: {error}"))?;
    let mut options = OpenOptions::default().with_limits(limits);
    if let Some(password) = password {
        options = options.with_password(password);
    }
    let opened = match parse_format(format) {
        None => ArchiveReader::open(file, &options),
        Some(format) => ArchiveReader::open_as(file, format, &options),
    };
    let mut reader = opened.map_err(|error| format!("open: {error:?}"))?;
    let mut observed = Vec::with_capacity(reader.len());
    for index in 0..reader.len() {
        let entry = reader
            .entry(index)
            .ok_or_else(|| format!("entry {index} vanished from the list"))?;
        let is_dir = entry.is_directory();
        let is_encrypted = entry.is_encrypted;
        let name = entry.name.clone();
        let size = entry.size;
        let mut contents = Vec::new();
        if !is_dir {
            let mut stream = reader
                .open_entry(index)
                .map_err(|error| format!("open_entry({index}) [{name}]: {error:?}"))?;
            stream
                .read_to_end(&mut contents)
                .map_err(|error| format!("reading entry {index} [{name}]: {error}"))?;
        }
        observed.push(Observed {
            name,
            size,
            crc32: crc(&contents),
            is_dir,
            is_encrypted,
            stored_crc: None,
            contents,
        });
    }
    Ok(observed)
}

/// Reads a whole archive with this crate in one call, used by the malformed suite where the
/// point is "no panic, and an Err where the record promises one".
pub fn slurp_ours(path: &Path, format: &str, password: Option<&str>, limits: Limits) -> Result<(), String> {
    let entries = read_ours(path, format, password, limits)?;
    // Reading is the second half of the contract: a malformed *entry* must Err too, and only
    // streaming every entry gets there. Reading is done inside read_ours.
    let _ = entries;
    Ok(())
}

/// Opens with `unarc-rs` 0.7.2 — the crate the app uses today for the long tail, ZIP, TAR and
/// the single streams it covers.
pub fn read_unarc(path: &Path, format: &str, password: Option<&str>) -> Result<Vec<Observed>, String> {
    use unarc_rs::unified::{ArchiveFormat, ArchiveOptions, UnifiedArchive};
    let unarc_format = match format {
        "zip" => ArchiveFormat::Zip,
        "7z" => ArchiveFormat::SevenZ,
        "tar" => ArchiveFormat::Tar,
        "tar.gz" => ArchiveFormat::Tgz,
        "tar.bz2" => ArchiveFormat::Tbz,
        "z" => ArchiveFormat::Z,
        "gz" => ArchiveFormat::Gz,
        "bz2" => ArchiveFormat::Bz2,
        "rar" => ArchiveFormat::Rar,
        "lzh" => ArchiveFormat::Lha,
        "ace" => ArchiveFormat::Ace,
        "arc" => ArchiveFormat::Arc,
        "zoo" => ArchiveFormat::Zoo,
        "arj" => ArchiveFormat::Arj,
        "sq" => ArchiveFormat::Sq,
        "uc2" => ArchiveFormat::Uc2,
        "ha" => ArchiveFormat::Ha,
        "hyp" => ArchiveFormat::Hyp,
        "ice" => ArchiveFormat::Ice,
        other => return Err(format!("unarc-rs has no mapping for format {other}")),
    };
    let file = fs::File::open(path).map_err(|error| format!("opening {path:?}: {error}"))?;
    let mut options = ArchiveOptions::new();
    if let Some(password) = password {
        options = options.with_password(password);
    }
    let mut archive =
        UnifiedArchive::open_with_format_and_options(BufReader::new(file), unarc_format, options)
            .map_err(|error| format!("unarc open: {error}"))?;
    let entries = archive.entries().map_err(|error| format!("unarc entries: {error}"))?;
    let mut observed = Vec::with_capacity(entries.len());
    for entry in entries {
        let is_dir = entry.is_directory();
        let contents = if is_dir {
            Vec::new()
        } else {
            archive.read(&entry).map_err(|error| {
                format!("unarc read [{}]: {error}", entry.name())
            })?
        };
        let stored = entry.crc();
        observed.push(Observed {
            name: normalise_name(entry.name()),
            size: Some(entry.original_size()),
            crc32: crc(&contents),
            is_dir,
            is_encrypted: entry.is_encrypted(),
            // unarc reports 0 when the format records no CRC, which is what TAR does.
            stored_crc: if stored == 0 { None } else { Some(stored as u32) },
            contents,
        });
    }
    Ok(observed)
}

/// Reads a 7Z with `sevenz-rust2` 0.23.0 (the vendored copy this crate itself builds on).
pub fn read_sevenz(path: &Path, password: Option<&str>) -> Result<Vec<Observed>, String> {
    let password = sevenz_rust2::Password::from_raw(password.unwrap_or("").as_bytes());
    let mut reader = sevenz_rust2::ArchiveReader::open(path, password)
        .map_err(|error| format!("sevenz open: {error}"))?;
    let archive = reader.archive().clone();
    let mut observed = Vec::new();
    for file in archive.files.iter() {
        let name = normalise_name(file.name());
        if file.is_directory() {
            observed.push(Observed {
                name,
                size: Some(0),
                crc32: 0,
                is_dir: true,
                is_encrypted: false,
                stored_crc: None,
                contents: Vec::new(),
            });
            continue;
        }
        if !file.has_stream() {
            // An empty file with no pack stream of its own: sevenz's `read_file` cannot produce
            // bytes for it, and its header CRC still gets compared below via the entry list.
            observed.push(Observed {
                name,
                size: Some(0),
                crc32: 0,
                is_dir: false,
                is_encrypted: false,
                stored_crc: non_zero_crc(file.crc),
                contents: Vec::new(),
            });
            continue;
        }
        let contents = reader
            .read_file(file.name())
            .map_err(|error| format!("sevenz read [{name}]: {error}"))?;
        observed.push(Observed {
            name,
            size: Some(file.size),
            crc32: crc(&contents),
            is_dir: false,
            is_encrypted: false,
            stored_crc: non_zero_crc(file.crc),
            contents,
        });
    }
    Ok(observed)
}

fn non_zero_crc(crc: u64) -> Option<u32> {
    if crc == 0 {
        None
    } else {
        Some(crc as u32)
    }
}

/// Reads with `oxiarc` at 93b17af — the fork whose ZIP/TAR/LZH/XZ readers the app shipped and
/// whose performance fixes this project has to carry forward.
pub fn read_oxiarc(path: &Path, format: &str) -> Result<Vec<Observed>, String> {
    use oxiarc_archive::{BrotliReader, Bzip2Reader, LzhReader, Lz4Reader, TarReader, ZipReader, ZstdReader};
    let file = fs::File::open(path).map_err(|error| format!("opening {path:?}: {error}"))?;
    match format {
        "zip" => {
            let mut reader = ZipReader::new(BufReader::new(file)).map_err(|e| format!("oxiarc zip open: {e}"))?;
            let entries = reader.entries().to_vec();
            let mut observed = Vec::with_capacity(entries.len());
            for entry in entries {
                let is_dir = entry.is_dir();
                let contents = if is_dir {
                    Vec::new()
                } else {
                    reader.extract(&entry).map_err(|e| format!("oxiarc zip extract [{}]: {e}", entry.name))?
                };
                observed.push(Observed {
                    name: normalise_name(&entry.name),
                    size: Some(entry.size),
                    crc32: crc(&contents),
                    is_dir,
                    is_encrypted: false,
                    stored_crc: entry.crc32.filter(|crc| *crc != 0),
                    contents,
                });
            }
            Ok(observed)
        }
        "tar" => {
            let mut reader = TarReader::new(BufReader::new(file)).map_err(|e| format!("oxiarc tar open: {e}"))?;
            let entries = reader.entries().to_vec();
            let mut observed = Vec::with_capacity(entries.len());
            for entry in entries {
                let is_dir = entry.is_dir();
                let contents = if is_dir {
                    Vec::new()
                } else {
                    reader.extract_to_vec(&entry).map_err(|e| format!("oxiarc tar extract [{}]: {e}", entry.name))?
                };
                observed.push(Observed {
                    name: normalise_name(&entry.name),
                    size: Some(entry.size),
                    crc32: crc(&contents),
                    is_dir,
                    is_encrypted: false,
                    stored_crc: None,
                    contents,
                });
            }
            Ok(observed)
        }
        "lzh" => {
            let mut reader = LzhReader::new(BufReader::new(file)).map_err(|e| format!("oxiarc lzh open: {e}"))?;
            let entries = reader.entries();
            let mut observed = Vec::with_capacity(entries.len());
            for entry in entries {
                let is_dir = entry.is_dir();
                let contents = if is_dir {
                    Vec::new()
                } else {
                    reader.extract_to_vec(&entry).map_err(|e| format!("oxiarc lzh extract [{}]: {e}", entry.name))?
                };
                observed.push(Observed {
                    name: normalise_name(&entry.name),
                    size: Some(entry.size),
                    crc32: crc(&contents),
                    is_dir,
                    is_encrypted: false,
                    stored_crc: None,
                    contents,
                });
            }
            Ok(observed)
        }
        "gz" => single_stream(oxiarc_archive::gzip::decompress(&mut BufReader::new(file))),
        "bz2" => single_stream(
            Bzip2Reader::new(BufReader::new(file)).and_then(|mut r| r.decompress()),
        ),
        "xz" => single_stream(oxiarc_archive::xz::decompress(&mut BufReader::new(file))),
        "zst" => single_stream(ZstdReader::new(BufReader::new(file)).and_then(|mut r| r.decompress())),
        "lz4" => single_stream(Lz4Reader::new(BufReader::new(file)).and_then(|mut r| r.decompress())),
        "br" => single_stream(BrotliReader::new(BufReader::new(file)).and_then(|mut r| r.decompress())),
        other => Err(format!("oxiarc has no mapping for format {other}")),
    }
}

fn single_stream<E: std::fmt::Display>(
    result: Result<Vec<u8>, E>,
) -> Result<Vec<Observed>, String> {
    let contents = result.map_err(|error| format!("oxiarc decompress: {error}"))?;
    Ok(vec![Observed {
        // Our single-stream reader names its one entry "content"; the oracle has no name at all,
        // so the harness gives it ours.
        name: "content".to_owned(),
        size: Some(contents.len() as u64),
        crc32: crc(&contents),
        is_dir: false,
        is_encrypted: false,
        stored_crc: None,
        contents,
    }])
}

/// Runs every oracle named in `oracles` for this record and returns, per oracle, either its
/// entries or the reason it does not apply (a format mapping that does not exist, for example).
pub fn run_oracles(
    path: &Path,
    record: &ManifestFile,
) -> Vec<(String, Result<Vec<Observed>, String>)> {
    record
        .oracles
        .iter()
        .map(|oracle| {
            let result = match oracle.as_str() {
                "unarc" => read_unarc(path, &record.format, record.password.as_deref()),
                "sevenz" => read_sevenz(path, record.password.as_deref()),
                "oxiarc" => read_oxiarc(path, &record.format),
                other => Err(format!("unknown oracle {other}")),
            };
            (oracle.clone(), result)
        })
        .collect()
}

/// Compares our observations against the manifest's plaintext ground truth, in index order.
pub fn compare_against_manifest(
    ours: &[Observed],
    expected: &[ManifestEntry],
    label: &str,
) -> Vec<String> {
    let mut failures = Vec::new();
    if ours.len() != expected.len() {
        failures.push(format!(
            "{label}: {} entries here, manifest says {}",
            ours.len(),
            expected.len()
        ));
        return failures;
    }
    for (index, (got, want)) in ours.iter().zip(expected.iter()).enumerate() {
        if got.name != normalise_name(&want.name) {
            failures.push(format!(
                "{label}: entry {index} is named {:?}, manifest says {:?}",
                got.name, want.name
            ));
        }
        if got.is_dir != want.dir {
            failures.push(format!(
                "{label}: entry {index} [{}] dir-ness {} != manifest {}",
                got.name, got.is_dir, want.dir
            ));
        }
        if !want.dir && got.size != Some(want.size) {
            failures.push(format!(
                "{label}: entry {index} [{}] size {:?} != manifest {}",
                got.name, got.size, want.size
            ));
        }
        if !want.dir && got.crc32 != want.crc32 {
            failures.push(format!(
                "{label}: entry {index} [{}] crc {:08x} != manifest {:08x}",
                got.name, got.crc32, want.crc32
            ));
        }
    }
    failures
}

/// Compares our observations against an oracle's, matched **by name** (oracle order is free — 7Z
/// block order is not name order; the manifest alone owns index order).
pub fn compare_against_oracle(ours: &[Observed], theirs: &[Observed], label: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let our_by_name: HashMap<&str, &Observed> =
        ours.iter().map(|entry| (entry.name.as_str(), entry)).collect();
    let their_by_name: HashMap<&str, &Observed> =
        theirs.iter().map(|entry| (entry.name.as_str(), entry)).collect();

    for name in their_by_name.keys() {
        if !our_by_name.contains_key(name) {
            failures.push(format!("{label}: the oracle lists {name:?}, we do not"));
        }
    }
    for (name, mine) in &our_by_name {
        let Some(theirs_entry) = their_by_name.get(name) else {
            // Directories are the known soft spot: some oracle APIs cannot produce them at all
            // (sevenz's `read_file`) or fold them away. Missing on the oracle side is only a
            // failure when the oracle claims the entry has content.
            if !mine.is_dir {
                failures.push(format!("{label}: we list {name:?}, the oracle does not"));
            }
            continue;
        };
        if mine.is_dir != theirs_entry.is_dir {
            failures.push(format!(
                "{label}: {name:?} dir-ness {} != oracle {}",
                mine.is_dir, theirs_entry.is_dir
            ));
            continue;
        }
        if mine.is_dir {
            continue;
        }
        if mine.is_encrypted != theirs_entry.is_encrypted {
            failures.push(format!(
                "{label}: {name:?} encrypted {} != oracle {}",
                mine.is_encrypted, theirs_entry.is_encrypted
            ));
        }
        if mine.contents != theirs_entry.contents {
            let at = mine
                .contents
                .iter()
                .zip(theirs_entry.contents.iter())
                .position(|(a, b)| a != b)
                .map(|index| index.to_string())
                .unwrap_or_else(|| {
                    if mine.contents.len() > theirs_entry.contents.len() {
                        "we have extra bytes".to_owned()
                    } else {
                        "they have extra bytes".to_owned()
                    }
                });
            failures.push(format!(
                "{label}: {name:?} contents differ at {at} (our crc {:08x}, their crc {:08x}; \
                 our {} bytes, their {} bytes)",
                mine.crc32,
                theirs_entry.crc32,
                mine.contents.len(),
                theirs_entry.contents.len()
            ));
            continue;
        }
        if mine.crc32 != theirs_entry.crc32 {
            failures.push(format!(
                "{label}: {name:?} computed crc {:08x} != oracle computed {:08x}",
                mine.crc32, theirs_entry.crc32
            ));
        }
        // The stored CRC is the archive's own claim about its contents. When the format records
        // one and the oracle exposes it, it must equal what we computed by actually streaming the
        // bytes — that is the check a same-bytes differential cannot make on its own.
        if let Some(stored) = theirs_entry.stored_crc {
            if stored != mine.crc32 {
                failures.push(format!(
                    "{label}: {name:?} computed crc {:08x} != the archive's stored crc {stored:08x}",
                    mine.crc32
                ));
            }
        }
        match (mine.size, theirs_entry.size) {
            (Some(mine), Some(theirs)) if mine != theirs => {
                failures.push(format!(
                    "{label}: {name:?} size {mine} != oracle size {theirs}"
                ));
            }
            _ => {}
        }
    }
    failures
}

/// Runs one manifest record end to end: ours against the manifest's ground truth and against every
/// oracle. Returns the failures, empty when the record holds.
pub fn check_record(record: &ManifestFile) -> Vec<String> {
    let label = record.path.clone();
    let path = corpus_root().join(&record.path);
    if !path.exists() {
        return vec![format!("{label}: missing from the corpus")];
    }
    match record.expect.kind.as_str() {
        "error" => {
            let result = read_ours(&path, &record.format, record.password.as_deref(), Limits::default());
            match result {
                Ok(entries) => vec![format!(
                    "{label}: expected an Err, but the archive opened and listed {} entries",
                    entries.len()
                )],
                Err(message) => match &record.expect.error_kind {
                    None => Vec::new(),
                    Some(kind) => {
                        let wanted = parse_error_kind(kind);
                        if error_kind_named(&message, wanted) {
                            Vec::new()
                        } else {
                            vec![format!(
                                "{label}: expected Err({kind}), got: {message}"
                            )]
                        }
                    }
                },
            }
        }
        "ok" => {
            let mut failures = Vec::new();
            let ours = match read_ours(&path, &record.format, record.password.as_deref(), Limits::default())
            {
                Ok(entries) => entries,
                Err(message) => return vec![format!("{label}: {message}")],
            };
            if let Some(expected) = &record.entries {
                failures.extend(compare_against_manifest(&ours, expected, &label));
            }
            for (oracle, result) in run_oracles(&path, record) {
                match result {
                    Ok(theirs) => failures.extend(compare_against_oracle(&ours, &theirs, &label)),
                    // A missing mapping is recorded, not fatal: the manifest's `oracles` list is
                    // meant to name every oracle that *has* a mapping, and a typo in it should
                    // show up here rather than silently skip a comparison.
                    Err(message) => failures.push(format!("{label}: oracle {oracle}: {message}")),
                }
            }
            failures
        }
        other => vec![format!("{label}: manifest expect.kind {other:?} is not ok|error")],
    }
}

/// Whether an error message from `read_ours` carries the expected kind. The message is built with
/// `{error:?}`, which prints the `ErrorKind` variant name.
fn error_kind_named(message: &str, kind: ErrorKind) -> bool {
    let needle = format!("{kind:?}");
    message.contains(&needle)
}

/// Opens and reads an archive under `catch_unwind`, so a single panicking case is reported and the
/// rest of the suite still runs. A panic here would abort the process under `panic = "abort"`, but
/// the test profile unwinds.
pub fn catch_case<F, T>(case: F) -> std::thread::Result<T>
where
    F: FnOnce() -> T + std::panic::UnwindSafe,
{
    std::panic::catch_unwind(case)
}

/// A 1 MiB ceiling, for the oversized-header and bomb cases: small enough that a hostile header is
/// refused before anything is allocated, large enough that no legitimate corpus file trips it.
pub fn tight_limits() -> Limits {
    Limits { max_entry_size: 1024 * 1024, max_total_size: 4 * 1024 * 1024, ..Limits::default() }
}

/// A wall-clock guard used by nothing today and kept for the cases where a hang is the failure
/// mode; flaky timeouts are worse than no timeout, so nothing asserts on it in-process.
#[allow(unused)]
pub fn slow() -> Duration {
    Duration::from_secs(30)
}

/// Reads a corpus file's bytes.
pub fn corpus_bytes(relative: &str) -> Vec<u8> {
    let path = corpus_root().join(relative);
    fs::read(&path).unwrap_or_else(|error| panic!("corpus file {path:?} is unreadable: {error}"))
}

/// Cursor over a corpus file, the shape every reader here takes.
pub fn corpus_cursor(relative: &str) -> Cursor<Vec<u8>> {
    Cursor::new(corpus_bytes(relative))
}
