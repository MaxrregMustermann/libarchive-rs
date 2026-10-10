//! Ceilings that keep a hostile archive from taking the device with it.
//!
//! Every number here has a default that a normal archive comes nowhere near, and a caller can
//! raise or lower any of them. The point is not to stop a real archive from being read; it is that
//! a 42 KiB file that expands to 4 GiB should fail with a clear error rather than fill the heap of
//! a phone.
//!
//! These are checked *before* memory is committed, not after. A codec that says an entry is
//! 900 PiB long is rejected on the strength of the header, so no allocation is attempted at all.

/// The most memory an LZMA decoder may allocate for its dictionary.
///
/// A decoder is told its dictionary size in the entry's own header, so a 20-byte file can ask for
/// a 4 GiB dictionary. The cap is a fourth of the per-entry ceiling, which is more than any real
/// archive uses — the 7-Zip default dictionary is 16 MiB — while still being refused cheaply.
pub const DEFAULT_LZMA_MEMORY_LIMIT: u64 = 1024 * 1024 * 1024;

/// The ceilings applied while reading and writing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most bytes one entry may decompress to.
    ///
    /// Default 4 GiB, which is more than any single file on a phone and far more than any entry
    /// anybody archives by hand.
    pub max_entry_size: u64,
    /// The most bytes a whole archive may decompress to across every entry read from it.
    ///
    /// This is the one that catches the classic zip bomb, where many entries are each just under
    /// the per-entry ceiling.
    pub max_total_size: u64,
    /// The most entries an archive may list.
    ///
    /// Bounds the memory the entry list itself can take, which is what a "zip quine" or a header
    /// flood attacks.
    pub max_entries: usize,
    /// The most bytes of headers and metadata that listing may allocate.
    ///
    /// Bounds formats whose directory has to be read before it can be parsed, which is most of them.
    pub max_metadata_size: u64,
    /// The compression ratio a single entry may reach before it is treated as a bomb.
    ///
    /// A run of zeroes compresses by a factor of about a thousand and is normal. A factor of a
    /// hundred thousand is not.
    pub max_compression_ratio: u64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_entry_size: 4 * 1024 * 1024 * 1024,
            max_total_size: 16 * 1024 * 1024 * 1024,
            max_entries: 1_000_000,
            max_metadata_size: 256 * 1024 * 1024,
            max_compression_ratio: 100_000,
        }
    }
}

/// Tracks how much an archive has produced across every entry, so a bomb split across many entries
/// is still caught.
///
/// Kept at module scope rather than inside [`Limits`] because Rust has no nested items in an impl
/// block, and because it is genuinely a separate thing: a reader owns one and charges every entry
/// against it.
#[derive(Clone, Debug, Default)]
pub struct Total {
    limit: u64,
    spent: u64,
}

impl Total {
    pub fn new(limit: u64) -> Total {
        Total { limit, spent: 0 }
    }

    /// Charges `bytes` against the budget, or reports that the budget is gone.
    pub fn charge(&mut self, bytes: u64) -> crate::Result<()> {
        // Saturating: a total that wrapped would hand back a small number and let the bomb
        // through, which is exactly the bug this is guarding against.
        self.spent = self.spent.saturating_add(bytes);
        if self.spent > self.limit {
            return Err(crate::Error::limit(format!(
                "The archive has produced more than the {} byte limit",
                self.limit
            )));
        }
        Ok(())
    }

    pub fn spent(&self) -> u64 {
        self.spent
    }
}

impl Limits {
    /// Limits with every ceiling off, for a caller that has decided the input is trustworthy.
    ///
    /// Not the default, and not reachable from the JNI bridge without a deliberate decision there.
    pub fn unlimited() -> Limits {
        Limits {
            max_entry_size: u64::MAX,
            max_total_size: u64::MAX,
            max_entries: usize::MAX,
            max_metadata_size: u64::MAX,
            max_compression_ratio: u64::MAX,
        }
    }

    /// Rejects an entry whose header claims a size past the ceiling.
    ///
    /// Called before the entry is opened, so a false size costs nothing.
    pub fn check_entry_size(&self, size: Option<u64>) -> crate::Result<()> {
        match size {
            Some(size) if size > self.max_entry_size => Err(crate::Error::limit(format!(
                "The entry claims {size} bytes, over the {} byte limit",
                self.max_entry_size
            ))),
            _ => Ok(()),
        }
    }

    /// Rejects an archive whose entry count is past the ceiling.
    pub fn check_entry_count(&self, count: usize) -> crate::Result<()> {
        if count > self.max_entries {
            return Err(crate::Error::limit(format!(
                "The archive lists {count} entries, over the {} entry limit",
                self.max_entries
            )));
        }
        Ok(())
    }

    /// Rejects a metadata read that is past the ceiling.
    ///
    /// Separate from [`Limits::check_entry_size`] because the two have different justifications: a
    /// metadata read is a single allocation the archive dictates the size of, whereas an entry's
    /// size is a claim that may be wrong. Both are checked before anything is committed.
    pub fn check_metadata_size(&self, size: u64) -> crate::Result<()> {
        if size > self.max_metadata_size {
            return Err(crate::Error::limit(format!(
                "The archive's metadata claims {size} bytes, over the {} byte limit",
                self.max_metadata_size
            )));
        }
        Ok(())
    }

    /// Rejects a compression ratio that no real data produces.
    ///
    /// Only applied once an entry has produced at least [`Limits::RATIO_FLOOR`] bytes, because the
    /// ratio of the first few bytes of an entry is meaningless: a 100-byte run of zeroes read out
    /// of a 20-byte deflate stream is a ratio of five, and a short entry that happens to end on a
    /// run boundary can look like a bomb.
    pub fn check_ratio(&self, compressed: u64, uncompressed: u64) -> crate::Result<()> {
        /// How many bytes must come out before the ratio means anything.
        const RATIO_FLOOR: u64 = 64 * 1024;
        if uncompressed < RATIO_FLOOR {
            return Ok(());
        }
        // `compressed == 0` with output is a codec bug rather than a bomb, so do not divide.
        if compressed == 0 {
            return Ok(());
        }
        let ratio = uncompressed / compressed;
        if ratio > self.max_compression_ratio {
            return Err(crate::Error::limit(format!(
                "The entry expands {ratio} times, over the {} limit",
                self.max_compression_ratio
            )));
        }
        Ok(())
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[test]
    fn a_normal_archive_is_nowhere_near_any_ceiling() {
        let limits = Limits::default();
        // A 4 GiB video, in an archive with a few hundred entries.
        assert!(limits.check_entry_size(Some(4 * 1024 * 1024 * 1024 - 1)).is_ok());
        assert!(limits.check_entry_count(500).is_ok());
        // Text compresses well. A factor of twenty is unremarkable.
        assert!(limits.check_ratio(5 * 1024 * 1024, 100 * 1024 * 1024).is_ok());
    }

    #[test]
    fn an_entry_that_claims_to_be_larger_than_the_ceiling_is_refused_before_it_is_opened() {
        let limits = Limits::default();
        let error = limits.check_entry_size(Some(u64::MAX)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::LimitExceeded);
        assert!(error.message().contains("over the"), "the error should say why: {error}");
    }

    #[test]
    fn an_unknown_size_is_allowed_because_the_decoder_finds_out_later() {
        // Many formats do not record a size. Refusing those would refuse most of the long tail.
        assert!(Limits::default().check_entry_size(None).is_ok());
    }

    #[test]
    fn a_bomb_is_caught_by_the_ratio_but_only_once_there_is_enough_output_to_judge() {
        let limits = Limits::default();
        // Under the floor, the ratio is not checked at all: this is ordinary short output.
        assert!(limits.check_ratio(1, 1000).is_ok());
        // Over the floor, a factor of a hundred thousand is refused.
        let error = limits.check_ratio(1, 100_000 * 1024).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::LimitExceeded);
    }

    #[test]
    fn a_ratio_check_does_not_divide_by_zero() {
        // A codec reporting output with no input is a bug, not a bomb, and must not panic.
        assert!(Limits::default().check_ratio(0, 10 * 1024 * 1024).is_ok());
    }

    #[test]
    fn a_bomb_split_across_many_entries_is_caught_by_the_running_total() {
        let mut total = Total::new(1024);
        assert!(total.charge(512).is_ok());
        assert_eq!(total.spent(), 512);
        let error = total.charge(1024).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::LimitExceeded);
    }

    #[test]
    fn a_total_that_would_overflow_saturates_rather_than_wrapping_back_to_something_small() {
        // A budget one byte short of the maximum, so the last charge overflows a naive `+=`.
        let mut total = Total::new(u64::MAX - 1);
        assert!(total.charge(u64::MAX - 1).is_ok());
        assert_eq!(total.spent(), u64::MAX - 1);
        // The whole point: a wrapping addition here would reset the counter to a small number and
        // let the bomb carry on, which is the failure this is written to prevent.
        let error = total.charge(1).expect_err("the budget is gone");
        assert_eq!(error.kind(), ErrorKind::LimitExceeded);
        assert_eq!(total.spent(), u64::MAX, "the counter saturates rather than wrapping");
    }

    #[test]
    fn an_archive_listing_a_million_entries_is_refused() {
        let limits = Limits::default();
        assert!(limits.check_entry_count(1_000_000).is_ok());
        assert_eq!(
            limits.check_entry_count(1_000_001).unwrap_err().kind(),
            ErrorKind::LimitExceeded
        );
    }
}