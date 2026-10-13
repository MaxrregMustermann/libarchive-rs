//! Temporary probe: writes every format/case pair so the external tools can be tried by hand.
//! Deleted before the work lands.

use std::fs;
use std::io::{Cursor, Read};
use std::path::PathBuf;

use libarchive::entry::EntryKind;
use libarchive::format::{Compression, Format};
use libarchive::writer::{ArchiveWriter, NewEntry, WriteOptions};

struct Spec {
    name: &'static str,
    dir: bool,
    contents: Vec<u8>,
}

fn cases() -> Vec<(&'static str, Vec<Spec>)> {
    vec![
        ("empty", Vec::new()),
        ("zero", vec![Spec { name: "zero.txt", dir: false, contents: Vec::new() }]),
        (
            "utf8",
            vec![Spec {
                name: "日本-файл-\u{1F600}-ünïcode.txt",
                dir: false,
                contents: (0..3000).map(|i| (i % 251) as u8).collect(),
            }],
        ),
        (
            "nested",
            vec![
                Spec { name: "nested", dir: true, contents: Vec::new() },
                Spec { name: "nested/dir", dir: true, contents: Vec::new() },
                Spec {
                    name: "nested/dir/leaf.txt",
                    dir: false,
                    contents: (0..5000).map(|i| (i % 7) as u8).collect(),
                },
            ],
        ),
        (
            "longname",
            vec![Spec {
                name: "a-very-long-directory-name-that-goes-well-past-the-one-hundred-byte-limit-of-the-traditional-ustar-name-field/leaf.txt",
                dir: false,
                contents: (0..300).map(|i| (i % 11) as u8).collect(),
            }],
        ),
        (
            "spanning",
            vec![Spec {
                name: "spanning.bin",
                dir: false,
                contents: (0..libarchive::writer::COPY_BUFFER_SIZE * 4 + 1234)
                    .map(|i| (i % 13) as u8)
                    .collect(),
            }],
        ),
    ]
}

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).expect("an output directory"));
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(&out).expect("make the directory");
    let formats = [
        Format::Tar,
        Format::CompressedTar(Compression::Gzip),
        Format::CompressedTar(Compression::Bzip2),
        Format::CompressedTar(Compression::Xz),
        Format::CompressedTar(Compression::Zstd),
        Format::CompressedTar(Compression::Lz4),
        Format::Zip,
        Format::SevenZ,
        Format::SingleStream(Compression::Gzip),
        Format::SingleStream(Compression::Bzip2),
        Format::SingleStream(Compression::Xz),
        Format::SingleStream(Compression::Zstd),
        Format::SingleStream(Compression::Lz4),
        Format::SingleStream(Compression::Brotli),
    ];
    for format in formats {
        for (label, specs) in cases() {
            if let Format::SingleStream(_) = format {
                if label == "utf8" || label == "nested" {
                    continue;
                }
            }
            let options = WriteOptions::default().with_compression(match format {
                Format::Tar | Format::Zip | Format::SevenZ => Compression::None,
                Format::CompressedTar(c) | Format::SingleStream(c) => c,
                _ => Compression::None,
            });
            let mut writer = ArchiveWriter::create(Cursor::new(Vec::new()), format, &options)
                .unwrap_or_else(|e| panic!("{format}: {e}"));
            match format {
                Format::SingleStream(_) => {
                    if let Some(spec) = specs.first() {
                        writer
                            .write_entry_from_slice(
                                &NewEntry::file(spec.name).with_size(spec.contents.len() as u64),
                                &spec.contents,
                            )
                            .unwrap_or_else(|e| { println!("{format}/{label} WRITER REFUSED: {e}"); return; });
                    }
                }
                _ => {
                    for spec in &specs {
                        let entry = if spec.dir {
                            NewEntry::directory(spec.name).with_size(0)
                        } else {
                            NewEntry::file(spec.name).with_size(spec.contents.len() as u64)
                        };
                        writer
                            .write_entry(&entry, &mut &spec.contents[..])
                            .unwrap_or_else(|e| { println!("{format}/{label} WRITER REFUSED: {e}"); return; });
                    }
                }
            }
            let bytes = writer.finish_into().expect("finish").into_inner();
            let name = format!("{}.{}", label, format.extension());
            let _ = &name;
            fs::write(out.join(&name), &bytes).expect("write");
            println!("{:<24} {:>8} bytes", name, bytes.len());
        }
    }
    let _ = EntryKind::File;
    // Second pass: read the UTF-8-named archives back with this crate's own reader.
    for ext in ["tar", "zip", "7z", "tar.gz", "tar.bz2", "tar.xz", "tar.zst", "tar.lz4"] {
        let path = out.join(format!("utf8.{ext}"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let format = match ext {
            "tar.gz" => Some(Format::CompressedTar(Compression::Gzip)),
            "tar.bz2" => Some(Format::CompressedTar(Compression::Bzip2)),
            "tar.xz" => Some(Format::CompressedTar(Compression::Xz)),
            "tar.zst" => Some(Format::CompressedTar(Compression::Zstd)),
            "tar.lz4" => Some(Format::CompressedTar(Compression::Lz4)),
            _ => None,
        };
        let mut reader = match format {
            Some(format) => libarchive::ArchiveReader::open_as(
                Cursor::new(bytes),
                format,
                &libarchive::OpenOptions::default(),
            )
            .unwrap_or_else(|e| panic!("{ext}: {e}")),
            None => libarchive::ArchiveReader::open(
                Cursor::new(bytes),
                &libarchive::OpenOptions::default(),
            )
            .unwrap_or_else(|e| panic!("{ext}: {e}")),
        };
        let mut names = Vec::new();
        for index in 0..reader.len() {
            let name = reader.entry(index).expect("entry").name.clone();
            let mut contents = Vec::new();
            reader
                .open_entry(index)
                .expect("open an entry")
                .read_to_end(&mut contents)
                .expect("read an entry");
            names.push((name, contents.len()));
        }
        println!("read back utf8.{ext}: {names:?}");
    }
}
