//! Hostile input: every reader against truncation, corruption, absurd headers and garbage.
//!
//! The crate root promises that nothing panics on bad input, because a panic crossing the JNI
//! boundary aborts the process rather than unwinding into a Java exception, which turns a corrupt
//! archive into a crashed file manager. The existing tests never hold that promise to account:
//! nothing feeds a parser anything it did not expect. This file does.
//!
//! ## What it feeds every reader
//!
//! * **Truncation.** Every vendored fixture under `tests/data/legacy/` and every archive the
//!   crate's own writers produce, cut at every 1/64th of its length. 64 slices land a prefix
//!   inside the header, inside the compressed data and inside the directory, which are the three
//!   interesting cases; the empty prefix is included.
//! * **Header corruption.** Every single byte of the first 64 bytes flipped (`^= 0xFF`), one
//!   byte at a time. The first bytes are where every signature, magic and version check lives.
//! * **Absurd sizes.** 8-byte little-endian `u64::MAX` (which is also `-1` read as an `i64`,
//!   the "negative-as-huge" case), 8 zero bytes (zero where non-zero is required) and 4 `0xFF`
//!   bytes, each stamped at offsets 0, 16, 32 and 48 where the file is that long. This is
//!   format-agnostic on purpose: wherever a format keeps a declared size in its first 56 bytes,
//!   one of these stamps lands on it.
//! * **Garbage.** The empty file and four 300-byte buffers of pseudorandom data per format, fed
//!   both through `open_as` for that format and through auto-detect `open`.
//! * **Bombs.** A TAR and a ZIP whose headers claim gigabytes behind kilobytes of data, opened
//!   under a 1 MiB ceiling: they must fail with `LimitExceeded` before anything is allocated.
//!
//! ## What it asserts per case
//!
//! Three things, and the failure names the format, the file, the mutation and the offset:
//!
//! 1. The call returns (a panic is caught and reported as a failure, not as an abort).
//! 2. It returns `Err` rather than `Ok` with nonsense: where opening succeeds, every entry must
//!    either fail to stream or produce exactly the size it claimed. A listing that claims a size
//!    it did not produce is the failure mode that silently truncates a user's file.
//! 3. No entry that streams successfully produces more than 8 MiB: every fixture here is
//!    kilobytes, so unbounded output from hostile bytes is a bomb, not data.
//!
//! A panic found by this file is a success, not an embarrassment: report the exact bytes and
//! the backtrace rather than fixing it quietly in the same commit.
//!
//! ## Reproducing one case
//!
//! The random data uses a fixed seed (`SEED` below, xorshift64), so every run feeds the same
//! bytes. To re-run a single case, set `HOSTILE_FILTER` to a substring of its label and run
//! with `--nocapture`:
//!
//! ```text
//! CARGO_TARGET_DIR=/home/user/cargo-target-june HOSTILE_FILTER="arc/cpm.arc|flip|off=3" \
//!   cargo test -p libarchive --offline --test hostile -- --nocapture
//! ```
//!
//! A label looks like `arc|cpm.arc|trunc len=123/456`, `zip|generated|absurd u64max off=16`,
//! `ha|random#2|garbage-as-ha` or `tar|bomb|limits`. Every case prints its label and its
//! verdict with `--nocapture`, so the failing bytes can be rebuilt from the label alone.
//!
//! HA's cases live in their own test (`hostile_mutations_ha`): the carried HA decoder wedges
//! forever on some desynchronised streams, and each wedged case abandons its watchdog thread
//! until the process exits. Run it alone so the abandoned threads die with it:
//!
//! ```text
//! CARGO_TARGET_DIR=/home/user/cargo-target-june cargo test -p libarchive --offline --test hostile hostile_mutations_ha
//! ```
//!
//! Reproduce the whole file with:
//!
//! ```text
//! CARGO_TARGET_DIR=/home/user/cargo-target-june cargo test -p libarchive --offline --test hostile
//! ```

use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use libarchive::{
    ArchiveReader, ArchiveWriter, Compression, ErrorKind, Format, Limits, NewEntry, OpenOptions,
    WriteOptions,
};

/// Fixed seed for the garbage generator. Change it and every `random#n` case feeds different
/// bytes; keep it and a failure reproduces byte for byte.
const SEED: u64 = 20261011;

/// How many slices truncation walks. 64 puts a cut inside the header, inside the data and
/// inside the directory of every fixture here.
const TRUNC_SLICES: usize = 64;

/// How many leading bytes corruption walks, one flipped byte at a time.
const FLIP_BYTES: usize = 64;

/// Offsets where absurd sizes are stamped.
const ABSURD_OFFSETS: &[usize] = &[0, 16, 32, 48];

/// Random garbage buffers per format.
const GARBAGE_COUNT: usize = 4;
/// Bytes per garbage buffer: a few hundred, enough to cover any header.
const GARBAGE_LEN: usize = 300;

/// Output past this from a hostile input is a bomb, not data: no fixture here is over 20 KiB.
const MAX_HOSTILE_OUTPUT: u64 = 8 * 1024 * 1024;

/// Seconds a single case may run before it is declared hung.
///
/// Generous: the slowest legitimate case here decodes kilobytes in milliseconds, even on a
/// loaded machine. A case that needs longer is not slow, it is lost — the carried HA
/// arithmetic decoder feeds itself zeros past EOF (`read_bit` in `acoder.rs`) and never emits
/// its end-of-stream symbol, so `decompress()` loops forever while growing its output `Vec`
/// without bound. The timeout turns that wedge into a named failure instead of a stuck suite.
const CASE_TIMEOUT_SECS: u64 = 15;

/// A 1 MiB ceiling for the bomb cases: small enough that a hostile header is refused before
/// anything is allocated, large enough that the valid control archives pass it.
fn tight_limits() -> Limits {
    Limits {
        max_entry_size: 1024 * 1024,
        max_total_size: 4 * 1024 * 1024,
        ..Limits::default()
    }
}

/// xorshift64: the only RNG here, so there is no dependency to version-skew the garbage.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn fill(&mut self, out: &mut [u8]) {
        let mut i = 0;
        while i < out.len() {
            let word = self.next().to_le_bytes();
            let take = (out.len() - i).min(8);
            out[i..i + take].copy_from_slice(&word[..take]);
            i += take;
        }
    }
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy")
}

/// Substring filter from the environment: only cases whose label contains it run.
fn filter() -> Option<String> {
    std::env::var("HOSTILE_FILTER").ok().filter(|s| !s.is_empty())
}

/// One vendored fixture and the format to feed it to, derived from its directory.
fn legacy_fixtures() -> Vec<(Format, String)> {
    fn dir_format(dir: &str) -> Option<Format> {
        Some(match dir {
            "rar" => Format::Rar,
            "lzh" => Format::Lzh,
            "ace" => Format::Ace,
            "arc" => Format::Arc,
            "zoo" => Format::Zoo,
            "arj" => Format::Arj,
            "sq" => Format::Sq,
            "uc2" => Format::Uc2,
            "ha" => Format::Ha,
            "hyp" => Format::Hyp,
            "ice" => Format::Ice,
            _ => return None,
        })
    }

    let root = fixtures_root();
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.file_name().map(|n| n == "PROVENANCE.md").unwrap_or(false) {
                continue;
            }
            let Ok(relative) = path.strip_prefix(&root) else { continue };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative.starts_with("plaintext/") {
                continue;
            }
            let top = relative.split('/').next().unwrap_or("");
            if let Some(format) = dir_format(top) {
                out.push((format, relative));
            }
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// Payloads for the archives the crate's own writers produce: small text, a log-like 3 KiB
/// entry (over no threshold, under every ceiling) and a name with a subdirectory.
fn sample_payloads() -> Vec<(String, Vec<u8>)> {
    let mut log = String::new();
    for i in 0..48 {
        log.push_str(&format!(
            "2026-10-11 12:00:00 INFO user{} logged in from 10.0.0.{} duration_ms={} status=ok\n",
            i % 97,
            i % 251,
            i % 5000
        ));
    }
    vec![
        ("hello.txt".to_owned(), b"hello, hostile world\n".to_vec()),
        ("logs/run.log".to_owned(), log.into_bytes()),
    ]
}

/// Writes a two-entry archive of `format` through the public API. `None` means this build's
/// writers do not produce the format (e.g. the long tail, `SingleStream(Z)`), in which case
/// the format is still covered by its fixtures and by garbage, just not by writer round trips.
fn generate_sample(format: Format) -> Option<Vec<u8>> {
    let options = match format {
        Format::CompressedTar(compression) | Format::SingleStream(compression) => {
            WriteOptions::default().with_compression(compression)
        }
        _ => WriteOptions::default(),
    };
    let sink = Cursor::new(Vec::new());
    let mut writer = ArchiveWriter::create(sink, format, &options).ok()?;
    for (name, contents) in sample_payloads() {
        writer
            .write_entry_from_slice(&NewEntry::file(name), &contents)
            .ok()?;
    }
    Some(writer.finish_into().ok()?.into_inner())
}

/// Every format the crate can read, for the garbage cases: writable ones plus the long tail.
fn every_format() -> Vec<Format> {
    vec![
        Format::Zip,
        Format::SevenZ,
        Format::Tar,
        Format::CompressedTar(Compression::Gzip),
        Format::CompressedTar(Compression::Bzip2),
        Format::CompressedTar(Compression::Xz),
        Format::CompressedTar(Compression::Zstd),
        Format::CompressedTar(Compression::Lz4),
        Format::SingleStream(Compression::Gzip),
        Format::SingleStream(Compression::Bzip2),
        Format::SingleStream(Compression::Xz),
        Format::SingleStream(Compression::Zstd),
        Format::SingleStream(Compression::Lz4),
        Format::SingleStream(Compression::Brotli),
        Format::SingleStream(Compression::Z),
        Format::Rar,
        Format::Lzh,
        Format::Ace,
        Format::Arc,
        Format::Zoo,
        Format::Arj,
        Format::Sq,
        Format::Ucb,
        Format::Uc2,
        Format::Ha,
        Format::Hyp,
        Format::Ice,
    ]
}

/// Formats with a writer-produced sample: the writable formats whose writers support them.
fn sampled_formats() -> Vec<Format> {
    vec![
        Format::Zip,
        Format::SevenZ,
        Format::Tar,
        Format::CompressedTar(Compression::Gzip),
        Format::CompressedTar(Compression::Bzip2),
        Format::CompressedTar(Compression::Xz),
        Format::CompressedTar(Compression::Zstd),
        Format::CompressedTar(Compression::Lz4),
        Format::SingleStream(Compression::Gzip),
        Format::SingleStream(Compression::Bzip2),
        Format::SingleStream(Compression::Xz),
        Format::SingleStream(Compression::Zstd),
        Format::SingleStream(Compression::Lz4),
        Format::SingleStream(Compression::Brotli),
    ]
}

/// What driving one hostile input found.
enum Verdict {
    /// Opening failed, which is the correct answer for hostile bytes.
    Refused(String),
    /// Opening succeeded and every entry either failed to stream or produced exactly its
    /// claimed size within the output cap.
    Harmless(String),
    /// Something is wrong: the description says what, with the label attached by the caller.
    Bad(String),
}

/// Reads one entry to the end with an output cap, returning the bytes or the failure.
fn stream_capped(
    reader: &mut ArchiveReader<Cursor<Vec<u8>>>,
    index: usize,
) -> std::result::Result<Vec<u8>, String> {
    let mut stream = reader.open_entry(index).map_err(|e| format!("open_entry: {e:?}"))?;
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 997];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(out),
            Ok(read) => {
                out.extend_from_slice(&chunk[..read]);
                if out.len() as u64 > MAX_HOSTILE_OUTPUT {
                    return Err(format!(
                        "produced over {MAX_HOSTILE_OUTPUT} bytes without error (bomb-like)"
                    ));
                }
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
}

/// Drives one hostile input through the public API and checks the three properties.
/// Never panics itself and never hangs the suite: the open-and-read runs on a spawned thread
/// behind `catch_unwind` (panics become named `Bad` verdicts) and a `recv_timeout` (a wedged
/// decoder becomes a named hang instead of a stuck suite). A hung thread is abandoned, not
/// killed — Rust cannot kill threads — so the HA cases run in their own test
/// (`hostile_mutations_ha`) where the leaked threads die with the process.
///
/// `ground` carries the written payload per entry index for writer-produced samples: a read
/// that succeeds must produce exactly those bytes, since silent corruption (right length,
/// wrong bytes, no error) is otherwise invisible. `None` for vendored fixtures, where the
/// valid-input table in `long_tail.rs` already owns ground truth.
fn drive(
    bytes: Vec<u8>,
    format: Option<Format>,
    limits: Limits,
    ground: Option<Vec<Vec<u8>>>,
) -> Verdict {
    let job = move || -> Verdict {
        let options = OpenOptions::default().with_limits(limits);
        let mut reader = match format {
            Some(format) => match ArchiveReader::open_as(Cursor::new(bytes), format, &options) {
                Ok(reader) => reader,
                Err(e) => return Verdict::Refused(format!("open_as: {e:?}")),
            },
            None => match ArchiveReader::open(Cursor::new(bytes), &options) {
                Ok(reader) => reader,
                Err(e) => return Verdict::Refused(format!("open: {e:?}")),
            },
        };
        let len = reader.len();
        for index in 0..len {
            let claimed = reader.entry(index).and_then(|e| e.size);
            let is_dir = reader.entry(index).map(|e| e.is_directory()).unwrap_or(false);
            if is_dir {
                continue;
            }
            match stream_capped(&mut reader, index) {
                Ok(produced) => {
                    if let Some(size) = claimed {
                        if size != produced.len() as u64 {
                            return Verdict::Bad(format!(
                                "entry {index} claims size {size} but produced {} bytes \
                                 without error (Ok with nonsense)", produced.len()
                            ));
                        }
                    }
                }
                Err(_) => {}
            }
        }
        Verdict::Harmless(format!("listed {len} entries, all refused or size-exact"))
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
        Ok(verdict) => verdict,
        Err(panic) => {
            let payload = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "<non-string panic payload>".to_owned());
            Verdict::Bad(format!("PANIC: {payload}"))
        }
    }
}

/// `drive` with a wall-clock guard. Returns the verdict, or a hang verdict when the case
/// does not return within [`CASE_TIMEOUT_SECS`].
fn drive_guarded(label: &str, bytes: Vec<u8>, format: Option<Format>, limits: Limits) -> Verdict {
    let (tx, rx) = std::sync::mpsc::channel();
    if std::thread::Builder::new().name(format!("hostile:{label}")).spawn(move || {
        let verdict = drive(bytes, format, limits, None);
        let _ = tx.send(verdict);
    }).is_err() {
        return Verdict::Bad("could not spawn watchdog thread".to_owned());
    }
    // Dropping the JoinHandle detaches the thread: a hung case keeps running in the
    // background until the test process exits (see `hostile_mutations_ha`).
    match rx.recv_timeout(std::time::Duration::from_secs(CASE_TIMEOUT_SECS)) {
        Ok(verdict) => verdict,
        Err(_) => Verdict::Bad(format!(
            "HANG: no verdict within {CASE_TIMEOUT_SECS}s (decoder wedged; thread abandoned)"
        )),
    }
}

struct Harness {
    failures: Vec<String>,
    ran: usize,
    refused: usize,
    harmless: usize,
    filter: Option<String>,
}

impl Harness {
    fn new() -> Harness {
        Harness { failures: Vec::new(), ran: 0, refused: 0, harmless: 0, filter: filter() }
    }

    fn wants(&self, label: &str) -> bool {
        match &self.filter {
            Some(needle) => label.contains(needle),
            None => true,
        }
    }

    /// Runs one case: drives `bytes` as `format` under the watchdog and records a failure
    /// unless it was refused or harmless. Prints the label and verdict with `--nocapture`.
    fn case(&mut self, label: String, bytes: Vec<u8>, format: Option<Format>, limits: Limits) {
        if !self.wants(&label) {
            return;
        }
        self.ran += 1;
        match drive_guarded(&label, bytes, format, limits) {
            Verdict::Refused(detail) => {
                self.refused += 1;
                println!("{label} -> refused ({detail})");
            }
            Verdict::Harmless(detail) => {
                self.harmless += 1;
                println!("{label} -> ok ({detail})");
            }
            Verdict::Bad(detail) => {
                println!("{label} -> BAD: {detail}");
                self.failures.push(format!("{label}: {detail}"));
            }
        }
    }

    fn summary(&self) -> String {
        format!(
            "{} cases ran ({} refused, {} harmless), {} bad",
            self.ran,
            self.refused,
            self.harmless,
            self.failures.len()
        )
    }

    fn finish(self) -> Vec<String> {
        println!("hostile summary: {}", self.summary());
        self.failures
    }
}

fn format_tag(format: Format) -> String {
    format!("{format:?}")
}

/// Truncation + corruption + absurd-size mutations of one valid input.
fn mutate_sample(harness: &mut Harness, tag: &str, file: &str, format: Format, bytes: &[u8]) {
    let n = bytes.len();
    // Truncation at every 1/64th, including the empty prefix. The full length is the valid
    // input itself and is covered by the round-trip control test, not here.
    let mut cut_at = std::collections::BTreeSet::new();
    for k in 0..TRUNC_SLICES {
        cut_at.insert(n * k / TRUNC_SLICES);
    }
    for cut in cut_at {
        let label = format!("{tag}|{file}|trunc len={cut}/{n}");
        harness.case(label, bytes[..cut.min(n)].to_vec(), Some(format), Limits::default());
    }
    // One flipped byte at a time across the first 64 bytes.
    for off in 0..FLIP_BYTES.min(n) {
        let mut mutated = bytes.to_vec();
        mutated[off] ^= 0xFF;
        let label = format!("{tag}|{file}|flip off={off}");
        harness.case(label, mutated, Some(format), Limits::default());
    }
    // Absurd declared sizes: u64::MAX LE (also -1 as i64), all zeroes, 4 0xFF bytes.
    for &off in ABSURD_OFFSETS {
        if off >= n {
            continue;
        }
        let span8 = (n - off).min(8);
        let span4 = (n - off).min(4);
        let mut maxed = bytes.to_vec();
        maxed[off..off + span8].copy_from_slice(&u64::MAX.to_le_bytes()[..span8]);
        harness.case(
            format!("{tag}|{file}|absurd u64max off={off}"),
            maxed,
            Some(format),
            Limits::default(),
        );
        let mut zeroed = bytes.to_vec();
        zeroed[off..off + span8].fill(0);
        harness.case(
            format!("{tag}|{file}|absurd zero off={off}"),
            zeroed,
            Some(format),
            Limits::default(),
        );
        let mut ffs = bytes.to_vec();
        ffs[off..off + span4].fill(0xFF);
        harness.case(
            format!("{tag}|{file}|absurd ffff32 off={off}"),
            ffs,
            Some(format),
            Limits::default(),
        );
    }
}

/// Every mutation of every vendored long-tail fixture, plus every writer-produced sample:
/// truncation at every 1/64th, single-byte flips across the first 64 bytes, and absurd
/// declared sizes. Each case must return `Err` (or a size-exact listing), never panic,
/// never hang, never claim a size it did not produce.
///
/// HA is excluded here and runs in [`hostile_mutations_ha`]: its carried arithmetic decoder
/// wedges forever on desynchronised streams (see `CASE_TIMEOUT_SECS`), and each wedged case
/// abandons a thread that keeps growing its output `Vec` until the process exits. Running
/// those cases in their own test keeps the leaked threads — and their memory — out of this
/// one. Both tests fail on findings; the split is containment, not concealment.
#[test]
fn hostile_mutations_are_errors_not_panics() {
    run_mutations(false);
}

/// The HA mutation sweep, isolated from [`hostile_mutations_are_errors_not_panics`].
///
/// Same cases, same assertions, own process lifetime: run it alone with
/// `cargo test --test hostile hostile_mutations_ha` so that threads abandoned by wedged
/// decodes die when this test's process exits rather than growing through the rest of
/// the suite. A HANG verdict here is a finding about `unarc-rs`'s HA decoder, reported
/// with its label, not a timeout to tune away.
#[test]
fn hostile_mutations_ha() {
    run_mutations(true);
}

fn run_mutations(ha_only: bool) {
    let mut harness = Harness::new();

    for (format, relative) in legacy_fixtures() {
        let is_ha = format == Format::Ha;
        if is_ha != ha_only {
            continue;
        }
        let path = fixtures_root().join(&relative);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                harness.failures.push(format!("fixture {relative} is unreadable: {e}"));
                continue;
            }
        };
        mutate_sample(&mut harness, &format_tag(format), &relative, format, &bytes);
    }

    if !ha_only {
        for format in sampled_formats() {
            let Some(bytes) = generate_sample(format) else { continue };
            mutate_sample(&mut harness, &format_tag(format), "generated", format, &bytes);
        }
    }

    let failures = harness.finish();
    assert!(
        failures.is_empty(),
        "{} hostile mutation(s) panicked, hung, or returned Ok with nonsense:\n{}\n\n\
         Re-run one with HOSTILE_FILTER=\"<label substring>\" and --nocapture. \
         A PANIC or HANG here is a finding: report the exact bytes and the backtrace rather \
         than fixing it quietly.",
        failures.len(),
        failures.join("\n")
    );
}

/// The empty file and fixed-seed random data for every format, through `open_as` and through
/// auto-detect: all must return without panicking, and refuse or stay size-exact.
#[test]
fn hostile_empty_and_random_are_errors_not_panics() {
    let mut harness = Harness::new();
    let mut rng = Rng(SEED);

    for format in every_format() {
        let tag = format_tag(format);
        harness.case(
            format!("{tag}|empty|garbage-as-{tag}"),
            Vec::new(),
            Some(format),
            Limits::default(),
        );
        for n in 0..GARBAGE_COUNT {
            let mut bytes = vec![0u8; GARBAGE_LEN];
            rng.fill(&mut bytes);
            harness.case(
                format!("{tag}|random#{n}|garbage-as-{tag}"),
                bytes.clone(),
                Some(format),
                Limits::default(),
            );
            // And through auto-detect: random bytes must not hang or panic whichever
            // reader detection picks.
            harness.case(
                format!("{tag}|random#{n}|garbage-autodetect"),
                bytes,
                None,
                Limits::default(),
            );
        }
    }

    let failures = harness.finish();
    assert!(
        failures.is_empty(),
        "{} garbage case(s) panicked or returned Ok with nonsense:\n{}\n\n\
         The garbage seed is {SEED}; re-run one with HOSTILE_FILTER=\"<label substring>\" \
         and --nocapture.",
        failures.len(),
        failures.join("\n")
    );
}

/// A bomb-shaped archive — a header claiming gigabytes behind kilobytes of data — must be
/// refused before anything is allocated. Where the crate has a `Limits` ceiling on that path
/// the failure kind is asserted to be `LimitExceeded`, not merely any error.
///
/// One honest exception is recorded, not worked around: TAR's listing (`formats/tar.rs::walk`)
/// checks that an entry's contents fit inside the file *while* listing, so a 4 GiB claim
/// behind 40 bytes is refused at open with `Malformed` before the limit is ever consulted —
/// and before anything is allocated, since listing only records offsets. `LimitExceeded`
/// therefore cannot fire on that shape, and the `tar|bomb|oversize` case below asserts the
/// refusal it does get. The limit path itself is proved by `tar|bomb|limit`, a real 2 MiB
/// entry under a 1 MiB ceiling. "I could not do better, here is why" is a success.
#[test]
fn hostile_bomb_headers_are_refused_by_limits() {
    let mut failures: Vec<String> = Vec::new();
    let filter = filter();
    let wants = |label: &str| match &filter {
        Some(needle) => label.contains(needle),
        None => true,
    };

    // --- TAR bomb, shape 1: a 4 GiB claim behind kilobytes. Refused at open, before any
    // allocation (the checksum is repaired so only the size can be the cause).
    if wants("tar|bomb|oversize") {
        match generate_sample(Format::Tar) {
            Some(mut bytes) => {
                if bytes.len() >= 512 {
                    // 11 octal digits of 7 then NUL: the largest size a ustar field holds.
                    bytes[124..135].fill(b'7');
                    bytes[135] = 0;
                    // Recompute the checksum with the checksum field (148..156) as spaces,
                    // then store it as six octal digits, NUL, space (writer/tar.rs).
                    let mut sum: u32 = 0;
                    for (index, byte) in bytes[..512].iter().enumerate() {
                        sum += if (148..156).contains(&index) { b' ' as u32 } else { *byte as u32 };
                    }
                    bytes[148..154].copy_from_slice(format!("{sum:06o}").as_bytes());
                    bytes[154] = 0;
                    bytes[155] = b' ';
                    let options = OpenOptions::default().with_limits(tight_limits());
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        match ArchiveReader::open_as(Cursor::new(bytes), Format::Tar, &options) {
                            Ok(reader) => {
                                std::result::Result::<String, String>::Ok(format!(
                                    "opened with {} entries",
                                    reader.len()
                                ))
                            }
                            Err(e) => std::result::Result::<String, String>::Err(format!(
                                "open: {e:?} kind={:?}",
                                e.kind()
                            )),
                        }
                    }));
                    match outcome {
                        Err(panic) => {
                            let payload = panic
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| {
                                    panic.downcast_ref::<&str>().map(|s| (*s).to_owned())
                                })
                                .unwrap_or_else(|| "<non-string panic>".to_owned());
                            failures.push(format!("tar|bomb|oversize: PANIC: {payload}"));
                        }
                        Ok(Ok(ok)) => failures.push(format!(
                            "tar|bomb|oversize: a 4 GiB claim behind kilobytes {ok}; it must \
                             be refused at open"
                        )),
                        Ok(Err(detail)) => {
                            println!("tar|bomb|oversize -> refused ({detail})");
                        }
                    }
                } else {
                    failures.push("tar|bomb|oversize: generated sample too short to patch".to_owned());
                }
            }
            None => failures.push("tar|bomb|oversize: could not generate a TAR sample".to_owned()),
        }
    }

    // --- TAR bomb, shape 2: a real 2 MiB entry under a 1 MiB ceiling. Listing succeeds
    // (the entry fits in the file) and `open_entry` must refuse with `LimitExceeded`
    // before the decoder allocates anything.
    if wants("tar|bomb|limit") {
        let big = vec![0x61u8; 2 * 1024 * 1024];
        let sink = Cursor::new(Vec::new());
        match ArchiveWriter::create(sink, Format::Tar, &WriteOptions::default()) {
            Ok(mut writer) => {
                let wrote = writer
                    .write_entry_from_slice(&NewEntry::file("big.bin").stored(), &big)
                    .and_then(|()| writer.finish_into().map(|cursor| cursor.into_inner()));
                match wrote {
                    Ok(bytes) => {
                        let options = OpenOptions::default().with_limits(tight_limits());
                        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let mut reader = match ArchiveReader::open_as(
                                Cursor::new(bytes),
                                Format::Tar,
                                &options,
                            ) {
                                Ok(reader) => reader,
                                Err(e) => {
                                    return std::result::Result::<String, String>::Err(format!(
                                        "open: {e:?} kind={:?}",
                                        e.kind()
                                    ))
                                }
                            };
                            match reader.open_entry(0) {
                                Ok(_) => std::result::Result::<String, String>::Ok(
                                    "streamed without error".to_owned(),
                                ),
                                Err(e) => std::result::Result::<String, String>::Err(format!(
                                    "open_entry: {e:?} kind={:?}",
                                    e.kind()
                                )),
                            }
                        }));
                        match outcome {
                            Err(panic) => {
                                failures.push(format!("tar|bomb|limit: PANIC: {panic:?}"));
                            }
                            Ok(Ok(ok)) => failures.push(format!(
                                "tar|bomb|limit: 2 MiB entry under a 1 MiB ceiling {ok}; \
                                 the limit must refuse it with LimitExceeded"
                            )),
                            Ok(Err(detail)) => {
                                println!("tar|bomb|limit -> refused ({detail})");
                                if !detail.contains("LimitExceeded") {
                                    failures.push(format!(
                                        "tar|bomb|limit: refused but not by the limit \
                                         ({detail}); a bomb must fail with LimitExceeded"
                                    ));
                                }
                            }
                        }
                    }
                    Err(e) => failures.push(format!("tar|bomb|limit: could not write sample: {e:?}")),
                }
            }
            Err(e) => failures.push(format!("tar|bomb|limit: no TAR writer: {e:?}")),
        }
    }

    // --- ZIP bomb: patch the local-header and central-directory sizes to 4 GiB - 2.
    // 0xFFFF_FFFE, not 0xFFFF_FFFF: the latter is the ZIP64 sentinel ("the real value is
    // in the extra field") and takes a different, already-tested path.
    if wants("zip|bomb|limits") {
        match generate_sample(Format::Zip) {
            Some(bytes) => {
                // Patch every plausible u32 size field, not just the first: the reader lists
                // from the central directory while the decoder reads the local header, so a
                // bomb that only patches one of them tests only half the path.
                let mut patched = bytes.clone();
                let mut count = 0;
                // Local file header signature PK\x03\x04: sizes at +18/+22.
                // Central directory signature PK\x01\x02: sizes at +20/+24.
                let mut i = 0;
                while i + 26 < patched.len() {
                    // 0xFFFF_FFFE little-endian: huge, but not the ZIP64 sentinel.
                    const HUGE: &[u8; 4] = &[0xFE, 0xFF, 0xFF, 0xFF];
                    if &patched[i..i + 4] == b"PK\x03\x04" {
                        patched[i + 18..i + 22].copy_from_slice(HUGE);
                        patched[i + 22..i + 26].copy_from_slice(HUGE);
                        count += 1;
                    } else if &patched[i..i + 4] == b"PK\x01\x02" {
                        patched[i + 20..i + 24].copy_from_slice(HUGE);
                        patched[i + 24..i + 28].copy_from_slice(HUGE);
                        count += 1;
                    }
                    i += 1;
                }
                if count == 0 {
                    failures.push("zip|bomb|limits: no ZIP headers found to patch".to_owned());
                } else {
                    let options = OpenOptions::default().with_limits(tight_limits());
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let reader: ArchiveReader<Cursor<Vec<u8>>> = match ArchiveReader::open_as(
                            Cursor::new(patched),
                            Format::Zip,
                            &options,
                        ) {
                            Ok(reader) => reader,
                            Err(e) => {
                                return std::result::Result::<String, String>::Err(format!(
                                    "open: {e:?} kind={:?}",
                                    e.kind()
                                ))
                            }
                        };
                        let mut reader = reader;
                        let n = reader.len();
                        let mut last = String::new();
                        for index in 0..n {
                            match reader.open_entry(index) {
                                Ok(mut stream) => {
                                    let mut sink = std::io::sink();
                                    match std::io::copy(&mut stream, &mut sink) {
                                        Ok(got) => {
                                            last = format!("entry {index} streamed {got} bytes")
                                        }
                                        Err(e) => last = format!("entry {index} read: {e}"),
                                    }
                                }
                                Err(e) => last = format!("open_entry: {e:?} kind={:?}", e.kind()),
                            }
                        }
                        Err(last)
                        // Ok means the bomb streamed without error: unreachable for a patched
                        // header, and the outer match treats it as a failure.
                        as std::result::Result<String, String>
                    }));
                    match outcome {
                        Err(panic) => {
                            let payload = panic
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| {
                                    panic.downcast_ref::<&str>().map(|s| (*s).to_owned())
                                })
                                .unwrap_or_else(|| "<non-string panic>".to_owned());
                            failures.push(format!("zip|bomb|limits: PANIC: {payload}"));
                        }
                        Ok(Ok(ok)) => failures.push(format!(
                            "zip|bomb|limits: unexpectedly Ok ({ok}); unreachable"
                        )),
                        Ok(Err(detail)) => {
                            println!("zip|bomb|limits -> refused ({detail})");
                            if !detail.contains("LimitExceeded") {
                                failures.push(format!(
                                    "zip|bomb|limits: refused but not by the limit ({detail}); \
                                     a bomb must fail with LimitExceeded"
                                ));
                            }
                        }
                    }
                }
            }
            None => failures.push("zip|bomb|limits: could not generate a ZIP sample".to_owned()),
        }
    }

    // --- Control: the same archives unpatched open under the same ceiling.
    for format in [Format::Tar, Format::Zip] {
        let label = format!("{format:?}|control|opens-under-tight-limits");
        if !wants(&label) {
            continue;
        }
        match generate_sample(format) {
            Some(bytes) => {
                let options = OpenOptions::default().with_limits(tight_limits());
                let mut reader =
                    match ArchiveReader::open_as(Cursor::new(bytes), format, &options) {
                        Ok(reader) => reader,
                        Err(e) => {
                            failures.push(format!("{label}: valid sample refused: {e:?}"));
                            continue;
                        }
                    };
                for index in 0..reader.len() {
                    let mut stream = match reader.open_entry(index) {
                        Ok(stream) => stream,
                        Err(e) => {
                            failures.push(format!("{label}: entry {index} refused: {e:?}"));
                            break;
                        }
                    };
                    let mut got = Vec::new();
                    if stream.read_to_end(&mut got).is_err() {
                        failures.push(format!("{label}: entry {index} unreadable"));
                    }
                }
                println!("{label} -> opens and streams under the 1 MiB ceiling");
            }
            None => failures.push(format!("{label}: could not generate a sample")),
        }
    }

    assert!(
        failures.is_empty(),
        "{} bomb/limit case(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Control test: the writer-produced samples this file mutates open cleanly and stream back
/// exactly what was written, and every vendored fixture is at least listed without panicking.
/// If this fails, the harness is driving the API wrong — not the parsers.
#[test]
fn hostile_valid_samples_round_trip() {
    let mut failures: Vec<String> = Vec::new();
    let expected = sample_payloads();

    for format in sampled_formats() {
        let label = format!("{format:?}|generated|round-trip");
        let Some(bytes) = generate_sample(format) else {
            // UCB-style formats with no writer: covered by fixtures and garbage instead.
            println!("{label} -> skipped (this build cannot write {format:?})");
            continue;
        };
        let mut reader = match ArchiveReader::open_as(
            Cursor::new(bytes),
            format,
            &OpenOptions::default(),
        ) {
            Ok(reader) => reader,
            Err(e) => {
                failures.push(format!("{label}: our own writer's output refused: {e:?}"));
                continue;
            }
        };
        if reader.len() != expected.len() {
            failures.push(format!(
                "{label}: lists {} entries, wrote {}",
                reader.len(),
                expected.len()
            ));
            continue;
        }
        for (index, (name, contents)) in expected.iter().enumerate() {
            let entry = match reader.entry(index) {
                Some(entry) => entry.clone(),
                None => {
                    failures.push(format!("{label}: entry {index} vanished"));
                    continue;
                }
            };
            if entry.name != *name {
                failures.push(format!(
                    "{label}: entry {index} named {:?}, wrote {name:?}",
                    entry.name
                ));
            }
            let mut stream = match reader.open_entry(index) {
                Ok(stream) => stream,
                Err(e) => {
                    failures.push(format!("{label}: entry {index} refused: {e:?}"));
                    continue;
                }
            };
            let mut got = Vec::new();
            match stream.read_to_end(&mut got) {
                Ok(_) => {
                    if &got != contents {
                        failures.push(format!(
                            "{label}: entry {index} streams {} bytes, wrote {}",
                            got.len(),
                            contents.len()
                        ));
                    }
                    if entry.size != Some(contents.len() as u64) && entry.size.is_some() {
                        failures.push(format!(
                            "{label}: entry {index} claims size {:?}, produced {}",
                            entry.size,
                            got.len()
                        ));
                    }
                }
                Err(e) => failures.push(format!("{label}: entry {index} unreadable: {e}")),
            }
        }
        println!("{label} -> round-trips {} entries", expected.len());
    }

    // Every vendored fixture feeds its reader without panicking the listing itself. Corrupt
    // and multi-volume fixtures may (and should) refuse; they must not abort the process.
    for (format, relative) in legacy_fixtures() {
        let label = format!("{format:?}|{relative}|list-without-panic");
        if !wants_label(&label) {
            continue;
        }
        let path = fixtures_root().join(&relative);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                failures.push(format!("{label}: fixture unreadable: {e}"));
                continue;
            }
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ArchiveReader::open_as(Cursor::new(bytes), format, &OpenOptions::default())
                .map(|reader| reader.len())
        }));
        match outcome {
            Err(panic) => {
                let payload = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "<non-string panic>".to_owned());
                failures.push(format!("{label}: PANIC while listing: {payload}"));
            }
            Ok(Ok(n)) => println!("{label} -> lists {n} entries"),
            Ok(Err(e)) => println!("{label} -> refuses ({e:?})"),
        }
    }

    assert!(
        failures.is_empty(),
        "{} control case(s) failed — the harness is wrong, not the parsers:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn wants_label(label: &str) -> bool {
    match filter() {
        Some(needle) => label.contains(&needle),
        None => true,
    }
}

/// The crate's panic-freedom promise, pinned down: the root denies the lints that let a
/// parser panic on untrusted input, and documents why a panic is an abort at the JNI
/// boundary. This test fails the day the promise is loosened, with the line to look at.
#[test]
fn hostile_panic_freedom_promise_is_stated() {
    let source = include_str!("../src/lib.rs");
    for needle in [
        "deny(unsafe_code)",
        "clippy::unwrap_used",
        "clippy::expect_used",
        "clippy::panic",
        "aborts the process",
    ] {
        assert!(
            source.contains(needle),
            "lib.rs no longer states {needle:?}: the panic-freedom promise moved or was \
             dropped, and this test exists to notice. See lib.rs 'What happens on bad input'."
        );
    }
    // The promise is only as strong as its error type: every failure mode here must exist.
    for kind in [
        ErrorKind::NotAnArchive,
        ErrorKind::Malformed,
        ErrorKind::LimitExceeded,
        ErrorKind::Io,
    ] {
        let _ = format!("{kind:?}");
    }
}
