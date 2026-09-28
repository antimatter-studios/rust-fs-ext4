//! Writing inline data (#428).
//!
//! An inline-data inode keeps its bytes in the inode: the first 60 in
//! `i_block` and the rest as the value of the in-inode extended attribute
//! `system.data` (see [`crate::inline_data`]). The kernel keeps two things
//! true of every such inode, and so does this module:
//!
//! - `system.data` EXISTS, even empty. `e2fsck` treats an inline inode
//!   without it as damaged.
//! - For a regular file its value is exactly the bytes past the first 60,
//!   and the bytes of `i_block` past `i_size` are zero.
//!
//! The functions here edit an inode image and nothing else; the caller
//! stages that image in the same transaction as whatever else the write
//! changes.

use crate::error::{Error, Result};
use crate::inode::InodeFlags;

/// The attribute holding the bytes past the first 60.
pub(crate) const DATA_ATTR: &str = "system.data";

/// `i_block`, where the first 60 bytes live.
const I_BLOCK: std::ops::Range<usize> = 0x28..0x64;

/// Bytes of inline data `i_block` holds.
pub(crate) const I_BLOCK_LEN: usize = 60;

/// No inode of `inode_size` bytes can hold more inline data than this: the
/// attribute area is inside the inode. A cheap bound checked before any
/// attribute area is encoded.
pub(crate) fn ceiling(inode_size: u16) -> usize {
    I_BLOCK_LEN + usize::from(inode_size)
}

/// The in-inode attribute area of the image `raw`, from its magic to the
/// end of the inode; `None` when the inode has none (`i_extra_isize` 0:
/// the kernel parses no area there, #380).
fn attr_area(raw: &[u8], inode_size: u16) -> Option<std::ops::Range<usize>> {
    if raw.len() < 0x82 {
        return None;
    }
    let extra = u16::from_le_bytes([raw[0x80], raw[0x81]]) as usize;
    let start = 128 + extra;
    let end = usize::from(inode_size).min(raw.len());
    (extra != 0 && start + 8 <= end).then_some(start..end)
}

/// Make the inode image `raw` hold `data` as inline data: the first 60
/// bytes in `i_block`, zero-padded, and the rest as `system.data`.
///
/// `Error::NoSpaceLeftOnDevice` when it does not fit beside the inode's
/// other attributes, and `raw` is then unchanged: the caller converts the
/// inode out of inline data instead. `i_size`, the times and the checksum
/// are the caller's.
pub(crate) fn store(raw: &mut [u8], inode_size: u16, data: &[u8]) -> Result<()> {
    if data.len() > ceiling(inode_size) {
        return Err(Error::NoSpaceLeftOnDevice);
    }
    let area = attr_area(raw, inode_size).ok_or(Error::NoSpaceLeftOnDevice)?;
    let head = data.len().min(I_BLOCK_LEN);
    let mut region = raw[area.clone()].to_vec();
    crate::xattr::plan_set_in_inode_region(&mut region, DATA_ATTR, &data[head..])?;
    raw[area].copy_from_slice(&region);
    raw[I_BLOCK].fill(0);
    raw[I_BLOCK.start..I_BLOCK.start + head].copy_from_slice(&data[..head]);
    Ok(())
}

/// Take the inline data out of the inode image `raw`: `system.data` is
/// removed, `EXT4_INLINE_DATA_FL` cleared and `i_block` zeroed, ready for
/// the caller to write a block map there.
pub(crate) fn strip(raw: &mut [u8], inode_size: u16) -> Result<()> {
    if let Some(area) = attr_area(raw, inode_size) {
        crate::xattr::plan_remove_in_inode_region(&mut raw[area], DATA_ATTR)?;
    }
    let flags = u32::from_le_bytes(raw[0x20..0x24].try_into().unwrap());
    let flags = flags & !InodeFlags::INLINE_DATA.bits();
    raw[0x20..0x24].copy_from_slice(&flags.to_le_bytes());
    raw[I_BLOCK].fill(0);
    Ok(())
}

/// Bytes of `i_block` a directory's entries use: the first 4 are its
/// parent's inode number, the implicit `..`.
const DIR_HEAD: std::ops::Range<usize> = 0x2C..0x64;

/// The smallest continuation the kernel creates: room for one entry with a
/// one-byte name (`ext4_update_inline_dir` refuses less).
const MIN_CONTINUATION: usize = crate::dir::entry_rec_len(1);

/// An inline-data directory's entries, as its inode holds them, for an
/// edit: the 56 bytes of `i_block` after the parent's inode number, and the
/// `system.data` continuation. Each area is a run of directory records
/// whose `rec_len`s cover it exactly, as in a directory block, and an entry
/// is added and removed in them the way it is in a block, so the entries
/// that stay keep their places.
pub(crate) struct InlineDir {
    pub(crate) parent: u32,
    head: [u8; 56],
    continuation: Vec<u8>,
}

impl InlineDir {
    /// The directory in the inode image `raw` (parsed as `inode`).
    pub(crate) fn load(
        dev: &dyn crate::block_io::BlockDevice,
        inode: &crate::inode::Inode,
        raw: &[u8],
        inode_size: u16,
        block_size: u32,
    ) -> Result<Self> {
        let continuation =
            crate::inline_data::dir_continuation(dev, inode, raw, inode_size, block_size)?;
        Ok(Self {
            parent: u32::from_le_bytes(raw[0x28..0x2C].try_into().unwrap()),
            head: raw[DIR_HEAD].try_into().unwrap(),
            continuation,
        })
    }

    /// Every entry, `.` and `..` excluded, in the order a readdir meets them.
    pub(crate) fn entries(&self, has_filetype: bool) -> Result<Vec<crate::dir::DirEntry>> {
        let mut out = Vec::new();
        for area in [&self.head[..], &self.continuation] {
            for entry in crate::dir::DirBlockIter::new(area, has_filetype) {
                out.push(entry?);
            }
        }
        Ok(out)
    }

    /// Add an entry where it fits, `i_block` first. `Ok(false)` when it
    /// fits in neither area.
    pub(crate) fn add(
        &mut self,
        ino: u32,
        name: &[u8],
        file_type: crate::dir::DirEntryType,
        has_filetype: bool,
    ) -> Result<bool> {
        for area in [&mut self.head[..], &mut self.continuation[..]] {
            match crate::dir::add_entry_to_block(area, ino, name, file_type, has_filetype, 0) {
                Ok(()) => return Ok(true),
                Err(Error::OutOfBounds) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }

    /// Remove the entry `name`. `Ok(false)` when there is none.
    pub(crate) fn remove(&mut self, name: &[u8], has_filetype: bool) -> Result<bool> {
        for area in [&mut self.head[..], &mut self.continuation[..]] {
            if crate::dir::remove_entry_from_block(area, name, has_filetype, 0)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Give a directory with no continuation the largest one the inode's
    /// attribute area holds, as one empty record, as the kernel's
    /// `ext4_update_inline_dir` does when `i_block` is full. `false`, and
    /// nothing changed, when it already has one or there is no room for
    /// the smallest entry.
    pub(crate) fn expand(&mut self, raw: &[u8], inode_size: u16) -> bool {
        if !self.continuation.is_empty() {
            return false;
        }
        let Some(area) = attr_area(raw, inode_size) else {
            return false;
        };
        let fits = |len: usize| {
            let mut region = raw[area.clone()].to_vec();
            crate::xattr::plan_set_in_inode_region(&mut region, DATA_ATTR, &vec![0; len]).is_ok()
        };
        // Largest multiple of 4 that fits: `fits` is monotone.
        let (mut lo, mut hi) = (0usize, area.len() / 4 + 1);
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if fits(mid * 4) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let len = lo * 4;
        if len < MIN_CONTINUATION || len > usize::from(u16::MAX) {
            return false;
        }
        let mut continuation = vec![0u8; len];
        continuation[4..6].copy_from_slice(&(len as u16).to_le_bytes());
        self.continuation = continuation;
        true
    }

    /// Write the directory back into the inode image `raw`: the parent and
    /// the head into `i_block`, the continuation as `system.data`, and
    /// `i_size` 60 plus its length. The times and the checksum are the
    /// caller's.
    pub(crate) fn store(&self, raw: &mut [u8], inode_size: u16) -> Result<()> {
        let area = attr_area(raw, inode_size).ok_or(Error::Corrupt(
            "inline directory without an in-inode attribute area",
        ))?;
        let mut region = raw[area.clone()].to_vec();
        crate::xattr::plan_set_in_inode_region(&mut region, DATA_ATTR, &self.continuation)?;
        raw[area].copy_from_slice(&region);
        raw[0x28..0x2C].copy_from_slice(&self.parent.to_le_bytes());
        raw[DIR_HEAD].copy_from_slice(&self.head);
        let size = (I_BLOCK_LEN + self.continuation.len()) as u64;
        raw[0x04..0x08].copy_from_slice(&(size as u32).to_le_bytes());
        raw[0x6C..0x70].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 256-byte inode image with a 32-byte extra area and an attribute
    /// area holding only its magic.
    fn image() -> Vec<u8> {
        let mut raw = vec![0u8; 256];
        raw[0x20..0x24].copy_from_slice(&InodeFlags::INLINE_DATA.bits().to_le_bytes());
        raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        raw[160..164].copy_from_slice(&crate::xattr::EXT4_XATTR_MAGIC.to_le_bytes());
        raw
    }

    /// A device nothing is read from: these inodes have no external block.
    struct NoDev;

    impl crate::block_io::BlockDevice for NoDev {
        fn read_at(&self, _: u64, _: &mut [u8]) -> Result<()> {
            unreachable!("no external attribute block")
        }
        fn write_at(&self, _: u64, _: &[u8]) -> Result<()> {
            unreachable!()
        }
        fn size_bytes(&self) -> u64 {
            0
        }
    }

    fn data_attr(raw: &[u8]) -> Option<Vec<u8>> {
        let inode = crate::inode::Inode::parse(raw).unwrap();
        crate::xattr::read_all(&NoDev, &inode, raw, 256, 4096)
            .unwrap()
            .into_iter()
            .find(|e| e.name == DATA_ATTR)
            .map(|e| e.value)
    }

    #[test]
    fn a_short_file_is_all_in_i_block_with_an_empty_attribute() {
        let mut raw = image();
        raw[0x28..0x64].fill(0xEE);
        store(&mut raw, 256, b"abc").unwrap();
        assert_eq!(&raw[0x28..0x2B], b"abc");
        assert!(raw[0x2B..0x64].iter().all(|&b| b == 0), "i_block tail");
        assert_eq!(data_attr(&raw), Some(Vec::new()));
    }

    #[test]
    fn the_bytes_past_sixty_are_the_attribute() {
        let mut raw = image();
        let data: Vec<u8> = (0..100u8).collect();
        store(&mut raw, 256, &data).unwrap();
        assert_eq!(&raw[0x28..0x64], &data[..60]);
        assert_eq!(data_attr(&raw), Some(data[60..].to_vec()));
    }

    #[test]
    fn what_does_not_fit_is_refused_and_changes_nothing() {
        let mut raw = image();
        store(&mut raw, 256, &[1; 70]).unwrap();
        let before = raw.clone();
        assert!(matches!(
            store(&mut raw, 256, &[2; 200]),
            Err(Error::NoSpaceLeftOnDevice)
        ));
        assert_eq!(raw, before);
    }

    #[test]
    fn an_inode_without_an_attribute_area_holds_nothing_past_i_block() {
        let mut raw = image();
        raw[0x80..0x82].fill(0);
        assert!(matches!(
            store(&mut raw, 256, &[1; 61]),
            Err(Error::NoSpaceLeftOnDevice)
        ));
    }

    /// An empty inline directory whose parent is 2, as the kernel makes
    /// one: one empty record spanning `i_block[4..60]`.
    fn empty_dir() -> (Vec<u8>, InlineDir) {
        let mut raw = image();
        raw[0x04..0x08].copy_from_slice(&60u32.to_le_bytes());
        raw[0x28..0x2C].copy_from_slice(&2u32.to_le_bytes());
        raw[0x30..0x32].copy_from_slice(&56u16.to_le_bytes());
        let inode = crate::inode::Inode::parse(&raw).unwrap();
        let dir = InlineDir::load(&NoDev, &inode, &raw, 256, 4096).unwrap();
        (raw, dir)
    }

    fn names(dir: &InlineDir) -> Vec<Vec<u8>> {
        dir.entries(true)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    #[test]
    fn a_directory_fills_i_block_then_a_continuation_then_refuses() {
        use crate::dir::DirEntryType::RegFile;
        let (mut raw, mut dir) = empty_dir();
        // 56 bytes: two 12-byte and one 32-byte record.
        assert!(dir.add(11, b"a", RegFile, true).unwrap());
        assert!(dir.add(12, b"b", RegFile, true).unwrap());
        assert!(dir.add(13, &[b'c'; 20], RegFile, true).unwrap());
        assert!(
            !dir.add(14, b"d", RegFile, true).unwrap(),
            "i_block is full"
        );
        assert!(dir.expand(&raw, 256), "the attribute area has room");
        assert!(!dir.expand(&raw, 256), "a continuation is made once");
        assert!(dir.add(14, b"d", RegFile, true).unwrap());
        let mut n = 15;
        while dir.add(n, b"eeee", RegFile, true).unwrap() {
            n += 1;
        }
        dir.store(&mut raw, 256).unwrap();
        let size = u32::from_le_bytes(raw[0x04..0x08].try_into().unwrap()) as usize;
        assert_eq!(size, 60 + data_attr(&raw).unwrap().len());
        // What was stored reads back, through the reader the lookups use.
        let inode = crate::inode::Inode::parse(&raw).unwrap();
        let read = crate::inline_data::read_dir(&NoDev, 99, &inode, &raw, 256, 4096, true)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect::<Vec<_>>();
        let mut want = vec![b".".to_vec(), b"..".to_vec()];
        want.extend(names(&dir));
        assert_eq!(read, want);
        assert_eq!(names(&dir).len(), 4 + (n - 15) as usize);
    }

    #[test]
    fn a_removed_entry_leaves_the_others_where_they_were() {
        use crate::dir::DirEntryType::RegFile;
        let (_, mut dir) = empty_dir();
        for (ino, name) in [(11, b"a"), (12, b"b"), (13, b"c")] {
            assert!(dir.add(ino, name, RegFile, true).unwrap());
        }
        let head = dir.head;
        assert!(dir.remove(b"b", true).unwrap());
        assert!(!dir.remove(b"b", true).unwrap());
        assert_eq!(names(&dir), [b"a".to_vec(), b"c".to_vec()]);
        assert_eq!(dir.head[24..36], head[24..36], "c did not move");
    }

    #[test]
    fn strip_removes_the_attribute_and_the_flag() {
        let mut raw = image();
        store(&mut raw, 256, &[7; 90]).unwrap();
        strip(&mut raw, 256).unwrap();
        assert_eq!(data_attr(&raw), None);
        assert!(raw[0x28..0x64].iter().all(|&b| b == 0));
        let flags = u32::from_le_bytes(raw[0x20..0x24].try_into().unwrap());
        assert_eq!(flags & InodeFlags::INLINE_DATA.bits(), 0);
    }
}
