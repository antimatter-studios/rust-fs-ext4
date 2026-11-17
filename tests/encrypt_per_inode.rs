//! A volume with the ENCRYPT feature mounts, reads its plain files as
//! e2fsprogs does, and refuses the encrypted ones by name (#76).
//!
//! `mkfs.ext4 -O encrypt -d` builds the volume. Marking a directory and a
//! file encrypted needs an fscrypt policy from a kernel, so `debugfs` sets
//! `EXT4_ENCRYPT_FL` on them instead: what this driver must do with such an
//! inode depends only on the flag, never on the ciphertext. The plain files'
//! reference is `debugfs cat`. Fails when e2fsprogs is not installed
//! (they run in the harness VM).

#![cfg(unix)]

use fs_ext4::block_io::FileDevice;
use fs_ext4::Filesystem;
use std::sync::Arc;

fn run(tool: &str, args: &[&str]) -> (Option<i32>, Vec<u8>, String) {
    let out = fs_ext4_test_support::oracle(tool).args(args).output();
    (
        out.status.code(),
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn lookup(fs: &Filesystem, path: &str) -> fs_ext4::Result<u32> {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    fs_ext4::path::lookup_with_csum(fs.dev.as_ref(), &fs.sb, &mut reader, path, &fs.csum)
}

fn read(fs: &Filesystem, path: &str) -> fs_ext4::Result<Vec<u8>> {
    let ino = lookup(fs, path)?;
    let (inode, raw) = fs.read_inode_verified(ino)?;
    let mut out = vec![0u8; inode.size as usize];
    let n =
        fs_ext4::file_io::read_with_raw_verified(fs, &inode, &raw, ino, 0, inode.size, &mut out)?;
    out.truncate(n as usize);
    Ok(out)
}

fn names_encryption(e: &fs_ext4::Error) -> bool {
    matches!(e, fs_ext4::Error::Unsupported(msg) if msg.contains("encrypted"))
}

#[test]
fn plain_files_read_and_encrypted_ones_are_refused() {
    let mkfs = "mkfs.ext4";
    let debugfs = "debugfs";
    let root = fs_ext4_test_support::temp_path!("fs_ext4_encrypt_{}", std::process::id());
    std::fs::create_dir_all(format!("{root}/plain")).unwrap();
    std::fs::create_dir_all(format!("{root}/secret")).unwrap();
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(format!("{root}/plain/big.bin"), &big).unwrap();
    std::fs::write(format!("{root}/plain/small.txt"), b"plain text\n").unwrap();
    std::fs::write(format!("{root}/plain/sealed.bin"), vec![0xC3; 9000]).unwrap();
    std::fs::write(format!("{root}/secret/inside.bin"), vec![0x5A; 5000]).unwrap();
    let image = format!("{root}.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(32 * 1024 * 1024))
        .unwrap();
    let (code, _, log) = run(mkfs, &["-q", "-F", "-O", "encrypt", "-d", &root, &image]);
    assert_eq!(code, Some(0), "{log}");
    let (code, _, log) = run(
        debugfs,
        &["-w", "-R", "symlink /plain/link small.txt", &image],
    );
    assert_eq!(code, Some(0), "{log}");
    // EXTENTS | ENCRYPT on the directory, the file beside plain ones, and the
    // fast symlink (whose target lives in i_block).
    for (path, flags) in [
        ("/secret", "0x80800"),
        ("/plain/sealed.bin", "0x80800"),
        ("/plain/link", "0x800"),
    ] {
        let (code, _, log) = run(
            debugfs,
            &[
                "-w",
                "-R",
                &format!("set_inode_field {path} flags {flags}"),
                &image,
            ],
        );
        assert_eq!(code, Some(0), "{log}");
    }

    let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap()))
        .expect("a volume with the ENCRYPT feature mounts");

    for path in ["/plain/big.bin", "/plain/small.txt"] {
        let (code, reference, log) = run(debugfs, &["-R", &format!("cat {path}"), &image]);
        assert_eq!(code, Some(0), "{log}");
        assert!(
            read(&fs, path).unwrap() == reference,
            "{path} differs from debugfs cat"
        );
    }
    assert!(read(&fs, "/plain/big.bin").unwrap() == big);

    let err = read(&fs, "/plain/sealed.bin").expect_err("an encrypted file's ciphertext");
    assert!(names_encryption(&err), "{err:?}");
    let err = lookup(&fs, "/secret/inside.bin").expect_err("a name in an encrypted directory");
    assert!(names_encryption(&err), "{err:?}");
    // The directory itself is still there to stat.
    lookup(&fs, "/secret").expect("the encrypted directory's own entry is plain");
    drop(fs);

    // The C ABI's listing and readlink refuse too.
    unsafe {
        let cpath = std::ffi::CString::new(image.clone()).unwrap();
        let handle = fs_ext4::capi::fs_ext4_mount(cpath.as_ptr());
        assert!(!handle.is_null());
        let secret = std::ffi::CString::new("/secret").unwrap();
        let iter = fs_ext4::capi::fs_ext4_dir_open(handle, secret.as_ptr());
        assert!(iter.is_null(), "an encrypted directory was listed");
        let link = std::ffi::CString::new("/plain/link").unwrap();
        let mut buf = [0 as std::ffi::c_char; 64];
        let rc = fs_ext4::capi::fs_ext4_readlink(handle, link.as_ptr(), buf.as_mut_ptr(), 64);
        assert_eq!(rc, -1, "an encrypted symlink's target was returned");
        let plain = std::ffi::CString::new("/plain").unwrap();
        let iter = fs_ext4::capi::fs_ext4_dir_open(handle, plain.as_ptr());
        assert!(!iter.is_null(), "a plain directory beside it lists");
        fs_ext4::capi::fs_ext4_dir_close(iter);
        fs_ext4::capi::fs_ext4_umount(handle);
    }

    // Writes are refused volume-wide: nothing on the write side reads the
    // flag.
    let err = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap()))
        .err()
        .expect("a writable mount of an ENCRYPT volume");
    assert!(
        matches!(err, fs_ext4::Error::UnsupportedIncompat(_)),
        "{err:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&image);
}
