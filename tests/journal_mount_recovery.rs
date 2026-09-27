//! Independent Linux/e2fsprogs oracle for the checked plain-JBD2 lifecycle.
//!
//! Fixture bytes are encoded here, independently of the library transaction
//! writer. Every image is disposable; no kernel mount or physical device is
//! involved. mkfs.ext4, debugfs and e2fsck run in the harness VM through
//! `fs_ext4_test_support::oracle`, which fails rather than skips when the
//! VM or a tool is missing.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::error::{Error, Result};
use fs_ext4::{jbd2, Filesystem};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const BLOCK: usize = 4096;
const SEQUENCE: u32 = 42;
const RECOVER: u32 = 4;
const EXPECTED_MODE: u16 = 0o100600;
const LABEL: &str = "journal-oracle";

fn command(program: &str, args: &[&str]) -> Output {
    fs_ext4_test_support::oracle(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
}

fn successful(program: &str, args: &[&str]) -> String {
    let output = command(program, args);
    assert!(output.status.success(), "{program}: {output:?}");
    String::from_utf8(output.stdout).expect("tool output")
}

fn put_be(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}
fn le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn be(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn header(kind: u32, sequence: u32) -> Vec<u8> {
    let mut block = vec![0; BLOCK];
    put_be(&mut block, 0, 0xc03b3998);
    put_be(&mut block, 4, kind);
    put_be(&mut block, 8, sequence);
    block
}

struct Fixture {
    dir: PathBuf,
    pending: PathBuf,
    inode_block: usize,
    inode_offset: usize,
    journal_block: usize,
}

impl Fixture {
    fn new(tail_revoke: bool) -> Self {
        Self::build(tail_revoke, false)
    }

    /// The journaled superblock carries `s_state` without `EXT4_VALID_FS`,
    /// as a kernel mount leaves it: replay then writes the volume back to
    /// "not clean", and only the release puts the found state back.
    fn kernel_mounted() -> Self {
        Self::build(false, true)
    }

    fn build(tail_revoke: bool, journaled_not_clean: bool) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = fs_ext4_test_support::temp_dir().join(format!(
            "jbd2-oracle-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).expect("isolated oracle directory");
        let pending = dir.join("pending.img");
        File::create(&pending)
            .unwrap()
            .set_len(32 * 1024 * 1024)
            .unwrap();
        let path = pending.to_str().unwrap();
        successful(
            "mkfs.ext4",
            &[
                "-q",
                "-F",
                "-b",
                "4096",
                "-I",
                "256",
                "-O",
                "^metadata_csum,^64bit,^orphan_file",
                "-E",
                "lazy_itable_init=0,lazy_journal_init=0",
                path,
            ],
        );
        successful("debugfs", &["-w", "-R", "write /dev/null /oracle", path]);
        let mapping = successful("debugfs", &["-R", "imap /oracle", path]);
        let location = mapping
            .lines()
            .find(|line| line.contains("located at block"))
            .unwrap();
        let words: Vec<_> = location.split_whitespace().collect();
        let inode_block = words[3].trim_end_matches(',').parse::<usize>().unwrap();
        let inode_offset = usize::from_str_radix(words[5].trim_start_matches("0x"), 16).unwrap();
        let journal: Vec<usize> = (0..10)
            .map(|logical| {
                successful("debugfs", &["-R", &format!("bmap <8> {logical}"), path])
                    .trim()
                    .parse()
                    .unwrap()
            })
            .collect();

        let mut image = fs::read(&pending).unwrap();
        let incompat = le(&image, 1024 + 0x60) | RECOVER;
        image[1024 + 0x60..1024 + 0x64].copy_from_slice(&incompat.to_le_bytes());
        let mut jsb = image[journal[0] * BLOCK..(journal[0] + 1) * BLOCK].to_vec();
        assert_eq!(be(&jsb, 0x24), 0, "plain journal compatibility flags");
        assert_eq!(
            be(&jsb, 0x28),
            0,
            "mkfs journal must have no checksum format"
        );
        let uuid = jsb[0x30..0x40].to_vec();
        put_be(&mut jsb, 0x18, SEQUENCE);
        put_be(&mut jsb, 0x1c, 1);
        put_be(&mut jsb, 0x28, 1); // Plain JBD2 with revoke support.

        let mut inode = image[inode_block * BLOCK..(inode_block + 1) * BLOCK].to_vec();
        assert_ne!(
            u16::from_le_bytes(inode[inode_offset..inode_offset + 2].try_into().unwrap()),
            EXPECTED_MODE
        );
        inode[inode_offset..inode_offset + 2].copy_from_slice(&EXPECTED_MODE.to_le_bytes());
        let mut superblock = image[..BLOCK].to_vec();
        superblock[1024 + 120..1024 + 136].fill(0);
        superblock[1024 + 120..1024 + 120 + LABEL.len()].copy_from_slice(LABEL.as_bytes());
        if journaled_not_clean {
            let state =
                u16::from_le_bytes(superblock[1024 + 0x3a..1024 + 0x3c].try_into().unwrap());
            assert_eq!(state & 1, 1, "mkfs leaves the volume clean");
            superblock[1024 + 0x3a..1024 + 0x3c].copy_from_slice(&(state & !1).to_le_bytes());
        }

        // First tag carries the UUID; the second uses SAME_UUID | LAST_TAG.
        let mut descriptor = header(1, SEQUENCE);
        put_be(&mut descriptor, 12, inode_block as u32);
        put_be(&mut descriptor, 16, 0);
        descriptor[20..36].copy_from_slice(&uuid);
        put_be(&mut descriptor, 36, 0); // primary superblock lives in fs block 0
        put_be(&mut descriptor, 40, 2 | 8);

        let mut trailing = header(1, SEQUENCE + 1);
        put_be(&mut trailing, 12, inode_block as u32);
        put_be(&mut trailing, 16, 8);
        trailing[20..36].copy_from_slice(&uuid);
        let mut uncommitted = inode.clone();
        uncommitted[inode_offset..inode_offset + 2].copy_from_slice(&0o100777u16.to_le_bytes());
        let mut blocks = vec![
            descriptor,
            inode,
            superblock,
            header(2, SEQUENCE),
            trailing,
            uncommitted,
        ];
        if tail_revoke {
            let mut revoke = header(5, SEQUENCE + 1);
            put_be(&mut revoke, 12, 20); // header + count + one 32-bit block
            put_be(&mut revoke, 16, inode_block as u32);
            blocks.push(revoke);
        }
        blocks.push(vec![0; BLOCK]); // No commit for the trailing transaction.
        for (index, block) in blocks.iter().enumerate() {
            let offset = journal[index + 1] * BLOCK;
            image[offset..offset + BLOCK].copy_from_slice(block);
        }
        image[journal[0] * BLOCK..(journal[0] + 1) * BLOCK].copy_from_slice(&jsb);
        fs::write(&pending, image).unwrap();
        Self {
            dir,
            pending,
            inode_block,
            inode_offset,
            journal_block: journal[0],
        }
    }

    fn copy(&self, name: &str) -> PathBuf {
        let path = self.dir.join(format!("{name}.img"));
        fs::copy(&self.pending, &path).unwrap();
        path
    }

    fn assert_recovered(&self, path: &Path) {
        let image = fs::read(path).unwrap();
        let offset = self.inode_block * BLOCK + self.inode_offset;
        assert_eq!(
            u16::from_le_bytes(image[offset..offset + 2].try_into().unwrap()),
            EXPECTED_MODE
        );
        assert_eq!(
            &image[1024 + 120..1024 + 120 + LABEL.len()],
            LABEL.as_bytes()
        );
        assert_eq!(
            le(&image, 1024 + 0x60) & RECOVER,
            0,
            "finished mount must clear RECOVER"
        );
        let jsb = &image[self.journal_block * BLOCK..(self.journal_block + 1) * BLOCK];
        assert_eq!(be(jsb, 0x1c), 0, "journal must be clean");
        assert!(be(jsb, 0x18) > SEQUENCE, "sequence must advance");
    }

    fn linux_recover(&self, path: &Path) {
        let transcript = fs_ext4_test_support::oracle("e2fsck")
            .args(["-fy", path.to_str().unwrap()])
            .env("LC_ALL", "C")
            .judged()
            .repaired("linux recovery");
        fs::write(path.with_extension("e2fsck.txt"), &transcript).unwrap();
        for unexpected in [
            "Fix?",
            "Clear?",
            "wrong",
            "corrupt",
            "Illegal",
            "UNEXPECTED",
        ] {
            assert!(
                !transcript.contains(unexpected),
                "recovery required unrelated repair: {transcript}"
            );
        }
        self.assert_recovered(path);
        self.linux_check(path);
    }

    fn linux_check(&self, path: &Path) {
        // The verdict refuses a check that skipped journal recovery, as
        // well as one that found anything.
        fs_ext4_test_support::oracle("e2fsck")
            .args(["-fn", path.to_str().unwrap()])
            .env("LC_ALL", "C")
            .judged()
            .clean("linux check");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::env::var_os("FS_EXT4_JOURNAL_ORACLE_KEEP").is_some() {
            eprintln!("journal oracle fixtures: {}", self.dir.display());
        } else {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

fn checked_mount(path: &Path) -> Filesystem {
    Filesystem::mount_recovering(Arc::new(
        FileDevice::open_rw(path.to_str().unwrap()).unwrap(),
    ))
    .expect("checked plain-JBD2 mount")
}

#[test]
fn committed_plain_journal_matches_linux_and_refreshes_mount_metadata() {
    for tail_revoke in [false, true] {
        let fixture = Fixture::new(tail_revoke);
        let linux = fixture.copy("linux-oracle");
        fixture.linux_recover(&linux);
        let library = fixture.copy("library");
        let mounted = checked_mount(&library);
        assert_eq!(
            mounted.sb.volume_name, LABEL,
            "replay must refresh the mount's superblock snapshot"
        );
        assert_eq!(jbd2::read_superblock(&mounted).unwrap().unwrap().start, 0);
        mounted.finish().expect("explicit clean finish");
        fixture.assert_recovered(&library);
        fixture.linux_check(&library);
        let after_first_finish = fs::read(&library).unwrap();
        checked_mount(&library)
            .finish()
            .expect("repeat checked mount and finish");
        assert_eq!(
            fs::read(&library).unwrap(),
            after_first_finish,
            "repeat recovery must be byte-stable"
        );
    }
}

#[test]
fn live_flush_and_fresh_read_keep_recovery_marker_until_finish() {
    let fixture = Fixture::new(false);
    let path = fixture.copy("live-mount");
    let mut mounted = checked_mount(&path);
    mounted.apply_pwrite("/oracle", 0, b"first").unwrap();
    mounted.flush().unwrap();
    assert_ne!(le(&fs::read(&path).unwrap(), 1024 + 0x60) & RECOVER, 0);
    mounted.fresh_read().unwrap();
    assert_ne!(le(&fs::read(&path).unwrap(), 1024 + 0x60) & RECOVER, 0);
    mounted.apply_pwrite("/oracle", 0, b"second").unwrap();
    mounted.fresh_read().unwrap();
    mounted.finish().unwrap();
    fixture.assert_recovered(&path);
    fixture.linux_check(&path);
    let output = successful("debugfs", &["-R", "cat /oracle", path.to_str().unwrap()]);
    assert_eq!(output, "second");
}

/// Two device persistence models: acknowledged writes may already be durable,
/// or all writes since the last completed flush may disappear at power loss.
struct InterruptedDevice {
    inner: FileDevice,
    fail_at: AtomicUsize,
    event: AtomicUsize,
    reads: AtomicUsize,
    writeback: bool,
    pending: Mutex<Vec<(u64, Vec<u8>)>>,
}

impl InterruptedDevice {
    fn gate(&self) -> Result<()> {
        let event = self.event.fetch_add(1, Ordering::SeqCst);
        if event >= self.fail_at.load(Ordering::SeqCst) {
            self.pending.lock().unwrap().clear();
            return Err(Error::Corrupt("injected I/O interruption"));
        }
        Ok(())
    }
}

impl BlockDevice for InterruptedDevice {
    fn read_at(&self, offset: u64, bytes: &mut [u8]) -> Result<()> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.read_at(offset, bytes)?;
        for (start, data) in self.pending.lock().unwrap().iter() {
            let left = offset.max(*start);
            let right = (offset + bytes.len() as u64).min(*start + data.len() as u64);
            if left < right {
                bytes[(left - offset) as usize..(right - offset) as usize]
                    .copy_from_slice(&data[(left - start) as usize..(right - start) as usize]);
            }
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
    fn is_writable(&self) -> bool {
        true
    }
    fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.gate()?;
        if self.writeback {
            self.pending.lock().unwrap().push((offset, bytes.to_vec()));
            Ok(())
        } else {
            self.inner.write_at(offset, bytes)
        }
    }
    fn flush(&self) -> Result<()> {
        self.gate()?;
        for (offset, bytes) in self.pending.lock().unwrap().drain(..) {
            self.inner.write_at(offset, &bytes)?;
        }
        self.inner.flush()
    }
}

fn interrupted_mount(path: &Path, fail_at: usize, writeback: bool) -> (bool, usize) {
    let device = Arc::new(InterruptedDevice {
        inner: FileDevice::open_rw(path.to_str().unwrap()).unwrap(),
        fail_at: AtomicUsize::new(fail_at),
        event: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
        writeback,
        pending: Mutex::new(Vec::new()),
    });
    let result = Filesystem::mount_recovering(device.clone()).and_then(|mounted| mounted.finish());
    let events = device.event.load(Ordering::SeqCst);
    (result.is_ok(), events)
}

#[test]
fn linux_recovers_every_checked_replay_write_and_flush_interruption() {
    let fixture = Fixture::new(true);
    for writeback in [false, true] {
        let baseline = fixture.copy(&format!("baseline-{writeback}"));
        let (success, events) = interrupted_mount(&baseline, usize::MAX, writeback);
        assert!(success);
        assert!(events >= 4, "must exercise actual writes and flushes");
        fixture.assert_recovered(&baseline);
        fixture.linux_check(&baseline);
        for fail_at in 0..events {
            let image = fixture.copy(&format!("interrupted-{writeback}-{fail_at}"));
            let (success, _) = interrupted_mount(&image, fail_at, writeback);
            assert!(
                !success,
                "I/O failure at event {fail_at} must not claim a clean finish"
            );
            fixture.linux_recover(&image);
        }
        eprintln!(
            "independent e2fsck recovery: {events} write/flush boundaries; writeback={writeback}"
        );
    }
}

#[test]
fn finish_after_a_failed_journal_operation_issues_no_device_io() {
    let fixture = Fixture::new(false);
    let image = fixture.copy("failed-operation-finish");
    let device = Arc::new(InterruptedDevice {
        inner: FileDevice::open_rw(image.to_str().unwrap()).unwrap(),
        fail_at: AtomicUsize::new(usize::MAX),
        event: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
        writeback: true,
        pending: Mutex::new(Vec::new()),
    });
    let mounted = Filesystem::mount_recovering(device.clone()).expect("checked mount");
    device
        .fail_at
        .store(device.event.load(Ordering::SeqCst), Ordering::SeqCst);
    assert!(
        mounted.apply_chmod("/oracle", 0o640).is_err(),
        "journal write must fail"
    );
    let events = device.event.load(Ordering::SeqCst);
    let reads = device.reads.load(Ordering::SeqCst);
    assert!(
        mounted.finish().is_err(),
        "failed journal must not claim a clean finish"
    );
    assert_eq!(
        device.event.load(Ordering::SeqCst),
        events,
        "finish must not write or flush after a journal failure"
    );
    assert_eq!(
        device.reads.load(Ordering::SeqCst),
        reads,
        "finish must stop before further device reads"
    );
    fixture.linux_recover(&image);
}

/// `dumpe2fs -h`'s verdict on the two flags a release changes: whether
/// `needs_recovery` is set, and whether the state reads `clean`.
fn release_flags(path: &Path) -> (bool, bool) {
    let header = successful("dumpe2fs", &["-h", path.to_str().unwrap()]);
    let field = |name: &str| {
        header
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap_or_else(|| panic!("dumpe2fs -h printed no {name}: {header}"))
            .trim()
            .to_string()
    };
    let features = field("Filesystem features:");
    let state = field("Filesystem state:");
    (
        features.split_whitespace().any(|f| f == "needs_recovery"),
        state == "clean",
    )
}

/// Mount, write (which clears the volume's clean state), then `finish` on a
/// device that fails from the `fail_at`-th device event of `finish` onward.
/// Returns whether `finish` succeeded and how many events it issued.
fn interrupted_finish(path: &Path, fail_at: usize, writeback: bool) -> (bool, usize) {
    let device = Arc::new(InterruptedDevice {
        inner: FileDevice::open_rw(path.to_str().unwrap()).unwrap(),
        fail_at: AtomicUsize::new(usize::MAX),
        event: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
        writeback,
        pending: Mutex::new(Vec::new()),
    });
    let mut mounted = Filesystem::mount_recovering(device.clone()).expect("checked mount");
    mounted.apply_pwrite("/oracle", 0, b"release").unwrap();
    mounted.flush().unwrap();
    let start = device.event.load(Ordering::SeqCst);
    device
        .fail_at
        .store(start.saturating_add(fail_at), Ordering::SeqCst);
    let result = mounted.finish();
    (result.is_ok(), device.event.load(Ordering::SeqCst) - start)
}

/// The recovery marker is the commit point of a release (#299): while it is
/// clear, the next owner takes the volume as properly put away. So a
/// `finish` that fails at any write or flush must leave either the marker
/// still set or the release complete, never the marker clear over a state
/// that was not restored. Both durability models, flags read by dumpe2fs.
#[test]
fn a_failed_finish_never_clears_the_recovery_marker_before_the_release_is_complete() {
    let fixture = Fixture::kernel_mounted();
    for writeback in [false, true] {
        let baseline = fixture.copy(&format!("release-baseline-{writeback}"));
        let (success, events) = interrupted_finish(&baseline, usize::MAX, writeback);
        assert!(success, "uninterrupted finish");
        assert!(events >= 4, "finish must write and flush both flags");
        assert_eq!(
            release_flags(&baseline),
            (false, true),
            "a completed release: marker clear, state clean"
        );
        fixture.linux_check(&baseline);
        for fail_at in 0..events {
            let image = fixture.copy(&format!("release-interrupted-{writeback}-{fail_at}"));
            let (success, _) = interrupted_finish(&image, fail_at, writeback);
            assert!(
                !success,
                "I/O failure at finish event {fail_at} must not claim a clean release"
            );
            let (needs_recovery, clean) = release_flags(&image);
            assert!(
                needs_recovery || clean,
                "finish failed at event {fail_at} (writeback={writeback}) and left \
                 needs_recovery clear over a state that is not clean: the next owner \
                 cannot tell the release was incomplete"
            );
            fixture.linux_recover(&image);
        }
    }
}
