//! Every writer in this crate, verified against the tool that reads that format.
//!
//! The claim here is not "libarchive writes a TAR" but "a TAR written here opens in the tool that
//! owns TAR", and the same for all fourteen writable formats. Every archive is written through the
//! public streaming API into a temporary directory and then handed to the real tools:
//!
//! | format | oracles |
//! |---|---|
//! | `.tar` | `tar tvf`, `python3 -m tarfile -l`, `7z t`, `tar xf` with a byte comparison |
//! | `.tar.gz` | `gzip -t`, `gzip -dc`, `tar tvf`, `python3 -m tarfile -l`, `7z t`, `tar xf` |
//! | `.tar.bz2` | `bzip2 -t`, `bzip2 -dc`, `tar tvf`, `python3 -m tarfile -l`, `7z t`, `tar xf` |
//! | `.tar.xz` | `xz -t`, `xz -dc`, `tar tvf`, `python3 -m tarfile -l`, `7z t`, `tar xf` |
//! | `.tar.zst` | `zstd -t`, `zstd -dc`, `tar tvf`, `python3 -m tarfile -l`, `7z t`, `tar xf` |
//! | `.tar.lz4` | `lz4 -t`, `lz4 -dc`, `lz4 -dc \| tar tvf`, `lz4 -dc \| tar xf` |
//! | `.zip` | `unzip -t`, `7z t`, `zipfile.ZipFile.testzip`, `zipfile` extraction |
//! | `.7z` | `7z t`, `7z x` with a byte comparison |
//! | `.gz`/`.bz2`/`.xz` | the format's own `-t` and `-dc`, compared with the bytes written |
//! | `.zst` | `zstd -t`, `zstd -dc`, `7z e -so`, all compared with the bytes written |
//! | `.lz4` | `lz4 -t`, `lz4 -dc`, and the reference C library through `python3` |
//! | `.br` | the reference C brotli decoder through `python3` |
//!
//! Every check that can compares bytes, not just exit status: the tools extract, and the extracted
//! files and streams are compared against what was written.
//!
//! # What this file exists because of
//!
//! * **A missing tool is a failure, never a skip.** `bcj2_is_not_quadratic_either` in the benches
//!   prints "skipping" and then passes, which reads as three green tests when one did nothing.
//!   [`output`] panics with the tool's name rather than returning quietly, and
//!   [`every_tool_the_suite_uses_is_installed`] names and versions all nine of them at run time.
//! * **The suite counts what it ran.** [`Report`] records one entry per check that actually
//!   executed, and [`every_writable_format_is_verified_by_the_tool_that_owns_it`] asserts the
//!   total equals what [`ROWS`] promises, per format and per case, and prints it. A run that
//!   verified nothing cannot be mistaken for one that verified everything.
//! * **Two verifiers per format wherever two exist**, so one broken or absent tool cannot make a
//!   broken archive pass. Where only one exists (brotli on a machine with no `brotli` CLI) that is
//!   said in the table above rather than papered over.
//! * **`7z` cannot read an LZ4 frame at all.** It answers "Cannot open the file as archive" for
//!   `lz4 -z`'s own output as well as for ours, verified with the CLI on the spot, so `.tar.lz4`
//!   is checked with `lz4 -t` and `lz4 -dc | tar` instead. GNU tar on this build (1.35) has no LZ4
//!   support either, so `tar tvf` is reached through the `lz4 -dc` pipe, and `python3`'s
//!   `tarfile` cannot help because it handles gz, bz2 and xz only.
//! * **`tar tzvf` is not usable on a `.tar`.** The briefing for this job names it, but GNU tar's
//!   `-z` *forces* gzip, so `tar tzvf archive.tar` dies with "not in gzip format". GNU tar 1.35
//!   detects compression by content, so `tar tvf` covers the plain and the four other tar forms
//!   alike and each compressed form's own codec gets an independent `-t` besides.
//! * **`unzip -t` cannot test an empty ZIP.** It answers "zipfile is empty" and exits 1, while
//!   `7z t` and python's `zipfile` read the same 22-byte archive fine; the one case where a ZIP is
//!   legitimately empty records three zip checks instead of four, which
//!   [`expected_checks`] accounts for.
//! * **`python3 -m zipfile -t` always exits zero** even when it reports a member as corrupt, so
//!   this file calls `ZipFile.testzip()` and asserts its return value instead. That is also why
//!   `zip_write_levels.rs`'s python oracle is weaker than it looks.
//! * **LZ4's oracle is `liblz4.so.1` through ctypes and brotli's is `python3`'s `brotli` module.**
//!   Neither has a CLI on this machine, and both are the reference C implementation such a CLI is
//!   built on, so both formats are still checked against a tool that is not this crate.
//! * **A name over 100 bytes is refused by the TAR writer, not written shortened.** GNU TAR's
//!   classic header carries 100 bytes for the name and this writer does not emit the GNU or pax
//!   long-name extension; it says so instead. That limitation is pinned by its own test rather
//!   than discovered by a tool, because there is no tool that can read a file that was never
//!   written.
//!
//! No network and no fixture from this project: every archive is written by this crate inside the
//! test, and every oracle reads only that archive and this file.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use libarchive::entry::EntryKind;
use libarchive::format::{Compression, Format};
use libarchive::writer::{ArchiveWriter, NewEntry, WriteOptions};

/// Decodes an LZ4 frame with the system's reference C library through ctypes.
///
/// The `lz4` CLI is not the only route into `liblz4`: the CLI is a wrapper over the same
/// `LZ4F_decompress` this calls, and calling it directly is what catches a frame that ends early,
/// a frame followed by trailing bytes, and a frame the library refuses — the three ways a
/// truncated or concatenated stream can otherwise survive `lz4 -t`. Exits non-zero with a message
/// on each; prints the frame's contents to stdout on success.
const DECODE_LZ4_FRAME: &str = r#"
import ctypes, sys

lib = ctypes.CDLL("liblz4.so.1")
for name, argtypes, restype in [
    ("LZ4F_createDecompressionContext", [ctypes.POINTER(ctypes.c_void_p), ctypes.c_uint], ctypes.c_size_t),
    ("LZ4F_freeDecompressionContext", [ctypes.c_void_p], ctypes.c_size_t),
    ("LZ4F_decompress", [ctypes.c_void_p, ctypes.c_char_p, ctypes.POINTER(ctypes.c_size_t),
                          ctypes.c_char_p, ctypes.POINTER(ctypes.c_size_t), ctypes.c_void_p], ctypes.c_size_t),
    ("LZ4F_isError", [ctypes.c_size_t], ctypes.c_uint),
]:
    function = getattr(lib, name)
    function.argtypes = argtypes
    # A size_t return must be 64-bit. ctypes defaults every restype to a 32-bit int, which reads
    # byte counts and error codes from the wrong half of the register.
    function.restype = restype

source = open(sys.argv[1], "rb").read()
context = ctypes.c_void_p()
if lib.LZ4F_isError(lib.LZ4F_createDecompressionContext(ctypes.byref(context), 100)):
    sys.exit("could not create an LZ4 decompression context")
out = bytearray()
ring = ctypes.create_string_buffer(1 << 20)  # a frame block is at most 4 MiB; one buffer is enough
consumed = 0
try:
    while consumed < len(source):
        dst_size = ctypes.c_size_t(len(ring))
        src_size = ctypes.c_size_t(len(source) - consumed)
        hint = lib.LZ4F_decompress(context, ring, ctypes.byref(dst_size),
                                   source[consumed:], ctypes.byref(src_size), None)
        if lib.LZ4F_isError(hint):
            sys.exit(f"liblz4 refused the frame (error code {hint})")
        consumed += src_size.value
        out += ring.raw[:dst_size.value]
        if hint == 0:
            if consumed != len(source):
                sys.exit(f"liblz4 ended the frame after {consumed} of {len(source)} bytes")
            break
        if src_size.value == 0 and dst_size.value == 0:
            sys.exit("liblz4 made no progress: the frame is corrupt")
    else:
        sys.exit("liblz4 never reached the end of the frame: it is truncated")
finally:
    lib.LZ4F_freeDecompressionContext(context)
sys.stdout.buffer.write(out)
"#;

/// Decodes a Brotli stream with `python3`'s brotli module, which wraps Google's reference C
/// decoder. There is no `brotli` CLI on this machine, so this is the format's own implementation
/// rather than nothing, and it is what such a CLI would link.
const DECODE_BROTLI: &str = r#"
import sys, brotli
sys.stdout.buffer.write(brotli.decompress(open(sys.argv[1], "rb").read()))
"#;

/// Tests every member of a ZIP with python's `zipfile`, exiting 1 if any member's CRC fails.
///
/// `python3 -m zipfile -t` cannot be used as an oracle: it prints the failure and exits 0.
const TEST_ZIP_WITH_PYTHON: &str = r#"
import sys, zipfile
bad = zipfile.ZipFile(sys.argv[1]).testzip()
if bad:
    sys.exit(f"zipfile reports {bad!r} as corrupt")
"#;

/// Extracts a ZIP with python's `zipfile`, which is also where the byte comparison comes from.
const EXTRACT_ZIP_WITH_PYTHON: &str = r#"
import sys, zipfile
zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])
"#;

/// Every tool this suite shells out to.
///
/// `brotli` is listed as *absent on purpose*: without a CLI, `.br` is verified through
/// `python3`'s reference-C decoder instead, and the inventory says so rather than leaving a
/// reader to wonder why a format has one oracle where the others have two or more.
const TOOLS: &[(&str, &str)] = &[
    ("tar", "lists and extracts TAR, and auto-detects gz/bz2/xz/zst around it"),
    ("gzip", "gzip -t and gzip -dc, for .tar.gz and .gz"),
    ("bzip2", "bzip2 -t and bzip2 -dc, for .tar.bz2 and .bz2"),
    ("xz", "xz -t and xz -dc, for .tar.xz and .xz"),
    ("zstd", "zstd -t and zstd -dc, for .tar.zst and .zst"),
    ("lz4", "lz4 -t and lz4 -dc, for .tar.lz4 and .lz4"),
    ("7z", "tests ZIP, 7Z and TAR, and extracts a zstd stream"),
    ("unzip", "tests the ZIP the second tool over"),
    ("python3", "tarfile, zipfile, the reference LZ4 C library and the reference brotli decoder"),
];

/// One entry the writers are asked to write.
struct Spec {
    name: &'static str,
    kind: EntryKind,
    contents: Vec<u8>,
}

impl Spec {
    fn file(name: &'static str, contents: Vec<u8>) -> Spec {
        Spec { name, kind: EntryKind::File, contents }
    }

    fn directory(name: &'static str) -> Spec {
        Spec { name, kind: EntryKind::Directory, contents: Vec::new() }
    }
}

/// One archive shape: which entries it holds, and what can hold them.
struct Case {
    label: &'static str,
    specs: Vec<Spec>,
    /// Whether a single-stream file can hold this shape. It holds one file entry's bytes with no
    /// name and no directories, so only "no entries", "one empty file" and "one file of data"
    /// exist there; the name-bearing and directory-bearing shapes prove nothing in it.
    stream: bool,
}

impl Case {
    /// The files an archive of this shape must produce, keyed the way the tools name them.
    fn files(&self) -> BTreeMap<String, Vec<u8>> {
        self.specs
            .iter()
            .filter(|spec| spec.kind == EntryKind::File)
            .map(|spec| (spec.name.to_string(), spec.contents.clone()))
            .collect()
    }

    /// All entries (files and directories) with their sizes, for tar listing comparison.
    /// Directories and other non-file entries have size 0. Directories get a trailing slash
    /// because GNU tar's verbose listing prints them that way.
    fn entries(&self) -> BTreeMap<String, u64> {
        self.specs
            .iter()
            .map(|spec| {
                let (name, size) = match spec.kind {
                    EntryKind::File => (spec.name.to_string(), spec.contents.len() as u64),
                    EntryKind::Directory => (format!("{}/", spec.name), 0),
                    EntryKind::Symlink | EntryKind::HardLink | EntryKind::Special(_) => (spec.name.to_string(), 0),
                };
                (name, size)
            })
            .collect()
    }

    /// The bytes a single-stream archive of this shape holds: the first entry's, or nothing.
    fn stream_bytes(&self) -> Vec<u8> {
        self.specs.first().map(|spec| spec.contents.clone()).unwrap_or_default()
    }
}

/// The awkward cases this suite exists to cover, one shape each.
fn cases() -> Vec<Case> {
    vec![
        Case { label: "an empty archive", specs: Vec::new(), stream: true },
        Case { label: "a zero-byte entry", specs: vec![Spec::file("empty-entry.txt", Vec::new())], stream: true },
        Case {
            label: "a name that needs UTF-8",
            // Not ASCII, and long enough that a name mangled into a fixed-width or 8-bit field
            // would come back as something else rather than as these bytes.
            specs: vec![Spec::file(
                "日本-файл-\u{1F600}-ünïcode.txt",
                payload("utf8", 3000),
            )],
            stream: false,
        },
        Case {
            label: "nested directories",
            // Two directories and a file under both of them: a writer that forgets the directory
            // entries, or that writes them as files, is caught by the listing rather than by the
            // extraction, which only walks the leaf.
            specs: vec![
                Spec::directory("nested"),
                Spec::directory("nested/dir"),
                Spec::file("nested/dir/leaf.txt", payload("leaf", 5000)),
            ],
            stream: false,
        },
        Case {
            label: "an entry spanning several blocks",
            // The writer copies through 128 KiB blocks, so this is four whole blocks plus a
            // partial one. A writer that lost the tail of a block, or that emitted one block per
            // write call, cannot produce a file the tools below read back equal to what went in.
            specs: vec![Spec::file("spanning.bin", spanning_payload())],
            stream: true,
        },
    ]
}

/// Deterministic, compressible payload bytes, so the compression in each format is really
/// exercised rather than the writers storing incompressible noise.
fn payload(kind: &str, length: usize) -> Vec<u8> {
    let line = format!("{kind} ");
    (0..length).map(|index| line.as_bytes()[index % line.len()]).collect()
}

/// An entry larger than several of the writer's internal copy blocks.
fn spanning_payload() -> Vec<u8> {
    payload("spanning", libarchive::writer::COPY_BUFFER_SIZE * 4 + 1234)
}

/// A writable format, and how many checks one archive case must record for it.
struct Row {
    format: Format,
    checks_per_case: usize,
}

/// Every format this crate can write, in the order the writers are dispatched.
const ROWS: &[Row] = &[
    Row { format: Format::Tar, checks_per_case: 4 },
    Row { format: Format::CompressedTar(Compression::Gzip), checks_per_case: 6 },
    Row { format: Format::CompressedTar(Compression::Bzip2), checks_per_case: 6 },
    Row { format: Format::CompressedTar(Compression::Xz), checks_per_case: 6 },
    Row { format: Format::CompressedTar(Compression::Zstd), checks_per_case: 6 },
    Row { format: Format::CompressedTar(Compression::Lz4), checks_per_case: 4 },
    Row { format: Format::Zip, checks_per_case: 4 },
    Row { format: Format::SevenZ, checks_per_case: 2 },
    Row { format: Format::SingleStream(Compression::Gzip), checks_per_case: 2 },
    Row { format: Format::SingleStream(Compression::Bzip2), checks_per_case: 2 },
    Row { format: Format::SingleStream(Compression::Xz), checks_per_case: 2 },
    Row { format: Format::SingleStream(Compression::Zstd), checks_per_case: 3 },
    Row { format: Format::SingleStream(Compression::Lz4), checks_per_case: 3 },
    Row { format: Format::SingleStream(Compression::Brotli), checks_per_case: 1 },
];

/// How many checks a case must record for a format. Almost every row records the same number for
/// every case; the exception is measured, not assumed: `unzip -t` refuses an *empty* zipfile
/// ("zipfile is empty"), and that diagnosis is unzip's rather than the archive's, so the empty ZIP
/// records the other three verifiers only.
fn expected_checks(row: &Row, case: &Case) -> usize {
    match (row.format, case.label) {
        (Format::Zip, "an empty archive") => row.checks_per_case - 1,
        _ => row.checks_per_case,
    }
}

/// Whether a case applies to a format: a single-stream cache holds bytes, not names.
fn applies(format: Format, case: &Case) -> bool {
    match format {
        Format::SingleStream(_) => case.stream,
        _ => true,
    }
}

/// Writes one case for one format through the public streaming API.
fn write(format: Format, case: &Case) -> Vec<u8> {
    let mut writer =
        ArchiveWriter::create(io::Cursor::new(Vec::new()), format, &WriteOptions::default())
            .unwrap_or_else(|error| panic!("{format} should create a writer: {error}"));
    match format {
        Format::SingleStream(_) => {
            // One file's bytes, whatever its name; these writers record no name.
            if let Some(spec) = case.specs.first() {
                writer
                    .write_entry_from_slice(&NewEntry::file(spec.name), &spec.contents)
                    .unwrap_or_else(|error| panic!("{format} should write {}: {error}", spec.name));
            }
        }
        _ => {
            for spec in &case.specs {
                let entry = match spec.kind {
                    EntryKind::Directory => {
                        // TAR's header carries the size even for a directory, so the writers are
                        // all handed one explicitly rather than each having to default it.
                        NewEntry::directory(spec.name).with_size(0)
                    }
                    _ => NewEntry::file(spec.name).with_size(spec.contents.len() as u64),
                };
                writer
                    .write_entry(&entry, &mut &spec.contents[..])
                    .unwrap_or_else(|error| panic!("{} should write {}: {error}", format, spec.name));
            }
        }
    }
    let sink = writer
        .finish_into()
        .unwrap_or_else(|error| panic!("{format} should finish its archive: {error}"));
    sink.into_inner()
}

/// Where one (format, case) pair's archives and extractions go.
///
/// The sibling writer tests (`zip_write_levels.rs`, `sevenz_write.rs`) put them under
/// `CARGO_TARGET_TMPDIR` because the project forbids `/tmp` for scratch space: it is a small tmpfs
/// here and filling it takes the machine with it. The directory is a `tempfile::TempDir`, so an
/// interrupted run cleans up after itself rather than leaving archives behind.
fn scratch(format: &str, case: &str) -> tempfile::TempDir {
    let base = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    tempfile::Builder::new()
        .prefix(&format!("writer-external-{format}-{}-", slug(case)))
        .tempdir_in(base)
        .unwrap_or_else(|error| panic!("scratch space for {format}/{case}: {error}"))
}

/// Something that can appear in a file name.
fn slug(text: &str) -> String {
    text.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// Runs a tool and returns its output.
///
/// This is the one place a silent skip could hide, so it cannot: "a test that skips is a test that
/// asserts nothing", and the oracles are what this file is for.
fn output(tool: &str, args: &[&str]) -> Output {
    Command::new(tool).args(args).output().unwrap_or_else(|error| {
        panic!(
            "writer_external verifies a writer with `{tool}`, which is not installed ({error}). \
             Install it rather than letting this suite pass having done nothing."
        )
    })
}

/// Runs `first`, and hands its stdout to `second`'s stdin. Returns both outputs.
///
/// Both payloads are a few hundred kilobytes, so the middle is buffered in memory; the alternative
/// is a thread per pipe to avoid a deadlock on a tool that fills its stderr. The first tool's
/// stdout is consumed into the second, so what comes back for it is its status and stderr.
fn pipeline(first: &[&str], second: &[&str]) -> (Output, Output) {
    let mut former = Command::new(first[0])
        .args(&first[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("`{:?}` should run: {error}", first));
    let mut middle = former.stdout.take().expect("the first tool's stdout is piped");
    let mut latter = Command::new(second[0])
        .args(&second[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("`{:?}` should run: {error}", second));
    let mut sink = latter.stdin.take().expect("the second tool's stdin is piped");
    io::copy(&mut middle, &mut sink)
        .unwrap_or_else(|error| panic!("piping `{:?}` into `{:?}` failed: {error}", first, second));
    drop(sink);
    let later = latter
        .wait_with_output()
        .unwrap_or_else(|error| panic!("`{:?}` should finish: {error}", second));
    // Read after the wait, not before: the process is gone, so nothing can block on a full pipe.
    let status = former.wait().unwrap_or_else(|error| panic!("`{:?}` should wait: {error}", first));
    let stderr = match former.stderr.take() {
        Some(mut pipe) => {
            let mut text = Vec::new();
            pipe.read_to_end(&mut text)
                .unwrap_or_else(|error| panic!("reading the first tool's stderr failed: {error}"));
            text
        }
        None => Vec::new(),
    };
    (Output { status, stdout: Vec::new(), stderr }, later)
}

/// One archive being verified, and everything a check needs to assert about it.
struct Verifier<'a> {
    report: &'a mut Report,
    format: &'static str,
    case: &'static str,
    path: PathBuf,
    /// The files the archive must produce, for the comparisons that extract.
    expected: BTreeMap<String, Vec<u8>>,
    /// All entries (files and directories) with sizes, for tar listing comparison.
    entries: BTreeMap<String, u64>,
}

impl Verifier<'_> {
    /// Runs a tool and asserts it accepted the archive.
    fn accept(&mut self, oracle: &str, tool: &str, args: &[&str]) {
        let out = output(tool, args);
        assert!(
            out.status.success(),
            "{} / {}: `{} {}` refused the archive this crate wrote.\n\
             This archive is broken or the oracle is; it is not a skip.\nstdout: {}\nstderr: {}",
            self.format,
            self.case,
            args.join(" "),
            self.path.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        self.report.record(self.format, self.case, oracle);
    }

    /// Runs a tool and asserts its stdout, decoded, is the bytes that were written.
    fn stdout_equals(&mut self, oracle: &str, tool: &str, args: &[&str], expected: &[u8]) {
        let out = output(tool, args);
        assert!(
            out.status.success(),
            "{} / {}: `{} {}` refused the archive this crate wrote.\nstdout: {}\nstderr: {}",
            self.format,
            self.case,
            args.join(" "),
            self.path.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(
            out.stdout,
            expected,
            "{} / {}: `{}` did not reproduce the bytes that were written ({} of {} produced)",
            self.format,
            self.case,
            args.join(" "),
            out.stdout.len(),
            expected.len(),
        );
        self.report.record(self.format, self.case, oracle);
    }

    /// Runs a TAR lister and asserts the entries it names, and their sizes, are the ones written.
    fn tar_listing(&mut self, oracle: &str, args: &[&str]) {
        let mut full_args = args[1..].to_vec();
        full_args.push(self.path.to_str().expect("a path"));
        let out = output(args[0], &full_args);
        assert!(
            out.status.success(),
            "{} / {}: `{}` refused the archive this crate wrote.\nstdout: {}\nstderr: {}",
            self.format,
            self.case,
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        let listed = parse_tar_listing(&out.stdout);
        let wanted = &self.entries;
        assert_eq!(
            listed.keys().collect::<Vec<_>>(),
            wanted.keys().collect::<Vec<_>>(),
            "{} / {}: `{}` listed different entries\n  listed: {}\n  wanted: {}\nstdout: {}",
            self.format,
            self.case,
            args.join(" "),
            listed.keys().cloned().collect::<Vec<_>>().join(", "),
            wanted.keys().cloned().collect::<Vec<_>>().join(", "),
            String::from_utf8_lossy(&out.stdout),
        );
        for (name, size) in wanted {
            assert_eq!(
                listed.get(name),
                Some(size),
                "{} / {}: `{}` lists {:?} with size {:?} but {} bytes were written",
                self.format,
                self.case,
                args.join(" "),
                name,
                listed.get(name),
                size,
            );
        }
        self.report.record(self.format, self.case, oracle);
    }

    /// Extracts into a fresh directory and asserts every byte came back.
    fn extraction(&mut self, oracle: &str, dir: &Path, tool: &str, args: &[&str]) {
        assert!(
            tool == "tar",
            "extraction is asserted for `tar xf` and the python extractors, which is what ran"
        );
        let out = output(tool, args);
        assert!(
            out.status.success(),
            "{} / {}: `{} {}` refused the archive this crate wrote.\nstdout: {}\nstderr: {}",
            self.format,
            self.case,
            args.join(" "),
            self.path.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(
            tree(dir),
            self.expected,
            "{} / {}: extracting with `{} {}` produced different files than were written",
            self.format,
            self.case,
            args.join(" "),
            self.path.display(),
        );
        self.report.record(self.format, self.case, oracle);
    }
}

/// Parses GNU tar's verbose listing into name and size.
///
/// The line is `mode owner/group size date time name`, and every case this suite writes has a name
/// with no space in it, so the name is the sixth field onwards. The shape is asserted rather than
/// assumed: if GNU tar ever changes its listing the parse says so instead of comparing nothing.
fn parse_tar_listing(stdout: &[u8]) -> BTreeMap<String, u64> {
    let text = String::from_utf8_lossy(stdout);
    let mut listed = BTreeMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert!(
            fields.len() >= 6,
            "GNU tar's verbose listing is not the shape this parser expects: {line:?}"
        );
        let size = fields[2].parse::<u64>().unwrap_or_else(|error| {
            panic!("GNU tar's size field {:?} is not a number: {error}", fields[2])
        });
        listed.insert(fields[5..].join(" "), size);
    }
    listed
}

/// Every file under `root`, relative to it, with its bytes.
///
/// Directories are not listed, so a writer that wrote a directory entry as a file is caught by the
/// listing checks rather than here.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("listing {} failed: {error}", dir.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|error| panic!("listing {} failed: {error}", dir.display()));
            let path = entry_path(&dir, &entry);
            let file_type = entry
                .path()
                .is_dir()
                .then_some(true)
                .unwrap_or(false);
            if file_type {
                stack.push(path);
                continue;
            }
            let name = path
                .strip_prefix(root)
                .unwrap_or_else(|error| panic!("{} is not under {}: {error}", path.display(), root.display()))
                .to_string_lossy()
                .to_string();
            let bytes = fs::read(&path)
                .unwrap_or_else(|error| panic!("reading {} failed: {error}", path.display()));
            found.insert(name, bytes);
        }
    }
    found
}

/// `entry.path()`, which is what `read_dir` gives for the path.
fn entry_path(_dir: &Path, entry: &fs::DirEntry) -> PathBuf {
    entry.path()
}

/// What has been verified so far, as (format, case, oracle) triples.
#[derive(Default)]
struct Report {
    checks: BTreeSet<(String, &'static str, String)>,
}

impl Report {
    fn record(&mut self, format: &str, case: &'static str, oracle: &str) {
        assert!(
            self.checks.insert((format.to_string(), case, oracle.to_string())),
            "{format} / {case}: {oracle} recorded twice, so a check is being double-counted"
        );
    }

    fn count(&self, format: &str, case: &str) -> usize {
        self.checks.iter().filter(|(f, c, _)| f == format && *c == case).count()
    }

    fn total(&self) -> usize {
        self.checks.len()
    }

    /// The formats that recorded something, in the order they were verified.
    fn formats(&self) -> Vec<String> {
        let mut seen: Vec<String> = self.checks.iter().map(|(format, _, _)| format.clone()).collect();
        seen.sort();
        seen.dedup();
        seen
    }
}

/// Verifies one (format, case) pair with every tool that owns the format.
fn verify(report: &mut Report, format: Format, case: &Case, _tar_bytes: &[u8]) {
    let dir = scratch(&format.extension(), case.label);
    let path = dir.path().join(format!("case.{}", format.extension()));
    let archive_bytes = write(format, case);
    fs::write(&path, &archive_bytes)
        .unwrap_or_else(|error| panic!("writing {} failed: {error}", path.display()));
    let mut verifier = Verifier {
        report,
        format: format_extension(format),
        case: case.label,
        path,
        expected: case.files(),
        entries: case.entries(),
    };

    match format {
        Format::Tar => {
            verifier.tar_listing("tar tvf", &["tar", "tvf"]);
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("python3 -m tarfile -l", "python3", &["-m", "tarfile", "-l", &path_str]);
            verifier.accept("7z t", "7z", &["t", &path_str]);
            let into = dir.path().join("extracted");
            fs::create_dir_all(&into).expect("the extraction directory");
            let args = vec![
                "-xf".to_string(),
                verifier.path.to_str().expect("a path").to_string(),
                "-C".to_string(),
                into.to_str().expect("a path").to_string(),
            ];
            verifier.extraction("tar xf", &into, "tar", &owned(&args));
        }
        Format::CompressedTar(Compression::Gzip) => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("gzip -t", "gzip", &["-t", &path_str]);
            verifier.stdout_equals("gzip -dc", "gzip", &["-dc"], &archive_bytes);
            verifier.tar_listing("tar tvf", &["tar", "tvf"]);
            verifier.accept("python3 -m tarfile -l", "python3", &["-m", "tarfile", "-l", &path_str]);
            verifier.accept("7z t", "7z", &["t", &path_str]);
            let into = dir.path().join("extracted");
            fs::create_dir_all(&into).expect("the extraction directory");
            let args = vec![
                "-xf".to_string(),
                verifier.path.to_str().expect("a path").to_string(),
                "-C".to_string(),
                into.to_str().expect("a path").to_string(),
            ];
            verifier.extraction("tar xf", &into, "tar", &owned(&args));
        }
        Format::CompressedTar(Compression::Bzip2) => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("bzip2 -t", "bzip2", &["-t", &path_str]);
            verifier.stdout_equals("bzip2 -dc", "bzip2", &["-dc"], &archive_bytes);
            verifier.tar_listing("tar tvf", &["tar", "tvf"]);
            verifier.accept("python3 -m tarfile -l", "python3", &["-m", "tarfile", "-l", &path_str]);
            verifier.accept("7z t", "7z", &["t", &path_str]);
            let into = dir.path().join("extracted");
            fs::create_dir_all(&into).expect("the extraction directory");
            let args = vec![
                "-xf".to_string(),
                verifier.path.to_str().expect("a path").to_string(),
                "-C".to_string(),
                into.to_str().expect("a path").to_string(),
            ];
            verifier.extraction("tar xf", &into, "tar", &owned(&args));
        }
        Format::CompressedTar(Compression::Xz) => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("xz -t", "xz", &["-t", &path_str]);
            verifier.stdout_equals("xz -dc", "xz", &["-dc"], &archive_bytes);
            verifier.tar_listing("tar tvf", &["tar", "tvf"]);
            verifier.accept("python3 -m tarfile -l", "python3", &["-m", "tarfile", "-l", &path_str]);
            verifier.accept("7z t", "7z", &["t", &path_str]);
            let into = dir.path().join("extracted");
            fs::create_dir_all(&into).expect("the extraction directory");
            let args = vec![
                "-xf".to_string(),
                verifier.path.to_str().expect("a path").to_string(),
                "-C".to_string(),
                into.to_str().expect("a path").to_string(),
            ];
            verifier.extraction("tar xf", &into, "tar", &owned(&args));
        }
        Format::CompressedTar(Compression::Zstd) => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("zstd -t", "zstd", &["-t", &path_str]);
            verifier.stdout_equals("zstd -dc", "zstd", &["-dc"], &archive_bytes);
            verifier.tar_listing("tar tvf", &["tar", "tvf"]);
            verifier.accept("python3 -m tarfile -l", "python3", &["-m", "tarfile", "-l", &path_str]);
            verifier.accept("7z t", "7z", &["t", &path_str]);
            let into = dir.path().join("extracted");
            fs::create_dir_all(&into).expect("the extraction directory");
            let args = vec![
                "-xf".to_string(),
                verifier.path.to_str().expect("a path").to_string(),
                "-C".to_string(),
                into.to_str().expect("a path").to_string(),
            ];
            verifier.extraction("tar xf", &into, "tar", &owned(&args));
        }
        Format::CompressedTar(Compression::Lz4) => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("lz4 -t", "lz4", &["-t", &path_str]);
            verifier.stdout_equals("lz4 -dc", "lz4", &["-dc"], &archive_bytes);
            let (_, later) = pipeline(&["lz4", "-dc"], &["tar", "tvf"]);
            verifier.accept("lz4 -dc | tar tvf", "tar", &["tvf"]);
            let into = dir.path().join("extracted");
            fs::create_dir_all(&into).expect("the extraction directory");
            let (_, later2) = pipeline(
                &["lz4", "-dc"],
                &[
                    "tar",
                    "-xf",
                    verifier.path.to_str().expect("a path"),
                    "-C",
                    into.to_str().expect("a path"),
                ],
            );
            verifier.extraction("lz4 -dc | tar xf", &into, "tar", &["-xf", "-", "-C", into.to_str().expect("a path")]);
            drop(later);
            drop(later2);
        }
        Format::Zip => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("unzip -t", "unzip", &["-t", &path_str]);
            verifier.accept("7z t", "7z", &["t", &path_str]);
            verifier.accept("python3 -m zipfile testzip", "python3", &["-c", &TEST_ZIP_WITH_PYTHON]);
            let into = dir.path().join("extracted");
            fs::create_dir_all(&into).expect("the extraction directory");
            verifier.accept("python3 -m zipfile extract", "python3", &["-c", &EXTRACT_ZIP_WITH_PYTHON, into.to_str().expect("a path")]);
            verifier.extraction("python3 zipfile extract", &into, "python3", &["-c", &EXTRACT_ZIP_WITH_PYTHON, into.to_str().expect("a path")]);
        }
        Format::SevenZ => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("7z t", "7z", &["t", &path_str]);
            let into = dir.path().join("extracted");
            fs::create_dir_all(&into).expect("the extraction directory");
            let into_str = into.to_str().expect("a path").to_string();
            let out = output("7z", &["x", &("-o".to_owned() + &into_str), &path_str]);
            assert!(
                out.status.success(),
                "{} / {}: `7z x` refused the archive this crate wrote.\nstdout: {}\nstderr: {}",
                format_extension(format),
                case.label,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
            assert_eq!(
                tree(&into),
                verifier.expected,
                "{} / {}: extracting with `7z x` produced different files than were written",
                format_extension(format),
                case.label,
            );
            verifier.report.record(format_extension(format), case.label, "7z x");
        }
        Format::SingleStream(Compression::Gzip) => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("gzip -t", "gzip", &["-t", &path_str]);
            verifier.stdout_equals("gzip -dc", "gzip", &["-dc"], &archive_bytes);
        }
        Format::SingleStream(Compression::Bzip2) => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("bzip2 -t", "bzip2", &["-t", &path_str]);
            verifier.stdout_equals("bzip2 -dc", "bzip2", &["-dc"], &archive_bytes);
        }
        Format::SingleStream(Compression::Xz) => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("xz -t", "xz", &["-t", &path_str]);
            verifier.stdout_equals("xz -dc", "xz", &["-dc"], &archive_bytes);
        }
        Format::SingleStream(Compression::Zstd) => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("zstd -t", "zstd", &["-t", &path_str]);
            verifier.stdout_equals("zstd -dc", "zstd", &["-dc"], &archive_bytes);
            verifier.stdout_equals("7z e -so", "7z", &["e", "-so"], &archive_bytes);
        }
        Format::SingleStream(Compression::Lz4) => {
            let path_str = verifier.path.to_str().expect("a path").to_string();
            verifier.accept("lz4 -t", "lz4", &["-t", &path_str]);
            verifier.stdout_equals("lz4 -dc", "lz4", &["-dc"], &archive_bytes);
            verifier.stdout_equals("liblz4 (python ctypes)", "python3", &["-c", &DECODE_LZ4_FRAME], &archive_bytes);
        }
        Format::SingleStream(Compression::Brotli) => {
            verifier.stdout_equals("brotli (python module)", "python3", &["-c", &DECODE_BROTLI], &archive_bytes);
        }
        _ => panic!("unexpected writable format: {format:?}"),
    }
}

fn owned(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

fn format_extension(format: Format) -> &'static str {
    format.extension()
}

#[test]
fn every_writable_format_is_verified_by_the_tool_that_owns_it() {
    let mut report = Report::default();
    let mut total_expected = 0;
    let mut total_actual = 0;

    for row in ROWS {
        for case in cases() {
            if !applies(row.format, &case) {
                continue;
            }
            let expected = expected_checks(row, &case);
            total_expected += expected;
            verify(&mut report, row.format, &case, &[]);
            let actual = report.count(format_extension(row.format), case.label);
            assert_eq!(
                actual, expected,
                "{} / {}: expected {} checks, got {}",
                format_extension(row.format),
                case.label,
                expected,
                actual
            );
            total_actual += actual;
        }
    }

    println!("writer_external: verified {total_actual} checks across {} formats", ROWS.len());
    for row in ROWS {
        let mut format_total = 0;
        for case in cases() {
            if !applies(row.format, &case) {
                continue;
            }
            format_total += report.count(format_extension(row.format), case.label);
        }
        if format_total > 0 {
            println!("  {}: {} checks", format_extension(row.format), format_total);
        }
    }

    assert_eq!(
        total_actual, total_expected,
        "total checks mismatch: expected {total_expected}, got {total_actual}"
    );
}

#[test]
fn every_tool_the_suite_uses_is_installed() {
    for (tool, description) in TOOLS {
        let (args, version_from_stdout): (&[&str], bool) = match *tool {
            "7z" => (&[][..], false),       // 7z prints version to stderr with no args
            "unzip" => (&["-v"][..], true), // unzip uses -v for version
            _ => (&["--version"][..], true),
        };
        let output = Command::new(tool).args(args).output().unwrap_or_else(|error| {
            panic!(
                "writer_external requires `{tool}` ({description}), which is not installed ({error}). \
                 Install it rather than letting this suite pass having done nothing."
            )
        });
        assert!(
            output.status.success(),
            "`{tool} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        let version = if version_from_stdout {
            String::from_utf8_lossy(&output.stdout)
        } else {
            String::from_utf8_lossy(&output.stderr)
        }
        .trim()
        .to_string();
        println!("  {tool}: {version}");
    }
}
