use super::*;
use crate::features::FsFlavor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Event {
    BeforeCommit,
    Contended,
}

pub(super) type Hook = Arc<dyn Fn(Event) + Send + Sync>;

impl Filesystem {
    pub(super) fn write_test_event(&self, event: Event) {
        let hook = self.write_test_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(event);
        }
    }
}

pub(crate) struct MemDev(Mutex<Vec<u8>>);

impl BlockDevice for MemDev {
    fn read_at(&self, offset: u64, bytes: &mut [u8]) -> Result<()> {
        let data = self.0.lock().unwrap();
        bytes.copy_from_slice(&data[offset as usize..offset as usize + bytes.len()]);
        Ok(())
    }

    fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.0.lock().unwrap()[offset as usize..offset as usize + bytes.len()]
            .copy_from_slice(bytes);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.0.lock().unwrap().len() as u64
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }

    fn is_writable(&self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug)]
enum Case {
    Creates,
    ExtAttr,
    SharedInode,
    LargeFile,
}

const WAIT: Duration = Duration::from_secs(20);
const CONTENT: &[u8] = b"readers remain available";

pub(crate) fn formatted(flavor: FsFlavor, large_file: bool) -> Arc<MemDev> {
    let size = 32 * 1024 * 1024;
    let dev = Arc::new(MemDev(Mutex::new(vec![0; size])));
    crate::mkfs::format_filesystem_with_flavor(
        dev.as_ref(),
        None,
        Some([0x60; 16]),
        size as u64,
        4096,
        flavor,
    )
    .unwrap();
    // mkfs sets large_file; a volume without it is made by clearing it.
    if !large_file {
        let mut sb = Superblock::read(dev.as_ref()).unwrap().raw;
        let flags = u32::from_le_bytes(sb[0x64..0x68].try_into().unwrap());
        sb[0x64..0x68]
            .copy_from_slice(&(flags & !features::RoCompat::LARGE_FILE.bits()).to_le_bytes());
        if flavor == FsFlavor::Ext4 {
            let checksum = crate::checksum::linux_crc32c(!0, &sb[..0x3fc]);
            sb[0x3fc..0x400].copy_from_slice(&checksum.to_le_bytes());
        }
        dev.write_at(crate::superblock::SUPERBLOCK_OFFSET, &sb)
            .unwrap();
    }
    dev
}

pub(crate) fn interleave<A, B, First, Second>(
    fs: &Filesystem,
    first: A,
    second: B,
) -> (First, Second)
where
    A: FnOnce() -> First + Send,
    B: FnOnce() -> Second + Send,
    First: Send,
    Second: Send,
{
    let (parked_tx, parked_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let (second_tx, second_rx) = mpsc::channel();
    let resume_rx = Mutex::new(resume_rx);
    let parked = AtomicBool::new(false);
    let blocked_tx = second_tx.clone();
    *fs.write_test_hook.lock().unwrap() = Some(Arc::new(move |event| {
        let current = thread::current();
        if current.name() == Some("writer-a")
            && event == Event::BeforeCommit
            && !parked.swap(true, Ordering::SeqCst)
        {
            parked_tx.send(()).unwrap();
            resume_rx.lock().unwrap().recv_timeout(WAIT).unwrap();
        } else if current.name() == Some("writer-b") && event == Event::Contended {
            blocked_tx.send(()).unwrap();
        }
    }));

    let results = thread::scope(|scope| {
        let first_thread = thread::Builder::new()
            .name("writer-a".into())
            .spawn_scoped(scope, first)
            .unwrap();
        parked_rx.recv_timeout(WAIT).unwrap();

        let (read_tx, read_rx) = mpsc::channel();
        let reader = scope.spawn(move || {
            let ino = fs.lookup_path_bytes(b"/stable").unwrap();
            assert_eq!(fs.stat_ino(ino).unwrap().size, CONTENT.len() as u64);
            let mut bytes = vec![0; CONTENT.len()];
            assert_eq!(fs.read_ino(ino, 0, &mut bytes).unwrap(), CONTENT.len());
            assert_eq!(bytes, CONTENT);
            assert_eq!(fs.lookup_at(2, b"stable").unwrap(), ino);
            read_tx.send(()).unwrap();
        });
        let read_result = read_rx.recv_timeout(WAIT);
        if read_result.is_err() {
            let _ = resume_tx.send(());
        }
        read_result.expect("readers must finish while writer A remains staged");
        reader.join().unwrap();

        let second_thread = thread::Builder::new()
            .name("writer-b".into())
            .spawn_scoped(scope, move || {
                let result = second();
                second_tx.send(()).unwrap();
                result
            })
            .unwrap();
        let second_result = second_rx.recv_timeout(WAIT);
        resume_tx.send(()).unwrap();
        second_result.expect("writer B must either finish or reach the held write lock");
        (first_thread.join().unwrap(), second_thread.join().unwrap())
    });
    *fs.write_test_hook.lock().unwrap() = None;
    results
}

fn exercise(dev: Arc<MemDev>, journaled: bool, case: Case) -> Arc<MemDev> {
    let fs = Filesystem::mount(dev.clone()).unwrap();
    assert_eq!(fs.journal.is_some(), journaled);
    fs.apply_create("/warm", 0o644).unwrap();
    fs.apply_create("/stable", 0o644).unwrap();
    fs.apply_replace_file_content("/stable", CONTENT).unwrap();
    let target = match case {
        Case::Creates => 0,
        Case::ExtAttr => fs.apply_create("/x", 0o644).unwrap(),
        Case::SharedInode | Case::LargeFile => fs.apply_create("/big", 0o644).unwrap(),
    };
    if matches!(case, Case::ExtAttr | Case::LargeFile) {
        for index in 0..32 {
            fs.apply_create(&format!("/padding-{index}"), 0o644)
                .unwrap();
        }
    }
    let before = Superblock::read(dev.as_ref()).unwrap();
    if matches!(case, Case::ExtAttr) {
        assert_eq!(before.feature_compat & features::Compat::EXT_ATTR.bits(), 0);
    }
    let (first, second) = interleave(
        &fs,
        || fs.apply_create("/a", 0o644),
        || match case {
            Case::Creates => fs.apply_create_at(2, b"b", 0o644),
            Case::ExtAttr => fs
                .apply_setxattr("/x", "user.concurrent", b"kept")
                .map(|()| target),
            Case::SharedInode => fs.apply_truncate_ino(target, 65536).map(|()| target),
            Case::LargeFile => fs
                .apply_truncate_ino(target, 3 * 1024 * 1024 * 1024)
                .map(|()| target),
        },
    );
    let first = first.unwrap();
    let second = second.unwrap();
    if matches!(case, Case::Creates) {
        assert_ne!(first, second, "concurrent creates must not share an inode");
    }
    drop(fs);

    let reopened = Filesystem::mount(dev.clone()).unwrap();
    assert_eq!(reopened.lookup_path_bytes(b"/a").unwrap(), first);
    let expected_allocations = if matches!(case, Case::Creates) { 2 } else { 1 };
    assert_eq!(
        reopened.sb.free_inodes_count,
        before.free_inodes_count - expected_allocations,
        "every concurrent allocation must be counted"
    );
    let (group, bitmap_bit) = (
        (first - 1) / reopened.sb.inodes_per_group,
        (first - 1) % reopened.sb.inodes_per_group,
    );
    let bitmap = reopened
        .read_block(reopened.groups[group as usize].inode_bitmap)
        .unwrap();
    assert_ne!(bitmap[bitmap_bit as usize / 8] & (1 << (bitmap_bit % 8)), 0);
    match case {
        Case::Creates => {
            assert_eq!(reopened.lookup_path_bytes(b"/b").unwrap(), second);
            let bitmap_bit = (second - 1) % reopened.sb.inodes_per_group;
            assert_ne!(bitmap[bitmap_bit as usize / 8] & (1 << (bitmap_bit % 8)), 0);
        }
        Case::ExtAttr => {
            assert_ne!(
                reopened.sb.feature_compat & features::Compat::EXT_ATTR.bits(),
                0
            );
            let (inode, raw) = reopened.read_inode_verified(target).unwrap();
            if reopened.sb.inode_size > 128 {
                assert_eq!(inode.file_acl, 0, "exercise the in-inode attribute path");
            }
            let entries = crate::xattr::read_all(
                reopened.dev.as_ref(),
                &inode,
                &raw,
                reopened.sb.inode_size,
                reopened.sb.block_size(),
            )
            .unwrap();
            assert!(entries
                .iter()
                .any(|entry| entry.name == "user.concurrent" && entry.value == b"kept"));
        }
        Case::SharedInode | Case::LargeFile => {
            let first_block = bgd::locate_inode(&reopened.sb, &reopened.groups, first)
                .unwrap()
                .0;
            let target_block = bgd::locate_inode(&reopened.sb, &reopened.groups, target)
                .unwrap()
                .0;
            assert_eq!(
                first_block == target_block,
                matches!(case, Case::SharedInode)
            );
            let expected_size = if matches!(case, Case::LargeFile) {
                3 * 1024 * 1024 * 1024
            } else {
                65536
            };
            assert_eq!(reopened.stat_ino(target).unwrap().size, expected_size);
            if matches!(case, Case::LargeFile) {
                assert_ne!(
                    reopened.sb.feature_ro_compat & features::RoCompat::LARGE_FILE.bits(),
                    0
                );
            }
        }
    }
    drop(reopened);
    dev
}

macro_rules! cases {
    ($($name:ident: $flavor:ident, $case:ident;)*) => {$(
        #[test]
        fn $name() {
            let dev = formatted(FsFlavor::$flavor, matches!(Case::$case, Case::LargeFile));
            exercise(dev, FsFlavor::$flavor == FsFlavor::Ext3, Case::$case);
        }
    )*};
}

cases! {
    distinct_creates_without_journal: Ext4, Creates;
    distinct_creates_with_journal: Ext3, Creates;
    ext_attr_without_journal: Ext4, ExtAttr;
    ext_attr_with_journal: Ext3, ExtAttr;
    shared_inode_without_journal: Ext4, SharedInode;
    shared_inode_with_journal: Ext3, SharedInode;
    large_file_without_journal: Ext4, LargeFile;
    large_file_with_journal: Ext3, LargeFile;
}

fn activate_large_file(flavor: FsFlavor) -> Arc<MemDev> {
    let dev = formatted(flavor, false);
    assert_eq!(
        Superblock::read(dev.as_ref()).unwrap().feature_ro_compat
            & features::RoCompat::LARGE_FILE.bits(),
        0
    );
    exercise(dev, flavor == FsFlavor::Ext3, Case::LargeFile)
}

#[test]
fn concurrent_large_file_activation_without_journal() {
    activate_large_file(FsFlavor::Ext4);
}

#[test]
fn concurrent_large_file_activation_with_journal() {
    activate_large_file(FsFlavor::Ext3);
}

mod needs_host {
    use super::*;

    #[test]
    fn concurrent_large_file_activation_is_clean_to_e2fsck() {
        for flavor in [FsFlavor::Ext4, FsFlavor::Ext3] {
            let dev = activate_large_file(flavor);
            let tag = format!("concurrent-large-file-activation-{flavor:?}");
            let path = fs_ext4_test_support::temp_path!("{tag}.img");
            std::fs::write(&path, &*dev.0.lock().unwrap()).unwrap();
            fs_ext4_test_support::assert_e2fsck_clean(&path, &tag);
        }
    }

    #[test]
    fn concurrent_writes_are_clean_to_e2fsck() {
        for flavor in [FsFlavor::Ext4, FsFlavor::Ext3] {
            for case in [
                Case::Creates,
                Case::ExtAttr,
                Case::SharedInode,
                Case::LargeFile,
            ] {
                let dev = formatted(flavor, matches!(case, Case::LargeFile));
                let dev = exercise(dev, flavor == FsFlavor::Ext3, case);
                let tag = format!("concurrent-{flavor:?}-{case:?}");
                let path = fs_ext4_test_support::temp_path!("{tag}.img");
                std::fs::write(&path, &*dev.0.lock().unwrap()).unwrap();
                fs_ext4_test_support::assert_e2fsck_clean(&path, &tag);
            }
        }
    }
}
