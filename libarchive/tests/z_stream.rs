//! Unix compress (`.Z`) through the public API, against fixtures no code in this
//! repository produced.
//!
//! The four files in `data/z/` were emitted by an independent LZW encoder written for the
//! purpose and verified byte-identical through the real `gzip -dc` and `7z x` before check-in:
//! `empty` (header only), `text` (matches, one width), `rot` (width growth to 11 bits), `big`
//! (70 KiB of pseudo-random bytes: a full table, CLEAR codes, codes at every width to 16 bits).
//! What the tests assert is that this crate decodes those same bytes — and, when the tools are
//! present, that all three still agree with each other.
//!
//! Plaintexts are regenerated in-test from the recipe, so only the `.Z` bytes are checked in.

use std::io::Read;
use std::path::{Path, PathBuf};

use libarchive::{ArchiveReader, Compression, Format, OpenOptions};

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/z")
        .join(name)
}

/// The plaintext behind each fixture, from the generation recipe.
fn plaintext(name: &str) -> Vec<u8> {
    match name {
        "empty" => Vec::new(),
        "text" => b"The Unix compress format packs LZW codes LSB-first into blocks. "
            .repeat(100)
            .to_vec(),
        "rot" => (0..6000u32).map(|value| (value % 256) as u8).collect(),
        "big" => {
            let mut state = 0x243F6A88u32;
            (0..70_000)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    (state & 0xFF) as u8
                })
                .collect()
        }
        _ => panic!("unknown fixture {name}"),
    }
}

fn tool(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
    })
}

#[test]
fn every_fixture_decodes_to_its_plaintext() {
    for name in ["empty", "text", "rot", "big"] {
        let bytes = std::fs::read(data(&format!("{name}.Z"))).expect("fixture");
        let mut reader = ArchiveReader::open_as(
            std::io::Cursor::new(bytes),
            Format::SingleStream(Compression::Z),
            &OpenOptions::default(),
        )
        .expect("a .Z stream opens");
        assert_eq!(reader.len(), 1, "{name} is one entry");
        // A `.Z` records no size, so the entry says so rather than inventing one.
        assert_eq!(
            reader.entry(0).and_then(|entry| entry.size),
            None,
            "{name} has no recorded size"
        );
        let mut got = Vec::new();
        reader
            .open_entry(0)
            .expect("open")
            .read_to_end(&mut got)
            .expect("read");
        assert_eq!(got, plaintext(name), "{name} did not decode");
    }
}

#[test]
fn a_dot_z_file_is_detected_as_a_single_z_stream() {
    use libarchive::detect_header;
    for name in ["empty", "text", "rot", "big"] {
        let bytes = std::fs::read(data(&format!("{name}.Z"))).expect("fixture");
        let header: Vec<u8> = bytes.iter().copied().take(512).collect();
        assert_eq!(
            detect_header(&header),
            Format::SingleStream(Compression::Z),
            "{name} should detect"
        );
    }
    // And opening by detection, the way the app does, lands on the same entry.
    let bytes = std::fs::read(data("text.Z")).expect("fixture");
    let reader =
        ArchiveReader::open(std::io::Cursor::new(bytes), &OpenOptions::default()).expect("detect");
    assert_eq!(reader.format(), Format::SingleStream(Compression::Z));
    assert_eq!(reader.len(), 1);
}

#[test]
fn a_truncated_stream_opens_on_its_valid_header_and_fails_on_read() {
    // The header is fine but the codes stop after two bytes. `open` cannot know — no size is
    // recorded anywhere — so it succeeds and the read fails, exactly like a truncated gzip.
    // What must not happen is silence: no bytes, no error.
    let mut reader = ArchiveReader::open_as(
        std::io::Cursor::new(vec![0x1F, 0x9D, 0x90, 0xFF, 0xFF]),
        Format::SingleStream(Compression::Z),
        &OpenOptions::default(),
    )
    .expect("the header is valid");
    assert_eq!(reader.len(), 1);
    let mut got = Vec::new();
    let result = reader.open_entry(0).expect("open").read_to_end(&mut got);
    assert!(
        result.is_err(),
        "truncated codes must fail the read, not decode as content: {got:?}"
    );
}

#[test]
fn a_wrong_magic_is_malformed_at_open() {
    let error = match ArchiveReader::open_as(
        std::io::Cursor::new(b"gzip magic here\x1f\x8b\x08".to_vec()),
        Format::SingleStream(Compression::Z),
        &OpenOptions::default(),
    ) {
        Ok(_) => panic!("gzip bytes are not a .Z stream"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), libarchive::ErrorKind::Malformed);
}

#[test]
fn the_tools_still_agree_with_this_crate_on_every_fixture() {
    // The independent check, re-run whenever the tools are present: `gzip -dc` and `7z x`
    // decode the checked-in fixtures to the same plaintexts this crate produces. If neither
    // tool is installed the test passes having asserted nothing new — the checked-in bytes were
    // verified at generation time either way.
    let gzip = tool("gzip");
    let sevenz = tool("7z");
    if gzip.is_none() && sevenz.is_none() {
        eprintln!("neither gzip nor 7z found; skipping the cross-tool check");
        return;
    }
    for name in ["empty", "text", "rot", "big"] {
        let fixture = data(&format!("{name}.Z"));
        let want = plaintext(name);
        if let Some(gzip) = gzip.as_ref() {
            let out = std::process::Command::new(gzip)
                .arg("-dc")
                .arg(&fixture)
                .output()
                .expect("gzip runs");
            assert!(out.status.success(), "gzip -dc {name}.Z: {out:?}");
            assert_eq!(out.stdout, want, "gzip disagrees on {name}");
        }
        if let Some(sevenz) = sevenz.as_ref() {
            let out = std::process::Command::new(sevenz)
                .arg("x")
                .arg("-so")
                .arg(&fixture)
                .output()
                .expect("7z runs");
            assert!(out.status.success(), "7z x {name}.Z: {out:?}");
            assert_eq!(out.stdout, want, "7z disagrees on {name}");
        }
    }
}
