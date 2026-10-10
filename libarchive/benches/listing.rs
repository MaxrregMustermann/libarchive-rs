//! How fast is listing, and how much of the archive does it touch?
//!
//! The number that matters for the app is not just the time but the bytes: a listing that reads
//! 54.6 MB of a 76 MiB archive is slower than one that reads 411 KB, and the difference is a
//! `BufReader` whose 8 KiB refill is thrown away fifteen times out of sixteen.

use criterion::{criterion_group, criterion_main, Criterion};

/// A source that counts the bytes read out of it.
///
/// This is the stand-in for the app's Java channel, where every read crosses the JNI boundary.
struct CountingSource {
    bytes: Vec<u8>,
    position: usize,
    read: usize,
    reads: usize,
}

impl CountingSource {
    fn new(bytes: Vec<u8>) -> CountingSource {
        CountingSource { bytes, position: 0, read: 0, reads: 0 }
    }
}

impl std::io::Read for CountingSource {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let available = self.bytes.len() - self.position;
        let count = available.min(output.len());
        output[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
        self.position += count;
        self.read += count;
        self.reads += 1;
        Ok(count)
    }
}

impl std::io::Seek for CountingSource {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        let target = match position {
            std::io::SeekFrom::Start(offset) => offset as i64,
            std::io::SeekFrom::End(delta) => self.bytes.len() as i64 + delta,
            std::io::SeekFrom::Current(delta) => self.position as i64 + delta,
        };
        if target < 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "before the start"));
        }
        self.position = target as usize;
        Ok(target as u64)
    }
}

/// How many bytes the source has handed out so far.
fn read_so_far(source: &libarchive::Shared<CountingSource>) -> usize {
    libarchive::reader::lock(source).map(|guard| guard.read).unwrap_or(usize::MAX)
}

/// Builds an archive with `entries` entries and returns its bytes.
fn build(entries: usize) -> Vec<u8> {
    let options = libarchive::WriteOptions::default();
    let mut writer = libarchive::ArchiveWriter::create(
        std::io::Cursor::new(Vec::new()),
        libarchive::Format::Zip,
        &options,
    )
    .expect("a writer");
    for index in 0..entries {
        let name = format!("dir{}/file{index}.txt", index % 16);
        let contents: Vec<u8> = (0..4096).map(|offset| ((index + offset) % 251) as u8).collect();
        writer
            .write_entry(&libarchive::NewEntry::file(&name), &mut &contents[..])
            .expect("write");
    }
    writer.finish_into().expect("finish").into_inner()
}

fn listing_reads_little(criterion: &mut Criterion) {
    let bytes = build(3_355);
    let size = bytes.len();
    let mut group = criterion.benchmark_group("zip-listing");
    group.throughput(criterion::Throughput::Bytes(size as u64));
    group.bench_function("bytes-touched", |bencher| {
        bencher.iter(|| {
            let source = libarchive::reader::share(CountingSource::new(bytes.clone()));
            let reader = libarchive::ArchiveReader::open(
                libarchive::reader::share(CountingSource::new(bytes.clone())),
                &libarchive::OpenOptions::default(),
            )
            .expect("opens");
            std::hint::black_box((reader.len(), read_so_far(&source)))
        })
    });
    group.finish();
}

/// The assertion, which is the part that matters.
///
/// A listing that reads a large fraction of the archive is the `BufReader` bug, and a benchmark
/// that only reports a time does not catch it.
fn listing_stays_small(criterion: &mut Criterion) {
    criterion.bench_function("listing-reads-at-most-1MiB", |bencher| {
        bencher.iter(|| {
            let bytes = build(3_355);
            let source = libarchive::reader::share(CountingSource::new(bytes.clone()));
            let reader = libarchive::ArchiveReader::open(
                source.clone(),
                &libarchive::OpenOptions::default(),
            )
            .expect("opens");
            assert_eq!(reader.len(), 3_355);
            let read = read_so_far(&source);
            assert!(
                read <= 1024 * 1024,
                "listing {} entries read {} bytes of a {} byte archive",
                reader.len(),
                read,
                bytes.len()
            );
            read
        })
    });
}

criterion_group!(benches, listing_reads_little, listing_stays_small);
criterion_main!(benches);
