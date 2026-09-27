//! An external xattr block two inodes share is edited as shared.
//!
//! The kernel stores one xattr block for every inode with an identical
//! attribute set (mbcache), and counts them in `h_refcount`. Files with the
//! same ACL or security label end up sharing one. This driver read the block
//! as belonging to the inode in hand:
//!
//! - `apply_setxattr` rewrote it in place and stamped `h_refcount = 1`, so
//!   the other inode's attributes changed with it;
//! - `apply_removexattr` freed it once empty, while the other inode still
//!   pointed at it;
//! - `apply_unlink` never released it at all, whether shared or not.
//!
//! mke2fs never shares a block, so the shared state is built as the kernel
//! leaves it. `/a` is given an attribute too large for the inode. `/b` is
//! pointed at the same block, with its `i_blocks` raised by the block and
//! the block's count set to 2, all checksums restamped. `e2fsck -fn` must
//! accept that before anything is done to it, and after each operation on
//! `/a`, and `/b` must still read its attribute. Fails without e2fsprogs
//! (they run in the harness VM).

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use std::sync::Arc;

const NAME: &str = "user.shared";

fn mkfs(tag: &str) -> String {
    let path =
        fs_ext4_test_support::temp_path!("fs_ext4_shared_xattr_{tag}_{}.img", std::process::id());
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();
    let out = fs_ext4_test_support::oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256"])
        .arg(&path)
        .output();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

fn e2fsck_clean(path: &str, what: &str) {
    fs_ext4_test_support::assert_e2fsck_clean(path, what);
}

fn mount(path: &str) -> Filesystem {
    Filesystem::mount(Arc::new(FileDevice::open_rw(path).unwrap())).unwrap()
}

fn ino_of(fs: &Filesystem, path: &str) -> u32 {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    fs_ext4::path::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path).unwrap()
}

fn attribute(fs: &Filesystem, path: &str) -> Option<Vec<u8>> {
    let (inode, raw) = fs.read_inode_verified(ino_of(fs, path)).unwrap();
    fs_ext4::xattr::get(
        fs.dev.as_ref(),
        &inode,
        &raw,
        fs.sb.inode_size,
        fs.sb.block_size(),
        NAME,
    )
    .unwrap()
}

/// `/a` and `/b` sharing one external xattr block that holds `NAME`.
/// `share` false leaves `/b` without it, and the block `/a`'s alone.
fn volume(tag: &str, share: bool) -> String {
    let path = mkfs(tag);
    let value = vec![b'v'; 600];
    let fs = mount(&path);
    fs.apply_create("/a", 0o644).unwrap();
    fs.apply_create("/b", 0o644).unwrap();
    fs.apply_setxattr("/a", NAME, &value).unwrap();
    let (a, _) = fs.read_inode_verified(ino_of(&fs, "/a")).unwrap();
    assert_ne!(a.file_acl, 0, "[{tag}] the attribute stayed in the inode");
    if share {
        let bs = fs.sb.block_size() as u64;
        let at = a.file_acl;
        let mut block = vec![0u8; bs as usize];
        fs.dev.read_at(at * bs, &mut block).unwrap();
        block[4..8].copy_from_slice(&2u32.to_le_bytes());
        fs.csum.patch_xattr_block(at, &mut block);
        fs.dev.write_at(at * bs, &block).unwrap();

        let b_ino = ino_of(&fs, "/b");
        let (b, mut raw) = fs.read_inode_verified(b_ino).unwrap();
        raw[0x68..0x6C].copy_from_slice(&(at as u32).to_le_bytes());
        raw[0x76..0x78].copy_from_slice(&((at >> 32) as u16).to_le_bytes());
        let blocks = u32::from_le_bytes(raw[0x1C..0x20].try_into().unwrap());
        raw[0x1C..0x20].copy_from_slice(&(blocks + (bs / 512) as u32).to_le_bytes());
        if let Some((lo, hi)) = fs.csum.compute_inode_checksum(b_ino, b.generation, &raw) {
            raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
            raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
        }
        fs.write_inode_raw(b_ino, &raw).unwrap();
        fs.dev.flush().unwrap();
    }
    drop(fs);
    e2fsck_clean(&path, &format!("{tag}: as built"));
    if share {
        assert_eq!(attribute(&mount(&path), "/b").as_deref(), Some(&value[..]));
    }
    path
}

fn b_keeps_its_attribute(path: &str, what: &str) {
    e2fsck_clean(path, what);
    assert_eq!(
        attribute(&mount(path), "/b"),
        Some(vec![b'v'; 600]),
        "[{what}] /b's attribute changed with /a's"
    );
}

#[test]
fn setting_an_attribute_on_one_inode_leaves_the_other_alone() {
    let path = volume("set", true);
    mount(&path)
        .apply_setxattr("/a", NAME, &[b'w'; 700])
        .expect("setxattr");
    b_keeps_its_attribute(&path, "set");
    assert_eq!(attribute(&mount(&path), "/a"), Some(vec![b'w'; 700]));
}

#[test]
fn removing_an_attribute_from_one_inode_leaves_the_other_alone() {
    let path = volume("remove", true);
    mount(&path)
        .apply_removexattr("/a", NAME)
        .expect("removexattr");
    b_keeps_its_attribute(&path, "remove");
    assert_eq!(attribute(&mount(&path), "/a"), None);
}

#[test]
fn unlinking_one_inode_leaves_the_other_its_attribute() {
    let path = volume("unlink_shared", true);
    mount(&path).apply_unlink("/a").expect("unlink");
    b_keeps_its_attribute(&path, "unlink_shared");
}

#[test]
fn unlinking_the_only_inode_frees_its_block() {
    let path = volume("unlink_alone", false);
    mount(&path).apply_unlink("/a").expect("unlink");
    e2fsck_clean(&path, "unlink_alone");
}
