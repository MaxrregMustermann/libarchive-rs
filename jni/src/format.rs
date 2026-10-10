//! The format numbering the Kotlin side's `ArchiveFormat` uses.
//!
//! The ids are part of the contract with the app and are asserted on both sides: `ArchiveFormat.kt`
//! in the app and this module. An id this module does not recognise is a thrown `IOException` rather
//! than a silent fallback, because writing an archive as the wrong format produces a file that
//! cannot be read back.

use libarchive::format::{Compression, Format};

/// The format an id stands for, or `None` when the id is not one the app uses.
pub fn from_id(id: i32) -> Option<Format> {
    Some(match id {
        0 => Format::SevenZ,
        1 => Format::Zip,
        2 => Format::Tar,
        3 => Format::CompressedTar(Compression::Xz),
        4 => Format::SingleStream(Compression::Xz),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_format_id_the_app_uses_maps_to_a_format() {
        // ArchiveFormat.kt: SEVEN_Z=0, ZIP=1, TAR=2, TAR_XZ=3, XZ=4.
        assert_eq!(from_id(0), Some(Format::SevenZ));
        assert_eq!(from_id(1), Some(Format::Zip));
        assert_eq!(from_id(2), Some(Format::Tar));
        assert_eq!(from_id(3), Some(Format::CompressedTar(Compression::Xz)));
        assert_eq!(from_id(4), Some(Format::SingleStream(Compression::Xz)));
    }

    #[test]
    fn an_id_the_app_does_not_use_is_refused_rather_than_guessed() {
        // A wrong format id must not silently produce an archive of the wrong shape.
        assert_eq!(from_id(-1), None);
        assert_eq!(from_id(5), None);
        assert_eq!(from_id(i32::MAX), None);
    }
}
