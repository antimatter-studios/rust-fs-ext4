// Shared by both tiers, included textually rather than depended on.
//
// `tests/fuzz_decoders.rs` and `fuzz/src/lib.rs` both `include!` this
// file. A crate dependency would have been tidier, but the fuzz crate
// depends on `libfuzzer-sys`, which builds libFuzzer's C++ runtime, and
// making the gate depend on the fuzz crate would drag that into every
// pull request build on the stable toolchain.
//
// What matters is that the two tiers read an image identically, so a
// reproducer from one reproduces in the other.

// THIS CRATE HAS ITS OWN `BlockDevice`, in `block_io`, rather than
// fs-core's. They are the same shape and a different trait, and a
// device implementing the wrong one fails to compile with a type error
// that names neither -- so it is worth saying which is meant.
use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::{Error as Ext4Error, Result as Ext4Result};

/// An image held in memory, presented as a device.
///
/// The default `write_at` refuses, which is the right answer here and
/// keeps the fuzzing read-only.
pub struct Bytes(pub Vec<u8>);

impl BlockDevice for Bytes {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Ext4Result<()> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let end = start.saturating_add(buf.len());
        if end > self.0.len() {
            // What a real device answers for a read past its end, so a
            // crafted image cannot be told apart from a truncated one
            // by which error it provokes.
            return Err(Ext4Error::Corrupt("read past the end of the device"));
        }
        buf.copy_from_slice(&self.0[start..end]);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

/// How many inodes one walk will read.
///
/// A crafted superblock can claim any inode count, and reading all of
/// them would make a case slow rather than failing it -- which reads as
/// a hang without being one.
pub const INODE_BUDGET: u32 = 48;

/// Open an image and read what a caller would.
///
/// Every result is discarded. A crafted image is *supposed* to be
/// refused; what it may not do is panic, hang, or read somebody else's
/// memory.
///
/// `replay_journal_if_dirty` is in here deliberately. A journal is a
/// structure the format expects to be partially written -- that is its
/// purpose -- so it is parsed with a corruption tolerance the other
/// structures do not have, and it runs at mount, before anything has
/// been established.
pub fn walk(image: &[u8]) {
    let dev: std::sync::Arc<dyn BlockDevice> = std::sync::Arc::new(Bytes(image.to_vec()));
    walk_device(dev);
}

pub fn walk_device(dev: std::sync::Arc<dyn BlockDevice>) {
    let Ok(mut fs) = fs_ext4::Filesystem::mount(dev) else {
        return;
    };

    let _ = fs.replay_journal_if_dirty();
    let _ = fs.orphan_list();

    for ino in 2..2 + INODE_BUDGET {
        let Ok((inode, raw)) = fs.read_inode_verified(ino) else {
            continue;
        };
        // `xattr::read_all_resolved` takes the filesystem and follows
        // an external xattr block as well as the inline area, which is
        // the pair of paths a crafted inode gets to choose between.
        let _ = fs_ext4::xattr::read_all_resolved(&fs, &inode, &raw);
        let _ = fs_ext4::file_io::read_all(&fs, &inode);
        let _ = fs.map_inode_logical(&inode, 0);
    }
}
