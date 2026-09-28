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
