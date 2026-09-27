//! An index built by e2fsprogs sends every name to the leaf that holds it.
//!
//! `mkfs.ext4 -d` copies a directory of names into a fresh image, then
//! `e2fsck -fyD` rebuilds that directory as an htree. It hashes each name
//! with the filesystem's own algorithm (`s_def_hash_version`) and signedness
//! (`s_flags`), both set here with `debugfs ssv`, and partitions the leaves by the
//! result. So the index on disk is the formatter's statement of the hash.
//! For every name, the descent `htree::lookup_leaf_with` performs must land
//! on the logical block that actually contains it.
//!
//! Before #96 the hash packed bytes in the wrong order, so the descent
//! matched the leaf only by chance, and `path::lookup` hid it by falling
//! back to a linear scan.
//!
//! The names include ones longer than a TEA block (16 bytes) and a half_md4
//! block (32), and ones with bytes at or above 0x80, where the signed and
//! unsigned variants differ. Fails when e2fsprogs is not installed (`chore
//! tools`).

// e2fsprogs and byte-string file names: a Unix test.
#![cfg(unix)]

use fs_ext4::dir;
use fs_ext4::file_io;
use fs_ext4::fs::Filesystem;
use fs_ext4::htree;
use fs_ext4::inode::{Inode, InodeFlags};
use fs_ext4_test_support::oracle;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

fn names() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for i in 0..1500u32 {
        let mut name = match i % 5 {
            0 => format!("f{i}").into_bytes(),
            1 => format!("a_name_of_twenty_{i:05}").into_bytes(),
            2 => format!("{i:05}_a_name_long_enough_to_need_two_md4_blocks").into_bytes(),
            3 => format!("caf\u{e9}_{i}").into_bytes(),
            _ => format!("n{i}").into_bytes(),
        };
        if i % 7 == 0 {
            name.push(0xFF);
            name.push(0x80 | (i % 64) as u8);
        }
        out.push(name);
    }
    out
}

fn run(hash_alg: &str, s_flags: u32) {
    let mkfs = "mkfs.ext4";
    let e2fsck = "e2fsck";
    let debugfs = "debugfs";
    let tag = format!("{hash_alg}_{s_flags}");
    let root = fs_ext4_test_support::temp_path!("fs_ext4_htree_src_{tag}_{}", std::process::id());
    let bigdir = std::path::Path::new(&root).join("bigdir");
    std::fs::create_dir_all(&bigdir).unwrap();
    let names = names();
    for name in &names {
        std::fs::write(bigdir.join(std::ffi::OsStr::from_bytes(name)), b"").unwrap();
    }
    let image = format!("{root}.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .unwrap();

    let ok = |call: fs_ext4_test_support::Oracle, what: &str| {
        let out = call.output();
        (
            out.status.code(),
            format!(
                "{what}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };
    let call = oracle(mkfs)
        .args(["-q", "-F", "-b", "1024", "-d"])
        .arg(&root)
        .arg(&image);
    let (code, log) = ok(call, "mkfs.ext4");
    assert_eq!(code, Some(0), "{log}");
    for request in [
        format!("ssv def_hash_version {hash_alg}"),
        format!("ssv flags {s_flags}"),
    ] {
        let call = oracle(debugfs).args(["-w", "-R"]).arg(&request).arg(&image);
        let (code, log) = ok(call, "debugfs ssv");
        assert!(
            code == Some(0) && !log.contains("Invalid"),
            "{request}: {log}"
        );
    }
    oracle(e2fsck)
        .args(["-fyD"])
        .arg(&image)
        .judged()
        .repaired("e2fsck -fyD");

    let fs = Filesystem::mount(Arc::new(
        fs_ext4::block_io::FileDevice::open(&image).unwrap(),
    ))
    .unwrap();
    assert_eq!(fs.sb.unsigned_hash(), s_flags & 2 != 0, "[{tag}] s_flags");
    let bs = fs.sb.block_size() as usize;
    let root_inode = Inode::parse(&fs.read_inode_raw(2).unwrap()).unwrap();
    let root_data = file_io::read_all(&fs, &root_inode).unwrap();
    let bigdir_ino = root_data
        .chunks(bs)
        .flat_map(|b| dir::parse_block(b, true).unwrap_or_default())
        .find(|e| e.name == b"bigdir")
        .expect("/bigdir")
        .inode;
    let inode = Inode::parse(&fs.read_inode_raw(bigdir_ino).unwrap()).unwrap();
    assert_ne!(
        inode.flags & InodeFlags::INDEX.bits(),
        0,
        "[{tag}] e2fsck -D did not index /bigdir"
    );
    let data = file_io::read_all(&fs, &inode).unwrap();
    let blocks: Vec<&[u8]> = data.chunks(bs).collect();

    let mut wrong = Vec::new();
    for name in &names {
        let leaf = htree::lookup_leaf_with(
            name,
            blocks[0],
            &fs.sb.hash_seed,
            fs.sb.unsigned_hash(),
            |logical| Ok(blocks[logical as usize].to_vec()),
        )
        .unwrap()
        .expect("a leaf");
        let found = dir::parse_block(blocks[leaf as usize], true)
            .unwrap_or_default()
            .iter()
            .any(|e| &e.name == name);
        if !found {
            wrong.push(String::from_utf8_lossy(name).into_owned());
        }
    }
    assert!(
        wrong.is_empty(),
        "[{tag}] {} of {} names descend to a leaf that does not hold them, e.g. {:?}",
        wrong.len(),
        names.len(),
        &wrong[..wrong.len().min(5)]
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&image);
}

#[test]
fn legacy_signed() {
    run("legacy", 1);
}
#[test]
fn legacy_unsigned() {
    run("legacy", 2);
}
#[test]
fn half_md4_signed() {
    run("half_md4", 1);
}
#[test]
fn half_md4_unsigned() {
    run("half_md4", 2);
}
#[test]
fn tea_signed() {
    run("tea", 1);
}
#[test]
fn tea_unsigned() {
    run("tea", 2);
}
