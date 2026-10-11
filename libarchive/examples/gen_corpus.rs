//! Generates the corpus files no CLI on this machine can write, and a manifest fragment for
//! them. Driven by `python3 tests/corpus/generate.py`, which merges the fragment into
//! `manifest.json`; run it standalone with `cargo run -p libarchive --example gen_corpus`.
//!
//! What lands here and why:
//!
//! * `.lz4` and `.br` — the `lz4` and `brotli` CLIs are not installed; the crates are.
//! * `.tar.lz4` — same, for the compressed-TAR case.
//! * `.lzh` — no `lha`/`lhasa` CLI exists here and unarc-rs ships no LZH fixture; oxiarc's
//!   `LzhWriter` is the writer the app itself used, so it is the most honest generator there is.
//! * `.rar` — no `rar`/`unrar` CLI exists here. `rars` is the same builder unarc-rs's own test
//!   suite uses (unarc-rs-0.7.2/tests/rar_decompression.rs:10-48), so every file this writes is
//!   provably readable by the oracle.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

/// The same plaintext `generate.py` stages, in the same order. The CRCs in the fragment are
/// computed here from these bytes, so they agree with the python side by construction.
fn hello() -> &'static [u8] {
    b"Hello, libarchive-rs!\n"
}

fn data_bin() -> Vec<u8> {
    (0..4096u32).map(|i| ((i * 31 + 7) % 256) as u8).collect()
}

fn inner() -> &'static [u8] {
    b"nested entry\n"
}

fn plaintext() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("hello.txt", hello().to_vec()),
        ("data.bin", data_bin()),
        ("empty.txt", Vec::new()),
        ("nested/inner.txt", inner().to_vec()),
    ]
}

fn crc(bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

#[derive(Serialize)]
struct Fragment {
    files: Vec<Record>,
}

#[derive(Serialize)]
struct Record {
    path: String,
    format: String,
    status: String,
    owner: Option<String>,
    password: Option<String>,
    made_with: String,
    expect: Expect,
    entries: Option<Vec<Entry>>,
    oracles: Vec<String>,
    notes: String,
}

#[derive(Serialize)]
struct Expect {
    kind: String,
    error_kind: Option<String>,
}

#[derive(Clone, Serialize)]
struct Entry {
    name: String,
    size: u64,
    crc32: u32,
    dir: bool,
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let generated = root.join("generated");
    for sub in ["streams", "lzh", "rar"] {
        std::fs::create_dir_all(generated.join(sub)).expect("create the output directory");
    }
    let mut records = Vec::new();

    // --- lz4 frame and brotli: one stream, one entry, over data.bin. ---
    let payload = data_bin();
    let single = vec![Entry {
        name: "content".to_owned(),
        size: payload.len() as u64,
        crc32: crc(&payload),
        dir: false,
    }];

    let lz4_path = generated.join("streams/payload.lz4");
    let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
    encoder.write_all(&payload).expect("lz4 compress");
    let lz4_bytes = encoder.finish().expect("lz4 finish");
    std::fs::write(&lz4_path, &lz4_bytes).expect("write payload.lz4");
    records.push(Record {
        path: "generated/streams/payload.lz4".to_owned(),
        format: "lz4".to_owned(),
        status: "gate".to_owned(),
        owner: None,
        password: None,
        made_with: "cargo run -p libarchive --example gen_corpus (lz4_flex frame encoder)"
            .to_owned(),
        expect: Expect {
            kind: "ok".to_owned(),
            error_kind: None,
        },
        entries: Some(single.clone()),
        oracles: vec!["oxiarc".to_owned()],
        notes: String::new(),
    });

    let br_path = generated.join("streams/payload.br");
    let mut br_encoded = Vec::new();
    {
        let mut compressor = brotli::CompressorWriter::new(&mut br_encoded, 4096, 5, 22);
        compressor.write_all(&payload).expect("brotli compress");
    }
    std::fs::write(&br_path, &br_encoded).expect("write payload.br");
    records.push(Record {
        path: "generated/streams/payload.br".to_owned(),
        format: "br".to_owned(),
        status: "gate".to_owned(),
        owner: None,
        password: None,
        made_with: "cargo run -p libarchive --example gen_corpus (brotli CompressorWriter)"
            .to_owned(),
        expect: Expect {
            kind: "ok".to_owned(),
            error_kind: None,
        },
        entries: Some(single.clone()),
        // oxiarc reads brotli too; unarc-rs has no brotli at all.
        oracles: vec!["oxiarc".to_owned()],
        notes: String::from(
            "brotli has no magic of its own; the manifest's format drives open_as, and \
             detect() cannot find it in the wild.",
        ),
    });

    // --- tar.lz4: the ustar.tar generate.py already wrote, wrapped in an lz4 frame. ---
    let tar_bytes = std::fs::read(generated.join("tar/ustar.tar"))
        .expect("generate.py must run first: tar/ustar.tar");
    let mut tar_lz4 = lz4_flex::frame::FrameEncoder::new(Vec::new());
    tar_lz4.write_all(&tar_bytes).expect("lz4 compress the tar");
    let tar_lz4_bytes = tar_lz4.finish().expect("lz4 finish");
    std::fs::write(generated.join("streams/plain.tar.lz4"), &tar_lz4_bytes).expect("write tar.lz4");
    let tar_entries: Vec<Entry> = plaintext()
        .iter()
        .map(|(name, bytes)| Entry {
            name: (*name).to_owned(),
            size: bytes.len() as u64,
            crc32: crc(bytes),
            dir: false,
        })
        .collect();
    records.push(Record {
        path: "generated/streams/plain.tar.lz4".to_owned(),
        format: "tar.lz4".to_owned(),
        status: "gate".to_owned(),
        owner: None,
        password: None,
        made_with:
            "cargo run -p libarchive --example gen_corpus (lz4_flex frame over tar/ustar.tar)"
                .to_owned(),
        expect: Expect {
            kind: "ok".to_owned(),
            error_kind: None,
        },
        entries: Some(tar_entries.clone()),
        oracles: Vec::new(),
        notes: String::from(
            "no oracle here reads tar.lz4: unarc has no lz4, oxiarc has no compressed-TAR \
             wrapper. The manifest's plaintext CRCs are the ground truth.",
        ),
    });

    // --- LZH: oxiarc's LzhWriter, stored and LH5, the writer the app itself used. ---
    for (file_name, level, label) in [
        (
            "stored.lzh",
            oxiarc_archive::lzh::LzhCompressionLevel::Store,
            "lh0 (store)",
        ),
        (
            "lh5.lzh",
            oxiarc_archive::lzh::LzhCompressionLevel::Lh5,
            "lh5",
        ),
    ] {
        let mut writer = oxiarc_archive::lzh::LzhWriter::new(Vec::new());
        writer.set_compression(level);
        let mut entries = Vec::new();
        for (name, bytes) in &plaintext() {
            writer.add_file(name, bytes).expect("lzh add_file");
            entries.push(Entry {
                name: (*name).to_owned(),
                size: bytes.len() as u64,
                crc32: crc(bytes),
                dir: false,
            });
        }
        let bytes = writer.into_inner().expect("lzh finish");
        std::fs::write(generated.join("lzh").join(file_name), &bytes).expect("write the lzh");
        records.push(Record {
            path: format!("generated/lzh/{file_name}"),
            format: "lzh".to_owned(),
            status: "pending".to_owned(),
            owner: Some("legacy".to_owned()),
            password: None,
            made_with: format!(
                "cargo run -p libarchive --example gen_corpus (oxiarc LzhWriter 93b17af, {label})"
            ),
            expect: Expect {
                kind: "error".to_owned(),
                error_kind: None,
            },
            entries: Some(entries),
            oracles: vec!["oxiarc".to_owned()],
            notes: String::from(
                "LZH lists through the legacy path, but streaming any entry fails \
                 \"while decompressing: unsupported compression method\" — including the \
                 stored LH0 file, so it is the method mapping, not the codec. Owner: legacy. \
                 oxiarc is the second oracle because it is the writer too.",
            ),
        });
    }

    // --- RAR via rars, copying unarc-rs's own builder pattern. ---
    let rar_files: Vec<(Vec<u8>, Vec<u8>)> = plaintext()
        .into_iter()
        .map(|(name, bytes)| (name.as_bytes().to_vec(), bytes))
        .collect();
    let rar_entries: Vec<Entry> = rar_files
        .iter()
        .map(|(name, bytes)| Entry {
            name: String::from_utf8(name.clone()).expect("ascii names"),
            size: bytes.len() as u64,
            crc32: crc(bytes),
            dir: false,
        })
        .collect();
    let rar_cases: Vec<(&str, rars::ArchiveVersion, bool, bool, &str)> = vec![
        (
            "rar15.rar",
            rars::ArchiveVersion::Rar15,
            false,
            false,
            "RAR 1.5",
        ),
        (
            "rar29.rar",
            rars::ArchiveVersion::Rar29,
            false,
            false,
            "RAR 2.9",
        ),
        (
            "rar50.rar",
            rars::ArchiveVersion::Rar50,
            false,
            false,
            "RAR 5.0",
        ),
        (
            "rar50-solid.rar",
            rars::ArchiveVersion::Rar50,
            true,
            false,
            "RAR 5.0 solid",
        ),
        (
            "rar29-encrypted.rar",
            rars::ArchiveVersion::Rar29,
            false,
            true,
            "RAR 2.9, entry encryption",
        ),
    ];
    for (file_name, version, solid, encrypted, label) in rar_cases {
        let mut builder = rars::Builder::new(version).solid(solid);
        if encrypted {
            builder = builder.password(Some(b"secret".to_vec()));
        }
        for (name, bytes) in &rar_files {
            builder
                .add_bytes(name.clone(), bytes.clone(), None, None)
                .expect("rars add_bytes");
        }
        let bytes = builder.to_bytes().expect("rars to_bytes");
        std::fs::write(generated.join("rar").join(file_name), &bytes).expect("write the rar");
        // Measured by the sweep on 2026-10-11: the unencrypted fixtures read here exactly as
        // unarc reads them, so they gate. The encrypted one cannot: formats::legacy::open drops
        // OpenOptions on the floor (it calls unarc's open_with_format, not
        // open_with_format_and_options), so the password never reaches the backend and the read
        // fails Err(Password) even with the right password.
        let (status, expect, notes) =
            if encrypted {
                (
                "pending",
                Expect { kind: "error".to_owned(), error_kind: Some("Password".to_owned()) },
                "legacy::open does not pass OpenOptions through to unarc, so the password never \
                 reaches the backend: Err(Password) even with the right one. Owner: legacy.",
            )
            } else {
                (
                    "gate",
                    Expect {
                        kind: "ok".to_owned(),
                        error_kind: None,
                    },
                    "this crate reads it; the differential sweep compares name, size and bytes \
                 against unarc",
                )
            };
        records.push(Record {
            path: format!("generated/rar/{file_name}"),
            format: "rar".to_owned(),
            status: status.to_owned(),
            owner: if encrypted {
                Some("legacy".to_owned())
            } else {
                None
            },
            password: if encrypted {
                Some("secret".to_owned())
            } else {
                None
            },
            made_with: format!(
                "cargo run -p libarchive --example gen_corpus (rars Builder, {label})"
            ),
            expect,
            entries: Some(rar_entries.clone()),
            oracles: vec!["unarc".to_owned()],
            notes: notes.to_owned(),
        });
    }

    let fragment = Fragment { files: records };
    let json = serde_json::to_string_pretty(&fragment).expect("serialise the fragment");
    std::fs::write(
        generated.join("rust-manifest-fragment.json"),
        format!("{json}\n"),
    )
    .expect("write the fragment");
    println!(
        "gen_corpus: wrote lz4, brotli, tar.lz4, lzh and rar fixtures plus \
         generated/rust-manifest-fragment.json"
    );
}
