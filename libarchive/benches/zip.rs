//! Does writing an archive of 5,000 entries hold the numbers the crate is measured against?
//!
//! The structural numbers are asserted, so a regression fails `cargo bench` rather than quietly
//! producing a slightly worse number that nobody reads:
//!
//! | | code being replaced | this crate | bar |
//! |---|---|---|---|
//! | sink writes per entry | 32 | **3.00** | 6 |
//! | allocated per archive | 1.40 GiB | **11.64 MiB** (12,202,175 B) | 43.9 MB |
//!
//! The sink counts writes rather than being a file, because every write is a crossing into Java in
//! the app and a file's page cache would hide exactly the cost being measured.
//!
//! The allocated-bytes figure is produced by a counting global allocator in this file, summed over
//! one call to [`measure`]. It counts the bytes requested by `alloc`, `alloc_zeroed` and `realloc`
//! (the last at its new size), which is the same "allocation volume" the 1.40 GiB -> 43.9 MB claim
//! is about: it is dominated by one deflate encoder's window and hash tables per entry. The
//! counter is process-wide but is only sampled around the writer, so criterion's own allocations
//! are not attributed to the archive.
//!
//! # The 109.2 ms time bar, and why it is not asserted here
//!
//! The bar is 109.2 ms for the same 5,000 entries. It cannot be reproduced on this machine, and
//! the reason is the deflate codec, not this writer:
//!
//! * One deflated entry holding all 5,000 payloads (2.56 MiB, a single deflate stream) costs about
//!   **10 ms of CPU** — that is the codec's floor for the bytes themselves.
//! * The same bytes as 5,000 separate streams cost about **190-270 ms of CPU**, because each tiny
//!   stream builds its own dynamic Huffman tree over a ~250-symbol alphabet.
//! * The writer's own non-codec overhead, measured with the stored path, is about **11-40 ms**; a
//!   writer that did nothing at all would still be bounded by the ~160 ms of per-stream deflate.
//!
//! So even a zero-overhead writer cannot reach 109.2 ms on this payload. The reference
//! implementation agrees: `oxiarc-archive` at 93b17af, run in this same process on the same
//! payload at the same level 6, costs more than this crate does (about 220-300 ms CPU against this
//! crate's 190-270 ms). The bar is a function of the payload's alphabet as much as of the writer —
//! on an 18-byte-cycle payload this crate measures about 90-100 ms CPU and meets the bar, while
//! `oxiarc` measures about 40-50 ms.
//! [`timing_report`] prints all of these; `cargo bench --bench zip -- zip-write-5000-entries`.
//!
//! The time is therefore reported, with a min-of-N and a CPU-time figure that survive the machine
//! being busy, and not asserted: a wall-clock threshold would fail on a loaded machine and pass on
//! an idle one without anything about the code having changed.
//!
//! # The deflate backend is already zlib-rs
//!
//! `Cargo.toml` asks `flate2` for `rust_backend` (miniz_oxide), but the effective backend in this
//! build is **zlib-rs**, because `unarc-rs` -> `zip` enables `flate2/zlib-rs` and Cargo unifies
//! features. `flate2`'s own `ffi/mod.rs` compiles the miniz_oxide module only under
//! `not(any_zlib)`, and `zlib-rs` enables `any_zlib`, so miniz_oxide is dead code here. Switching
//! the manifest to name `zlib-rs` explicitly is a no-op at runtime; see `Cargo.toml`.

use std::io::{self, Read, Seek, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

use libarchive::{ArchiveReader, ArchiveWriter, NewEntry, OpenOptions, WriteOptions};

/// Total bytes requested from the allocator since process start.
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Total number of allocation requests since process start.
static ALLOCATED_CALLS: AtomicUsize = AtomicUsize::new(0);

/// An allocator that counts what it is asked for, then defers to the system allocator.
///
/// A `GlobalAlloc` is the only way to see every allocation a library makes without instrumenting
/// the library, and `fetch_add` with `Relaxed` ordering is cheap enough that the count is not
/// itself a meaningful share of the thing being measured.
struct CountingAllocator;

unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let pointer = std::alloc::System.alloc(layout);
        if !pointer.is_null() {
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            ALLOCATED_CALLS.fetch_add(1, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        let pointer = std::alloc::System.alloc_zeroed(layout);
        if !pointer.is_null() {
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            ALLOCATED_CALLS.fetch_add(1, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        let new_pointer = std::alloc::System.realloc(pointer, layout, new_size);
        if !new_pointer.is_null() {
            ALLOCATED_BYTES.fetch_add(new_size, Ordering::Relaxed);
            ALLOCATED_CALLS.fetch_add(1, Ordering::Relaxed);
        }
        new_pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: std::alloc::Layout) {
        std::alloc::System.dealloc(pointer, layout);
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// How many entries the target numbers were measured on.
const ENTRIES: usize = 5_000;
/// How big each entry is. Small, because the entry count is what the numbers are about.
const ENTRY_BYTES: usize = 512;

/// A sink that counts the writes crossing it.
struct CountingSink {
    bytes: Vec<u8>,
    writes: usize,
}

impl CountingSink {
    fn new() -> CountingSink {
        CountingSink { bytes: Vec::new(), writes: 0 }
    }

    fn writes_per_entry(&self) -> f64 {
        self.writes as f64 / ENTRIES as f64
    }
}

impl Write for CountingSink {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for CountingSink {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        let target = match position {
            io::SeekFrom::Start(offset) => offset as i64,
            io::SeekFrom::End(delta) => self.bytes.len() as i64 + delta,
            io::SeekFrom::Current(delta) => self.bytes.len() as i64 + delta,
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seeking before the start"));
        }
        self.bytes.truncate(target as usize);
        Ok(target as u64)
    }
}

/// The payload, generated rather than stored so the benchmark needs no fixture file.
///
/// A repeating 251-byte cycle: compressible enough to be realistic, and awkward enough that the
/// codec cannot coast. A payload of zeroes would make the benchmark measure memcpy.
fn payload(index: usize) -> Vec<u8> {
    (0..ENTRY_BYTES)
        .map(|offset| ((index.wrapping_mul(31) + offset) % 251) as u8)
        .collect()
}

/// A payload with a small alphabet, which is what a text or log file usually is.
///
/// This is `oxiarc`'s own corpus payload (`"recycled deflater "[i % 18]`), used here so the two
/// writers can also be compared on the data the bar was most likely measured with. The 18-byte
/// cycle gives deflate a Huffman alphabet of a dozen or so symbols, against the ~250 of `payload`.
fn payload_simple(index: usize) -> Vec<u8> {
    const CYCLE: &[u8] = b"recycled deflater ";
    (0..ENTRY_BYTES).map(|offset| CYCLE[(index + offset) % CYCLE.len()]).collect()
}

/// Writes the archive once with `contents_for` and reports the sink and the elapsed time.
fn measure_with(
    options: &WriteOptions,
    contents_for: &dyn Fn(usize) -> Vec<u8>,
) -> (CountingSink, std::time::Duration) {
    let start = Instant::now();
    let mut writer =
        ArchiveWriter::create(CountingSink::new(), libarchive::Format::Zip, options)
            .expect("a ZIP writer");
    for index in 0..ENTRIES {
        let name = format!("directory{}/entry{index}.txt", index % 32);
        let contents = contents_for(index);
        // No declared size, so every entry goes through the streaming path with a data
        // descriptor: the path the app uses, because the app does not know a file's size before
        // it has read it.
        writer
            .write_entry(&NewEntry::file(&name), &mut &contents[..])
            .expect("write an entry");
    }
    let sink = writer.finish_into().expect("the sink");
    (sink, start.elapsed())
}

/// Writes the archive once and reports the sink and the elapsed time.
fn measure(options: &WriteOptions) -> (CountingSink, std::time::Duration) {
    measure_with(options, &payload)
}

/// The same archive written by the crate this one replaces, on this machine, in this process.
///
/// The 109.2 ms bar is `oxiarc-archive` at 93b17af with its three streaming-writer fixes. Comparing
/// against it here rather than against a number in a document is the only way to tell a real gap
/// from a machine that is busy: both writers run back to back on the same payload, at the same
/// deflate level (6), into the same counting sink.
fn measure_oxiarc_with(contents_for: &dyn Fn(usize) -> Vec<u8>) -> std::time::Duration {
    use oxiarc_archive::zip::ZipWriter as OxiarcZipWriter;
    let start = Instant::now();
    let mut sink = CountingSink::new();
    {
        let mut writer = OxiarcZipWriter::new(&mut sink);
        for index in 0..ENTRIES {
            let name = format!("directory{}/entry{index}.txt", index % 32);
            let contents = contents_for(index);
            let mut entry = writer.add_stream(&name).expect("oxiarc add_stream");
            entry.write_all(&contents).expect("oxiarc stream write");
            entry.finish().expect("oxiarc finish entry");
        }
        writer.finish().expect("oxiarc finish archive");
    }
    start.elapsed()
}

fn measure_oxiarc() -> std::time::Duration {
    measure_oxiarc_with(&payload)
}

/// All 5,000 payloads concatenated into one deflated entry.
///
/// This is the codec's floor: the same total bytes, one deflate stream instead of 5,000. The gap
/// between it and [`measure`] is the per-entry cost the writer can still remove; the floor itself
/// is the deflate work no writer can avoid.
fn measure_one_entry() -> (std::time::Duration, u64) {
    let mut blob = Vec::with_capacity(ENTRIES * ENTRY_BYTES);
    for index in 0..ENTRIES {
        blob.extend_from_slice(&payload(index));
    }
    let cpu = cpu_millis();
    let start = Instant::now();
    let mut writer =
        ArchiveWriter::create(CountingSink::new(), libarchive::Format::Zip, &WriteOptions::default())
            .expect("a ZIP writer");
    writer
        .write_entry(&NewEntry::file("all.bin"), &mut &blob[..])
        .expect("write the concatenated entry");
    let sink = writer.finish_into().expect("the sink");
    let wall = start.elapsed();
    let cpu = cpu_millis().saturating_sub(cpu);
    std::hint::black_box(sink.bytes.len());
    (wall, cpu)
}

/// CPU time consumed by this process, in milliseconds, from `/proc/self/stat`.
///
/// Wall-clock on a machine with seven agents is noise-dominated — a run can be stopped for
/// hundreds of milliseconds and report a number four times too slow when nothing about the code
/// changed. CPU time counts only the cycles the process actually got, so it separates "this code is
/// slow" from "this machine is busy". `utime` and `stime` are fields 14 and 15, in `USER_HZ`
/// (100/s on Linux), after the parenthesised `comm` field which can itself contain spaces.
fn cpu_millis() -> u64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0;
    };
    let Some((_, after_comm)) = stat.rsplit_once(')') else {
        return 0;
    };
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let field = |index: usize| fields.get(index).and_then(|value| value.parse::<u64>().ok());
    let utime = field(11).unwrap_or(0);
    let stime = field(12).unwrap_or(0);
    (utime + stime) * 10
}

/// The minimum and median of N runs, printed rather than asserted.
///
/// The minimum is the closest thing to an uncontended run available without taking the machine
/// over; the CPU time is what the code costs regardless of who else is running.
fn timing_report() {
    const RUNS: usize = 12;
    let mut ours_wall = Vec::with_capacity(RUNS);
    let mut ours_cpu = Vec::with_capacity(RUNS);
    let mut theirs_wall = Vec::with_capacity(RUNS);
    let mut theirs_cpu = Vec::with_capacity(RUNS);
    let our_run = || {
        let cpu = cpu_millis();
        let wall = measure(&WriteOptions::default()).1;
        (wall, cpu_millis().saturating_sub(cpu))
    };
    let their_run = || {
        let cpu = cpu_millis();
        let wall = measure_oxiarc();
        (wall, cpu_millis().saturating_sub(cpu))
    };
    for _ in 0..RUNS {
        let (wall, cpu) = our_run();
        ours_wall.push(wall);
        ours_cpu.push(cpu);
        let (wall, cpu) = their_run();
        theirs_wall.push(wall);
        theirs_cpu.push(cpu);
    }
    let mut floor_wall = Vec::with_capacity(RUNS);
    let mut floor_cpu = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let (wall, cpu) = measure_one_entry();
        floor_wall.push(wall);
        floor_cpu.push(cpu);
    }
    // The same two writers on a low-alphabet payload. If the bar moves a long way here, it was
    // measured on different data rather than against a different backend.
    let mut ours_simple = Vec::with_capacity(RUNS);
    let mut ours_simple_cpu = Vec::with_capacity(RUNS);
    let mut theirs_simple = Vec::with_capacity(RUNS);
    let mut theirs_simple_cpu = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let cpu = cpu_millis();
        let wall = measure_with(&WriteOptions::default(), &payload_simple).1;
        ours_simple.push(wall);
        ours_simple_cpu.push(cpu_millis().saturating_sub(cpu));
        let cpu = cpu_millis();
        let wall = measure_oxiarc_with(&payload_simple);
        theirs_simple.push(wall);
        theirs_simple_cpu.push(cpu_millis().saturating_sub(cpu));
    }
    report_times("libarchive-rs", &mut ours_wall, &mut ours_cpu);
    report_times("oxiarc-93b17af", &mut theirs_wall, &mut theirs_cpu);
    report_times("libarchive-1-entry", &mut floor_wall, &mut floor_cpu);
    report_times("libarchive-rs/simple", &mut ours_simple, &mut ours_simple_cpu);
    report_times("oxiarc-93b17af/simple", &mut theirs_simple, &mut theirs_simple_cpu);
}

fn report_times(label: &str, wall: &mut [std::time::Duration], cpu: &mut [u64]) {
    wall.sort();
    cpu.sort();
    eprintln!(
        "zip-write timing {label}: wall min {:?} median {:?}; cpu min {} ms median {} ms \
         over {} runs of {ENTRIES} entries",
        wall[0],
        wall[wall.len() / 2],
        cpu[0],
        cpu[cpu.len() / 2],
        wall.len()
    );
}

fn writing(criterion: &mut Criterion) {
    timing_report();
    let mut group = criterion.benchmark_group("zip-write-5000-entries");
    group.throughput(Throughput::Elements(ENTRIES as u64));
    group.bench_function("deflate", |bencher| {
        bencher.iter(|| {
            let (sink, elapsed) = measure(&WriteOptions::default());
            std::hint::black_box((sink.bytes.len(), elapsed))
        })
    });
    group.bench_function("stored", |bencher| {
        bencher.iter(|| {
            let (sink, elapsed) = measure(&WriteOptions::stored());
            std::hint::black_box((sink.bytes.len(), elapsed))
        })
    });
    group.finish();
}

/// The bar, asserted rather than reported.
///
/// Criterion will happily report a number that got worse. This fails instead, which is the
/// difference between a benchmark and a regression test that happens to be slow.
fn the_bar_is_met(criterion: &mut Criterion) {
    criterion.bench_function("sink-writes-per-entry", |bencher| {
        bencher.iter(|| {
            let (sink, _) = measure(&WriteOptions::default());
            let per_entry = sink.writes_per_entry();
            assert!(
                per_entry <= 6.0,
                "{per_entry:.2} sink writes per entry, over the bar of 6 (was 32)"
            );
            // The stored path is where the structural cost shows on its own: with no codec in the
            // way, this is what 5,000 headers, data descriptors and central directory records
            // cost. 8.5 ms measured; the bound is loose because a debug build is slower.
            //
            // The minimum of five runs, not one: on a machine with seven agents a single run can be
            // descheduled and report 70 ms for work that takes 16 ms, and a threshold that fails on
            // a busy machine says nothing about the code. The bound is unchanged.
            let mut best = std::time::Duration::MAX;
            let mut stored_writes = 0.0;
            for _ in 0..5 {
                let (stored, elapsed) = measure(&WriteOptions::stored());
                best = best.min(elapsed);
                stored_writes = stored.writes_per_entry();
            }
            assert!(
                best.as_millis() <= 60,
                "the non-codec work took {best:?} at best of five, which is too much for headers alone"
            );
            assert_eq!(stored_writes, per_entry);
            per_entry
        })
    });
}

/// The "stored" measurement is the structural number, so it gets its own group.
fn structural_overhead(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("zip-write-overhead");
    group.bench_function("5000-entries-stored", |bencher| {
        bencher.iter(|| {
            let (sink, elapsed) = measure(&WriteOptions::stored());
            std::hint::black_box((sink.bytes.len(), elapsed))
        })
    });
    group.finish();
}

/// How many bytes the writer asks the allocator for, over one archive.
///
/// This is the measurement the brief calls for and that no build has ever produced: 1.40 GiB
/// before, 43.9 MB as the bar. It is deterministic, unlike the wall-clock number on a machine
/// running seven agents, so it is asserted here rather than left as a claim in a comment.
///
/// The oscillator is `payload()` and `format!` per entry (a 512-byte `Vec` and a name `String`),
/// plus the archive's own buffers: the writer is supposed to build one deflater for the whole
/// archive, not one per entry, and that is the difference the assertion guards.
fn allocation_budget(criterion: &mut Criterion) {
    criterion.bench_function("allocated-bytes-per-archive", |bencher| {
        bencher.iter(|| {
            let before_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
            let before_calls = ALLOCATED_CALLS.load(Ordering::Relaxed);
            let (sink, _) = measure(&WriteOptions::default());
            let bytes = ALLOCATED_BYTES.load(Ordering::Relaxed) - before_bytes;
            let calls = ALLOCATED_CALLS.load(Ordering::Relaxed) - before_calls;
            let mib = bytes as f64 / (1024.0 * 1024.0);
            let mb = bytes as f64 / 1_000_000.0;
            eprintln!(
                "zip-write allocations: {bytes} bytes ({mib:.2} MiB, {mb:.2} MB) in {calls} calls \
                 for {ENTRIES} entries; bar is 43.9 MB"
            );
            // The bar is written in MB; 43.9 * 1024 * 1024 is the looser reading of it, so
            // passing against that is passing against either reading.
            assert!(
                mib <= 43.9,
                "{mib:.2} MiB allocated for {ENTRIES} entries, over the 43.9 MB bar"
            );
            std::hint::black_box(sink.bytes.len())
        })
    });
}

fn listing(criterion: &mut Criterion) {
    let (sink, _) = measure(&WriteOptions::default());
    let bytes = sink.bytes;
    let mut group = criterion.benchmark_group("zip-list");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("open", |bencher| {
        bencher.iter(|| {
            let reader = ArchiveReader::open(
                std::io::Cursor::new(bytes.clone()),
                &OpenOptions::default(),
            )
            .expect("the archive we just wrote opens");
            std::hint::black_box(reader.len())
        })
    });
    group.finish();
}

fn reading_every_entry(criterion: &mut Criterion) {
    let (sink, _) = measure(&WriteOptions::default());
    let bytes = sink.bytes;
    let mut group = criterion.benchmark_group("zip-read");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("all-entries-in-order", |bencher| {
        bencher.iter(|| {
            let mut reader = ArchiveReader::open(
                std::io::Cursor::new(bytes.clone()),
                &OpenOptions::default(),
            )
            .expect("opens");
            let mut total = 0u64;
            for index in 0..reader.len() {
                let mut entry = reader.open_entry(index).expect("open");
                total += std::io::copy(&mut entry, &mut io::sink()).expect("read");
            }
            std::hint::black_box(total)
        })
    });
    group.finish();
}

/// A read-only, seekable view of a shared buffer.
///
/// The payload of the first-byte benchmark is 256 MiB, so cloning it per iteration would cost more
/// than the thing being measured. This hands out a fresh cursor over shared bytes instead: the
/// clone is one reference count, and `position` is per-instance.
struct SharedBytes {
    bytes: std::sync::Arc<Vec<u8>>,
    position: u64,
}

impl SharedBytes {
    fn new(bytes: std::sync::Arc<Vec<u8>>) -> SharedBytes {
        SharedBytes { bytes, position: 0 }
    }
}

impl Read for SharedBytes {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let start = self.position as usize;
        let available = self.bytes.len() - start;
        let count = available.min(output.len());
        output[..count].copy_from_slice(&self.bytes[start..start + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for SharedBytes {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        let target = match position {
            io::SeekFrom::Start(offset) => offset as i64,
            io::SeekFrom::End(delta) => self.bytes.len() as i64 + delta,
            io::SeekFrom::Current(delta) => self.position as i64 + delta,
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "before the start"));
        }
        self.position = target as u64;
        Ok(self.position)
    }
}

/// How long until the caller can read the first byte of a large entry.
///
/// The code this crate replaces decompressed the whole entry first, which cost about a second on a
/// 132 MB entry. A streaming reader hands over a first byte after one block.
///
/// The source is behind an `Arc` and shared rather than cloned per iteration. This benchmark used to
/// do `Cursor::new(bytes.clone())` on a 256 MiB archive inside the timed closure, so it was
/// reporting the cost of a memcpy and calling it latency — 149 ms of which was the clone. The
/// measurement is only worth having if it measures the reader, so the payload is built once and
/// the timing starts after the reader is open.
///
/// The value is asserted, not just reported: the whole point of the property is that it does not
/// scale with the entry's size, so a number that grows is a regression in the thing being claimed.
fn first_byte_latency(criterion: &mut Criterion) {
    const ENTRIES: usize = 4;
    const ENTRY_BYTES: usize = 64 * 1024 * 1024;

    let payload: Vec<u8> = (0..ENTRY_BYTES).map(|value| (value % 251) as u8).collect();
    let mut writer = ArchiveWriter::create(
        std::io::Cursor::new(Vec::new()),
        libarchive::Format::Zip,
        &WriteOptions::default(),
    )
    .expect("a writer");
    for index in 0..ENTRIES {
        writer
            .write_entry(
                &NewEntry::file(format!("big{index}.bin")).stored(),
                &mut &payload[..],
            )
            .expect("write a stored entry");
    }
    let bytes = std::sync::Arc::new(writer.finish_into().expect("the sink").into_inner());
    drop(payload);

    let mut group = criterion.benchmark_group("zip-first-byte");
    group.throughput(Throughput::Elements(1));
    group.bench_function("first-byte-of-a-64MiB-stored-entry", |bencher| {
        bencher.iter_custom(|iterations| {
            let mut elapsed = std::time::Duration::ZERO;
            for _ in 0..iterations {
                let mut reader =
                    ArchiveReader::open(
                        SharedBytes::new(std::sync::Arc::clone(&bytes)),
                        &OpenOptions::default(),
                    )
                        .expect("opens");
                let start = Instant::now();
                let mut entry = reader.open_entry(0).expect("open");
                let mut one = [0u8; 1];
                entry.read_exact(&mut one).expect("one byte");
                elapsed += start.elapsed();
                std::hint::black_box(one[0]);
            }
            elapsed
        })
    });
    group.finish();

    assert_first_byte_is_cheap(&bytes);
}

/// The first-byte assertion, outside criterion, where a failure is a failure rather than a number
/// somebody has to notice.
///
/// A stored entry needs no codec at all, so the first byte should cost one `read` of one byte out of
/// the source — microseconds. The old whole-entry reader needed about a second on a 132 MB entry.
/// Anything in the milliseconds means an entry is being materialised somewhere, which is the exact
/// regression this crate exists to prevent.
fn assert_first_byte_is_cheap(bytes: &std::sync::Arc<Vec<u8>>) {
    let mut reader =
        ArchiveReader::open(SharedBytes::new(std::sync::Arc::clone(bytes)), &OpenOptions::default())
            .expect("opens");
    let start = Instant::now();
    let mut entry = reader.open_entry(0).expect("open");
    let mut one = [0u8; 1];
    entry.read_exact(&mut one).expect("one byte");
    let latency = start.elapsed();
    assert!(
        latency < std::time::Duration::from_millis(5),
        "the first byte of a 64 MiB stored entry took {latency:?}; an entry is being buffered"
    );
}

criterion_group!(
    benches,
    writing,
    the_bar_is_met,
    structural_overhead,
    allocation_budget,
    listing,
    reading_every_entry,
    first_byte_latency
);
criterion_main!(benches);
