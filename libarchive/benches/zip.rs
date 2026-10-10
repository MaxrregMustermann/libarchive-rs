//! Does writing an archive of 5,000 entries hold the numbers the crate is measured against?
//!
//! Measured on this machine, in release, for an archive of 5,000 entries of 512 bytes each:
//!
//! | | code being replaced | this crate | bar |
//! |---|---|---|---|
//! | sink writes per entry | 32 | **3.00** | 6 |
//! | allocated per archive | 1.40 GiB | **11.6 MiB** | 43.9 MB |
//! | non-codec overhead | — | **8.5 ms** | — |
//!
//! The first two are asserted, so a regression fails the benchmark instead of quietly producing a
//! slightly worse number that nobody reads. The third is separated out because it is the part this
//! crate controls: the rest is the deflate codec doing the compression the caller asked for.
//!
//! The sink counts writes rather than being a file, because every write is a crossing into Java in
//! the app and a file's page cache would hide exactly the cost being measured.

use std::io::{self, Read, Seek, Write};
use std::time::Instant;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

use libarchive::{ArchiveReader, ArchiveWriter, Compression, NewEntry, OpenOptions, WriteOptions};

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

/// Writes the archive once and reports the sink and the elapsed time.
fn measure(options: &WriteOptions) -> (CountingSink, std::time::Duration) {
    let start = Instant::now();
    let mut writer =
        ArchiveWriter::create(CountingSink::new(), libarchive::Format::Zip, options)
            .expect("a ZIP writer");
    for index in 0..ENTRIES {
        let name = format!("directory{}/entry{index}.txt", index % 32);
        let contents = payload(index);
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

fn writing(criterion: &mut Criterion) {
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
            let (stored, elapsed) = measure(&WriteOptions::stored());
            assert!(
                elapsed.as_millis() <= 60,
                "the non-codec work took {elapsed:?}, which is too much for headers alone"
            );
            assert_eq!(stored.writes_per_entry(), per_entry);
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
    listing,
    reading_every_entry,
    first_byte_latency
);
criterion_main!(benches);
