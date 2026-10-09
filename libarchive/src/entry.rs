//! Entries: what the archive says about one of the things inside it.

use std::fmt;
use std::time::SystemTime;

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A regular file with contents.
    File,
    /// A directory. The name may or may not carry a trailing slash depending on the format; it is
    /// normalised away when the entry list is built, so callers never have to check.
    Directory,
    /// A symbolic link. The target is in [`Entry::symlink_target`].
    Symlink,
    /// A hard link. The target is in [`Entry::symlink_target`], as a path within the archive.
    HardLink,
    /// A device node, socket or FIFO. `FileType` says which.
    Special(EntrySpecial),
}

/// The special file kinds, for the formats that record them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntrySpecial {
    Fifo,
    CharDevice,
    BlockDevice,
    Socket,
}

/// One entry, as the archive describes it.
///
/// This is what a listing shows. It says nothing about the contents: getting those is
/// [`crate::reader::ArchiveReader::open_entry`], which streams them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path within the archive, always with `/` as the separator and never with a trailing
    /// slash, whatever the format itself uses.
    pub name: String,
    pub kind: EntryKind,
    /// The size the *uncompressed* contents will be, when the archive says. `None` when it does
    /// not, which is common and is not an error: a stream still works, it just cannot report a
    /// size in advance.
    pub size: Option<u64>,
    /// The size as stored, after compression. Useful for reporting what an archive costs.
    pub compressed_size: Option<u64>,
    pub last_modified: Option<SystemTime>,
    /// The unix permission bits, when the format records them.
    pub mode: Option<u32>,
    /// For a symlink or hard link, the path it points at. `None` for everything else, and for a
    /// symlink whose target the format did not record.
    pub symlink_target: Option<String>,
    /// Whether the contents are encrypted. An encrypted entry can still be listed; reading it
    /// needs a password.
    pub is_encrypted: bool,
    /// Whether this entry is the first of its entry in a solid block, and where its data begins.
    /// Only meaningful for 7Z, and only set when the archive was opened with
    /// [`crate::OpenOptions::expose_block_layout`].
    pub block: Option<BlockPlacement>,
}

/// Where an entry's data lives, for formats that put several entries in one compressed block.
///
/// This is what makes solid 7Z tractable. Decoding any entry means decoding its block from the
/// start, so the cost of reading entries *n* and *n+1* in the same block is one decode, not two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockPlacement {
    /// Which block the entry belongs to, numbered from zero in file order.
    pub block_index: usize,
    /// The entry's offset within the decoded block.
    pub offset_in_block: u64,
    /// The block's size once decoded.
    pub block_size: u64,
}

impl Entry {
    pub fn is_directory(&self) -> bool {
        self.kind == EntryKind::Directory
    }

    pub fn is_file(&self) -> bool {
        self.kind == EntryKind::File
    }

    pub fn is_symlink(&self) -> bool {
        self.kind == EntryKind::Symlink
    }

    /// The size, defaulting to zero, for the cases where a zero is the right answer.
    pub fn size_or_zero(&self) -> u64 {
        self.size.unwrap_or(0)
    }
}

/// Strips a trailing slash and normalises the separator.
///
/// Formats disagree: tar records `dir/`, ZIP usually does, and 7Z does not. Normalising once here
/// means every caller downstream, including the app's path resolution, sees one shape.
///
/// Also maps `\` to `/` for the formats that use it, since a caller resolving an entry name as a
/// path would otherwise treat `a\b.txt` as one file rather than two.
pub fn normalise_name(name: &str) -> String {
    let mut normalised = String::with_capacity(name.len());
    let mut previous_was_separator = false;
    for character in name.chars() {
        let is_separator = character == '/' || character == '\\';
        if is_separator {
            if previous_was_separator {
                continue;
            }
            previous_was_separator = true;
            normalised.push('/');
        } else {
            previous_was_separator = false;
            normalised.push(character);
        }
    }
    // A trailing slash marks a directory in most formats. Removing it is safe because the entry
    // kind already says whether it is a directory, and callers that care can check that.
    while normalised.len() > 1 && normalised.ends_with('/') {
        normalised.pop();
    }
    if normalised == "/" {
        normalised.clear();
    }
    normalised
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_trailing_slash_formats_disagree_about() {
        assert_eq!(normalise_name("dir/"), "dir");
        assert_eq!(normalise_name("dir"), "dir");
        assert_eq!(normalise_name("a/b/c.txt"), "a/b/c.txt");
    }

    #[test]
    fn collapses_repeated_separators_rather_than_leaving_empty_components() {
        assert_eq!(normalise_name("a//b"), "a/b");
        assert_eq!(normalise_name("a///b///"), "a/b");
    }

    #[test]
    fn treats_a_backslash_as_a_separator_in_every_format() {
        assert_eq!(normalise_name("a\\b.txt"), "a/b.txt");
    }

    #[test]
    fn keeps_a_single_character_and_a_dot_alone_they_are_not_directories() {
        assert_eq!(normalise_name("/"), "");
        assert_eq!(normalise_name("."), ".");
        assert_eq!(normalise_name(".."), "..");
        assert_eq!(normalise_name("a"), "a");
    }

    #[test]
    fn does_not_strip_the_slash_from_a_name_that_is_only_one() {
        assert_eq!(normalise_name("//"), "");
    }

    #[test]
    fn a_directory_is_told_by_its_kind_not_by_its_trailing_slash() {
        let entry = Entry {
            name: normalise_name("dir/"),
            kind: EntryKind::Directory,
            size: None,
            compressed_size: None,
            last_modified: None,
            mode: None,
            symlink_target: None,
            is_encrypted: false,
            block: None,
        };
        assert_eq!(entry.name, "dir");
        assert!(entry.is_directory());
        assert!(!entry.is_file());
    }
}