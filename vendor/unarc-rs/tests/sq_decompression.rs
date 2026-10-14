use std::io::Cursor;

use unarc_rs::sq::sq_archive::SqArchive;
use unarc_rs::unified::{ArchiveFormat, UnifiedArchive};

#[test]
fn extract_sq() {
    let file = Cursor::new(include_bytes!("qqq/license.sq"));
    let mut archive = SqArchive::new(file).unwrap();
    let entry = archive.get_next_entry().unwrap().unwrap();
    assert_eq!("license", entry.name);
    let result = archive.read(&entry).unwrap();
    assert_eq!(include_bytes!("../../../LICENSE"), result.as_slice());
}

#[test]
fn extract_sq2() {
    let file = Cursor::new(include_bytes!("qqq/license.sq2"));
    let mut archive = SqArchive::new(file).unwrap();
    let entry = archive.get_next_entry().unwrap().unwrap();
    // seems to be an error in the cmd line client with files with no extension :)
    assert_eq!(".LIC", entry.name);
    let result = archive.read(&entry).unwrap();
    assert_eq!(include_bytes!("../../../LICENSE"), result.as_slice());
}

/// J-0038: the listed size must equal the bytes the entry actually produces.
///
/// The SQ format records no size in its header, so the size can only be known by decompressing.
/// Both fixtures hold the same 11357-byte LICENSE; the unified listing must report that number,
/// not the 0 it reported before the fix (measured: `original_size()` was 0 while `read()`
/// produced 11357 bytes).
#[test]
fn sq_listing_reports_the_size_the_entry_streams() {
    for (fixture, bytes) in [
        ("qqq/license.sq", include_bytes!("qqq/license.sq").as_slice()),
        ("qqq/license.sq2", include_bytes!("qqq/license.sq2").as_slice()),
    ] {
        let mut archive =
            UnifiedArchive::open_with_format(Cursor::new(bytes), ArchiveFormat::Sq).unwrap();
        let entry = archive.next_entry().unwrap().unwrap();
        let listed = entry.original_size();
        let streamed = archive.read(&entry).unwrap().len() as u64;
        assert_eq!(
            listed, streamed,
            "{fixture}: listed size must equal the bytes the entry streams"
        );
        assert_eq!(11_357, streamed, "{fixture}: entry must stream the LICENSE bytes");
        assert!(
            archive.next_entry().unwrap().is_none(),
            "{fixture}: SQ holds one file"
        );
    }
}
