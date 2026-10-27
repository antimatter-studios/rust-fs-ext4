//! `import --recursive --preserve=times,links` read back by the independent
//! Linux kernel in the harness VM: one inode under three names, relative
//! and absolute symbolic links byte for byte, and access and modification
//! times at nanosecond resolution. A time the inode cannot hold is refused
//! for its own item, never truncated into one it can.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle};
use std::fs::FileTimes;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// An absolute target longer than the 60 bytes an inode holds inline, so
/// it is stored in a data block.
const LONG_TARGET: &str =
    "/srv/a/target/path/that/is/longer/than/the/sixty/bytes/an/inode/holds/inline";

/// 2040-01-01T00:00:00Z: past what a 128-byte inode's 32-bit field holds.
const PAST_2038: u64 = 2_208_988_800;

fn fresh(tag: &str, inode_size: &str) -> String {
    let img = sized_image(tag, 64 << 20);
    let out = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", inode_size, &img])
        .output();
    assert!(out.status.success(), "mkfs.ext4: {}", stderr(&out));
    img
}

fn source_dir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(image_path(tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    dir
}

fn at(secs: u64, nanos: u32) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::new(secs, nanos)
}

fn set_times(path: &Path, accessed: SystemTime, modified: SystemTime) {
    std::fs::File::open(path)
        .unwrap()
        .set_times(
            FileTimes::new()
                .set_accessed(accessed)
                .set_modified(modified),
        )
        .unwrap();
}

/// The report's item whose destination is `dest`.
fn item<'a>(report: &'a str, dest: &str) -> &'a str {
    let needle = format!("\"destination\": \"{dest}\"");
    let start = report
        .find(&needle)
        .unwrap_or_else(|| panic!("no item for {dest} in:\n{report}"));
    let rest = &report[start + needle.len()..];
    &rest[..rest.find("\"path\": ").unwrap_or(rest.len())]
}

/// The value after `key ` on the guest's output line that starts with it.
fn line(listing: &str, key: &str) -> String {
    listing
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key} ")))
        .unwrap_or_else(|| panic!("no {key} in:\n{listing}"))
        .to_string()
}

#[test]
fn preserve_times_links_keeps_three_names_both_symlinks_and_nanosecond_times() {
    let img = fresh("import-preserve", "256");
    let src = source_dir("import-preserve-src");
    std::fs::write(src.join("data"), pattern(9000, 5)).unwrap();
    std::fs::hard_link(src.join("data"), src.join("data.two")).unwrap();
    std::fs::create_dir(src.join("sub")).unwrap();
    std::fs::hard_link(src.join("data"), src.join("sub/data.three")).unwrap();
    symlink("../data", src.join("sub/rel")).unwrap();
    symlink("/etc/hostname", src.join("abs")).unwrap();
    symlink(LONG_TARGET, src.join("abs.long")).unwrap();
    set_times(
        &src.join("data"),
        at(1_100_000_000, 123_456_789),
        at(1_200_000_000, 987_654_321),
    );
    // Last, so that adding its entries does not move it.
    set_times(
        &src.join("sub"),
        at(1_300_000_000, 1),
        at(1_400_000_000, 999_999_999),
    );
    let src = src.to_str().unwrap();

    let report = stdout(&ok(tool("fs.ext4").args([
        &img,
        "import",
        "--recursive",
        "--preserve=times,links",
        src,
        "/tree",
    ])));
    for dest in [
        "/tree/abs",
        "/tree/abs.long",
        "/tree/data",
        "/tree/data.two",
        "/tree/sub",
        "/tree/sub/data.three",
        "/tree/sub/rel",
    ] {
        assert!(
            item(&report, dest).contains("\"status\": \"completed\""),
            "{dest}: {report}"
        );
    }

    // Every stat comes before any read, so the guest cannot move an atime.
    let seen = guest_kernel_write(
        &img,
        "cd \"$MNT/tree\"\n\
         echo names $(stat -c '%i:%h' data data.two sub/data.three)\n\
         echo data $(stat -c '%.9X %.9Y' data)\n\
         echo sub $(stat -c '%.9X %.9Y' sub)\n\
         echo rel $(readlink sub/rel)\n\
         echo abs $(readlink abs)\n\
         echo long $(readlink abs.long)\n\
         echo types $(stat -c %F sub/rel abs abs.long | tr ' ' _)\n",
    );
    assert!(seen.status.success(), "{}", stderr(&seen));
    let listing = stdout(&seen);
    let names = line(&listing, "names");
    let names: Vec<&str> = names.split(' ').collect();
    assert_eq!(names.len(), 3, "{listing}");
    assert!(
        names.iter().all(|n| *n == names[0]) && names[0].ends_with(":3"),
        "one inode, link count 3: {listing}"
    );
    assert_eq!(
        line(&listing, "data"),
        "1100000000.123456789 1200000000.987654321"
    );
    assert_eq!(
        line(&listing, "sub"),
        "1300000000.000000001 1400000000.999999999"
    );
    assert_eq!(line(&listing, "rel"), "../data");
    assert_eq!(line(&listing, "abs"), "/etc/hostname");
    assert_eq!(line(&listing, "long"), LONG_TARGET);
    assert_eq!(
        line(&listing, "types"),
        "symbolic_link symbolic_link symbolic_link"
    );
    assert_e2fsck_clean(&img, "import --preserve=times,links");
    std::fs::remove_dir_all(src).unwrap();
}

#[test]
fn preserve_times_refuses_a_time_the_inode_cannot_hold_for_that_item_alone() {
    // 128-byte inodes have no *_extra fields: 32-bit seconds, up to 2038.
    let img = fresh("import-preserve-range", "128");
    let src = source_dir("import-preserve-range-src");
    std::fs::write(src.join("late"), b"late").unwrap();
    std::fs::write(src.join("ok"), b"ok").unwrap();
    set_times(&src.join("late"), at(PAST_2038, 0), at(PAST_2038, 0));
    set_times(&src.join("ok"), at(1_000_000_000, 0), at(1_000_000_000, 0));
    let src = src.to_str().unwrap();

    let out = tool("fs.ext4")
        .args([
            &img,
            "import",
            "--recursive",
            "--preserve=times",
            src,
            "/tree",
        ])
        .output()
        .unwrap();
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
    assert_eq!(json_field(&report, "refused"), "1", "{report}");
    assert_eq!(json_field(&report, "failed"), "0", "{report}");
    assert!(
        item(&report, "/tree/late").contains("\"status\": \"refused\""),
        "{report}"
    );
    assert!(
        item(&report, "/tree/ok").contains("\"status\": \"completed\""),
        "{report}"
    );

    let seen = guest_kernel_write(
        &img,
        "cd \"$MNT/tree\"\n\
         echo late $(stat -c %Y late)\n\
         echo ok $(stat -c %Y ok)\n",
    );
    assert!(seen.status.success(), "{}", stderr(&seen));
    let listing = stdout(&seen);
    // Truncated to 32 bits, 2040 would read back as 1904.
    let wrapped = (PAST_2038 as i64 - (1i64 << 32)).to_string();
    let late = line(&listing, "late");
    assert_ne!(late, wrapped, "{listing}");
    assert_ne!(late, PAST_2038.to_string(), "{listing}");
    assert_eq!(line(&listing, "ok"), "1000000000", "{listing}");
    assert_e2fsck_clean(&img, "import refusing an unstorable time");
    std::fs::remove_dir_all(src).unwrap();
}
