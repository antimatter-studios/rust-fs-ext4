//! Content writes to the kernel's inline-data files (#428).
//!
//! `ext4-inline.img` holds files the kernel wrote with `-O inline_data`:
//! `/tiny.txt` (12 bytes, all in `i_block`) and `/medium.txt` (100 bytes,
//! the last 40 in the `system.data` attribute). Every write here is one
//! that was refused with `Unsupported` before (#383). A write whose result
//! still fits in the inode is made in place and the file stays inline; one
//! that outgrows it converts the file to an extent-mapped one in the same
//! transaction. Each case runs on a fresh in-memory copy of the fixture and
//! is read back through a new mount against a model of the bytes.
//!
//! e2fsck's and the kernel's view of the same writes are in
//! `tests/inline_file_writes_oracle.rs` and `tests/kernel_inline_writes.rs`.

use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::Result;
use fs_ext4::Filesystem;
use std::sync::{Arc, Mutex};

struct MemDev(Mutex<Vec<u8>>);

impl BlockDevice for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let b = self.0.lock().unwrap();
        let start = offset as usize;
        buf.copy_from_slice(&b[start..start + buf.len()]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut b = self.0.lock().unwrap();
        let start = offset as usize;
        b[start..start + buf.len()].copy_from_slice(buf);
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

fn image() -> Arc<MemDev> {
    let path = fs_ext4_test_support::fixture(env!("CARGO_MANIFEST_DIR"), "ext4-inline.img");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    Arc::new(MemDev(Mutex::new(bytes)))
}

fn mount(dev: &Arc<MemDev>) -> Filesystem {
    Filesystem::mount(dev.clone()).expect("mount")
}

fn ino_of(fs: &Filesystem, name: &str) -> u32 {
    fs.lookup_at(2u32, name.as_bytes()).expect("lookup")
}

/// Every byte of the file, read through a fresh mount, and whether it is
/// still inline.
fn read_back(dev: &Arc<MemDev>, name: &str) -> (Vec<u8>, bool) {
    let fs = mount(dev);
    let ino = ino_of(&fs, name);
    let inode = fs.stat_ino(ino).unwrap();
    let mut out = vec![0u8; inode.size as usize];
    let n = fs.read_ino(ino, 0, &mut out).unwrap();
    assert_eq!(n, out.len(), "{name}: short read");
    (out, inode.has_inline_data())
}

fn tiny() -> Vec<u8> {
    b"tiny inline\n".to_vec()
}

fn medium() -> Vec<u8> {
    vec![b'A'; 100]
}

/// `model` with `data` written at `offset`, zero-filled up to it.
fn spliced(model: &[u8], offset: usize, data: &[u8]) -> Vec<u8> {
    let mut out = model.to_vec();
    if out.len() < offset + data.len() {
        out.resize(offset + data.len(), 0);
    }
    out[offset..offset + data.len()].copy_from_slice(data);
    out
}

fn resized(model: &[u8], len: usize) -> Vec<u8> {
    let mut out = model.to_vec();
    out.resize(len, 0);
    out
}

type Op = fn(&Filesystem, u32) -> Result<()>;

/// One write and what it must leave: the file's bytes, and whether it is
/// still inline.
struct Case {
    what: &'static str,
    file: &'static str,
    op: Op,
    want: fn() -> Vec<u8>,
    inline: bool,
}

const CASES: &[Case] = &[
    Case {
        what: "pwrite inside i_block",
        file: "tiny.txt",
        op: |fs, _| fs.apply_pwrite("/tiny.txt", 5, b"XY").map(drop),
        want: || spliced(&tiny(), 5, b"XY"),
        inline: true,
    },
    Case {
        what: "pwrite growing into system.data",
        file: "tiny.txt",
        op: |fs, ino| fs.apply_pwrite_ino(ino, 12, &[b'b'; 50]).map(drop),
        want: || spliced(&tiny(), 12, &[b'b'; 50]),
        inline: true,
    },
    Case {
        what: "pwrite inside system.data",
        file: "medium.txt",
        op: |fs, _| fs.apply_pwrite("/medium.txt", 70, b"middle").map(drop),
        want: || spliced(&medium(), 70, b"middle"),
        inline: true,
    },
    Case {
        what: "pwrite past what the inode holds",
        file: "medium.txt",
        op: |fs, ino| fs.apply_pwrite_ino(ino, 100, &[b'c'; 1000]).map(drop),
        want: || spliced(&medium(), 100, &[b'c'; 1000]),
        inline: false,
    },
    Case {
        what: "pwrite a megabyte out",
        file: "tiny.txt",
        op: |fs, _| fs.apply_pwrite("/tiny.txt", 1 << 20, b"z").map(drop),
        want: || spliced(&tiny(), 1 << 20, b"z"),
        inline: false,
    },
    Case {
        what: "pwrite of several blocks",
        file: "tiny.txt",
        op: |fs, ino| fs.apply_pwrite_ino(ino, 3, &[b'd'; 20_000]).map(drop),
        want: || spliced(&tiny(), 3, &[b'd'; 20_000]),
        inline: false,
    },
    Case {
        what: "replace with less",
        file: "medium.txt",
        op: |fs, _| {
            fs.apply_replace_file_content("/medium.txt", b"hello")
                .map(drop)
        },
        want: || b"hello".to_vec(),
        inline: true,
    },
    Case {
        what: "replace with nothing",
        file: "medium.txt",
        op: |fs, _| fs.apply_replace_file_content("/medium.txt", b"").map(drop),
        want: Vec::new,
        inline: true,
    },
    Case {
        what: "replace with more than the inode holds",
        file: "tiny.txt",
        op: |fs, _| {
            fs.apply_replace_file_content("/tiny.txt", &[b'e'; 5000])
                .map(drop)
        },
        want: || vec![b'e'; 5000],
        inline: false,
    },
    Case {
        what: "truncate shrink out of system.data",
        file: "medium.txt",
        op: |fs, ino| fs.apply_truncate_shrink(ino, 30),
        want: || resized(&medium(), 30),
        inline: true,
    },
    Case {
        what: "truncate grow into system.data",
        file: "tiny.txt",
        op: |fs, ino| fs.apply_truncate_ino(ino, 80),
        want: || resized(&tiny(), 80),
        inline: true,
    },
    Case {
        what: "truncate grow past what the inode holds",
        file: "medium.txt",
        op: |fs, ino| fs.apply_truncate_grow(ino, 8192),
        want: || resized(&medium(), 8192),
        inline: false,
    },
    Case {
        what: "shrink then write past the old end",
        file: "medium.txt",
        op: |fs, ino| {
            fs.apply_truncate_ino(ino, 30)?;
            fs.apply_pwrite_ino(ino, 90, b"q").map(drop)
        },
        want: || spliced(&resized(&medium(), 30), 90, b"q"),
        inline: true,
    },
    Case {
        what: "shrink then grow past the inode",
        file: "medium.txt",
        op: |fs, ino| {
            fs.apply_truncate_ino(ino, 30)?;
            fs.apply_truncate_ino(ino, 5000)
        },
        want: || resized(&resized(&medium(), 30), 5000),
        inline: false,
    },
];

#[test]
fn writes_to_the_kernels_inline_files_succeed_and_read_back() {
    let mut failures = Vec::new();
    for case in CASES {
        let dev = image();
        let fs = mount(&dev);
        let ino = ino_of(&fs, case.file);
        assert!(fs.stat_ino(ino).unwrap().has_inline_data(), "fixture");
        let r = (case.op)(&fs, ino);
        drop(fs);
        if let Err(e) = r {
            failures.push(format!("{}: {e:?}", case.what));
            continue;
        }
        let (got, inline) = read_back(&dev, case.file);
        let want = (case.want)();
        if got != want {
            failures.push(format!(
                "{}: {} bytes read back, {} expected, first difference at {:?}",
                case.what,
                got.len(),
                want.len(),
                got.iter().zip(&want).position(|(a, b)| a != b)
            ));
        }
        if inline != case.inline {
            failures.push(format!(
                "{}: inline is {inline}, expected {}",
                case.what, case.inline
            ));
        }
        // The other file is untouched.
        let (other, name) = if case.file == "tiny.txt" {
            (medium(), "medium.txt")
        } else {
            (tiny(), "tiny.txt")
        };
        if read_back(&dev, name) != (other, true) {
            failures.push(format!("{}: {name} changed", case.what));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A device that stops writing after `budget` writes and reports success,
/// as a power cut would look to the writer.
struct CutDev {
    inner: Arc<MemDev>,
    budget: usize,
    writes: std::sync::atomic::AtomicUsize,
}

impl BlockDevice for CutDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let n = self
            .writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n < self.budget {
            self.inner.write_at(offset, buf)?;
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// THE CONVERSION AND THE WRITE ARE ONE TRANSACTION. Cut after every
/// device write of a write that converts `/medium.txt` out of the inode,
/// then mount (which replays the journal): the file is the kernel's inline
/// file or the written, converted one, never a mixture, and the volume's
/// own audit finds nothing.
#[test]
fn a_write_that_converts_an_inline_file_is_atomic_across_a_cut() {
    type Write = fn(&Filesystem, u32) -> Result<()>;
    type Bytes = fn() -> Vec<u8>;
    let writes: [(&str, Write, Bytes); 3] = [
        (
            "pwrite",
            |fs, ino| fs.apply_pwrite_ino(ino, 100, &[b'c'; 1000]).map(drop),
            || spliced(&medium(), 100, &[b'c'; 1000]),
        ),
        (
            "truncate",
            |fs, ino| fs.apply_truncate_ino(ino, 8192),
            || resized(&medium(), 8192),
        ),
        (
            "replace",
            |fs, _| {
                fs.apply_replace_file_content("/medium.txt", &[b'r'; 9000])
                    .map(drop)
            },
            || vec![b'r'; 9000],
        ),
    ];
    let snapshot = image().0.lock().unwrap().clone();
    for (what, write, want) in writes {
        let mut total = None;
        for cut in 0.. {
            let dev = Arc::new(MemDev(Mutex::new(snapshot.clone())));
            let crash = Arc::new(CutDev {
                inner: dev.clone(),
                budget: cut,
                writes: std::sync::atomic::AtomicUsize::new(0),
            });
            {
                let fs = Filesystem::mount(crash.clone()).expect("mount");
                assert!(fs.journal.is_some(), "fixture: the volume has a journal");
                let ino = ino_of(&fs, "medium.txt");
                write(&fs, ino).unwrap_or_else(|e| panic!("{what}, cut {cut}: {e:?}"));
            }
            let written = crash.writes.load(std::sync::atomic::Ordering::SeqCst);
            total.get_or_insert(written);

            let (got, inline) = read_back(&dev, "medium.txt");
            assert!(
                (got == medium() && inline) || (got == want() && !inline),
                "{what}, cut {cut} of {written}: {} bytes, inline {inline}",
                got.len()
            );
            let fs = mount(&dev);
            let report = fs_ext4::fsck::audit(&fs, u32::MAX, u32::MAX).expect("audit");
            assert!(
                report.anomalies.is_empty(),
                "{what}, cut {cut}: {:?}",
                report.anomalies
            );
            drop(fs);
            if cut >= written {
                break;
            }
        }
        assert!(total.unwrap_or(0) > 1, "{what}: nothing to cut");
    }
}
