//! `fs.ext4` reads a volume it did not make: `mkfs.ext4` (e2fsprogs) made
//! it and `debugfs -w` wrote its files, both in the harness VM. The names,
//! sizes, types and bytes `fs.ext4` reports must be the ones e2fsprogs
//! wrote, and `get` must agree with `dumpe2fs -h` on the superblock.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, oracle};

/// Bytes nobody would type: a fixed LCG, so a failure reproduces.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (x >> 16) as u8
        })
        .collect()
}

fn dumpe2fs_field(header: &str, name: &str) -> String {
    header
        .lines()
        .find(|l| l.starts_with(&format!("{name}:")))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| panic!("dumpe2fs -h has no {name}:\n{header}"))
}

#[test]
fn fs_ext4_reads_back_what_e2fsprogs_wrote() {
    let img = sized_image("e2fsprogs-made", 32 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-L", "REFVOL", &img])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));

    let files: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one", pattern(1, 1)),
        ("f4095", pattern(4095, 2)),
        ("f4096", pattern(4096, 3)),
        ("f4097", pattern(4097, 4)),
        ("big", pattern(1 << 20, 5)),
    ];
    let inner = pattern(5000, 6);
    let mut script = String::from("mkdir d\nsymlink s one\n");
    for (name, bytes) in files.iter().chain([("d-inner", inner.clone())].iter()) {
        let src = image_path(&format!("src-{name}"));
        std::fs::write(&src, bytes).unwrap();
        if *name == "d-inner" {
            script.push_str(&format!("cd d\nwrite {src} inner\ncd /\n"));
        } else {
            script.push_str(&format!("write {src} {name}\n"));
        }
    }
    oracle("debugfs")
        .args(["-w", "-f", "-", &img])
        .stdin(script.into_bytes())
        .output();
    assert_e2fsck_clean(&img, "the volume debugfs wrote");

    let listing = stdout(&ok(tool("fs.ext4").args([&img, "ls", "/"])));
    for (name, bytes) in &files {
        let at = listing
            .find(&format!("\"name\": \"{name}\""))
            .unwrap_or_else(|| panic!("{name} not listed:\n{listing}"));
        let entry = &listing[at..at + listing[at..].find('}').unwrap()];
        assert_eq!(json_field(entry, "type"), "file", "{entry}");
        assert_eq!(
            json_field(entry, "size"),
            bytes.len().to_string(),
            "{entry}"
        );
        let read = ok(tool("fs.ext4").args([&img, "read", &format!("/{name}")]));
        assert!(
            read.stdout == *bytes,
            "/{name}: the bytes read differ from those written"
        );
    }
    let s = &listing[listing.find("\"name\": \"s\"").expect("s listed")..];
    assert_eq!(json_field(s, "type"), "symlink");
    assert_eq!(json_field(s, "target"), "one");
    let d = stdout(&ok(tool("fs.ext4").args([&img, "ls", "/d"])));
    assert!(d.contains("\"name\": \"inner\""), "{d}");
    assert!(ok(tool("fs.ext4").args([&img, "read", "/d/inner"])).stdout == inner);

    // `get` against dumpe2fs, field by field.
    let get = stdout(&ok(tool("fs.ext4").args([&img, "get"])));
    let header = oracle("dumpe2fs").args(["-h", &img]).output();
    let header = stdout(&header);
    assert_eq!(
        dumpe2fs_field(&header, "Filesystem volume name"),
        json_field(&get, "label")
    );
    assert_eq!(json_field(&get, "label"), "REFVOL");
    assert_eq!(
        dumpe2fs_field(&header, "Filesystem UUID"),
        json_field(&get, "uuid")
    );
    assert_eq!(
        dumpe2fs_field(&header, "Block size"),
        json_field(&get, "block_size")
    );
    assert_eq!(
        dumpe2fs_field(&header, "Block count"),
        json_field(&get, "total_blocks")
    );
    assert_eq!(
        dumpe2fs_field(&header, "Free blocks"),
        json_field(&get, "free_blocks")
    );
    assert_eq!(
        dumpe2fs_field(&header, "Inode count"),
        json_field(&get, "total_inodes")
    );
    assert_eq!(
        dumpe2fs_field(&header, "Free inodes"),
        json_field(&get, "free_inodes")
    );
    let block: u64 = json_field(&get, "block_size").parse().unwrap();
    let blocks: u64 = dumpe2fs_field(&header, "Block count").parse().unwrap();
    let free: u64 = dumpe2fs_field(&header, "Free blocks").parse().unwrap();
    assert_eq!(
        json_field(&get, "total_bytes"),
        (blocks * block).to_string()
    );
    assert_eq!(json_field(&get, "free_bytes"), (free * block).to_string());
    assert_eq!(
        dumpe2fs_field(&header, "Filesystem state") == "clean",
        json_field(&get, "dirty") == "false",
        "dumpe2fs says the state is {:?}; get says dirty = {}",
        dumpe2fs_field(&header, "Filesystem state"),
        json_field(&get, "dirty")
    );
}
