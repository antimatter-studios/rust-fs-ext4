//! The first allocation into a `BLOCK_UNINIT` group must not be handed the
//! group's own metadata.
//!
//! An uninit group's bitmap is implied, not stored, and what it implies is
//! not "empty": a group that carries a backup keeps its superblock, GDT and
//! reserved GDT blocks at its head, and without flex_bg its own bitmaps and
//! inode table too. The block planner fabricated an all-zero bitmap for
//! such a group, so the first block it offered in group 1 was the backup
//! superblock. `e2fsck` then reported the block as claimed by both a file
//! and the filesystem, plus a free count off by one.
//!
//! The volume comes from the real `mkfs.ext4` because this crate's own mkfs
//! never leaves a group uninit. 1 KiB blocks make each group 8 MiB, so a
//! 64 MiB image has eight groups and directory spreading reaches the
//! untouched ones straight away. Fails when `mkfs.ext4` or `e2fsck` is not
//! installed (they run in the harness VM).

use fs_ext4::block_io::FileDevice;
use fs_ext4::Filesystem;
use std::sync::Arc;

fn run(tag: &str, features: &str) {
    let mkfs = "mkfs.ext4";
    let path = fs_ext4_test_support::temp_path!("fs_ext4_uninit_{tag}_{}.img", std::process::id());
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .expect("size the image");
    let out = fs_ext4_test_support::oracle(mkfs)
        .args(["-q", "-F", "-b", "1024", "-O", features])
        .arg(&path)
        .output();
    assert!(
        out.status.success(),
        "mkfs.ext4: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&path).expect("open_rw")))
            .expect("mount");
        assert!(fs.groups.len() > 2, "[{tag}] needs several groups");
        for d in 0..4 {
            let dir = format!("/d{d}");
            fs.apply_mkdir(&dir, 0o755).expect("mkdir");
            for f in 0..3 {
                let file = format!("{dir}/f{f}");
                fs.apply_create(&file, 0o644).expect("create");
                fs.apply_pwrite(&file, 0, &vec![0x5A; 20_000])
                    .expect("pwrite");
            }
        }
        fs.dev.flush().expect("flush");
    }

    // `e2fsck -n` can print a problem as IGNORED and still exit 0; the
    // verdict reads the report, not the exit status.
    fs_ext4_test_support::assert_e2fsck_clean(&path, tag);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sparse_super_backups_are_not_allocated() {
    run("flex", "metadata_csum");
}

#[test]
fn a_non_flex_bg_groups_own_bitmaps_and_table_are_not_allocated() {
    run("noflex", "metadata_csum,^flex_bg");
}
