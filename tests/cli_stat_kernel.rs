//! `fs.ext4 stat` reports what THE KERNEL reports: a volume made by
//! `mkfs.ext4` (e2fsprogs) in the harness VM, filled there by Linux, and
//! every path put to the guest's own `stat`. Identity, type, permission
//! bits, owner, links, size, allocated blocks, device numbers and all four
//! times to the nanosecond must be the ones the kernel printed, with a
//! symlink reported rather than followed.
//!
//! The second test is the other half of "a field the format does not
//! store is null": a 128-byte inode has no room for nanoseconds or a
//! creation time, the kernel says so (`%w` is `-`), and so must `stat`.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{guest_kernel_write, oracle};

/// One line per path: `LABEL|` then the guest's `stat -c FORMAT`.
/// Raw mode in hex, owner, links, size, blocks and their unit, the four
/// times as whole seconds and in the human form whose fraction is the
/// nanoseconds, and the device numbers in hex.
const FORMAT: &str = "%i|%f|%u|%g|%h|%s|%b|%B|%X|%x|%Y|%y|%Z|%z|%W|%w|%t|%T";

fn stat_script(fill: &str, paths: &[&str]) -> String {
    let mut script = format!("cd \"$MNT\"\n{fill}sync\n");
    for path in paths {
        let at = if *path == "/" { "." } else { &path[1..] };
        script.push_str(&format!(
            "printf '%s|' {path}; stat -c '{FORMAT}' -- {at}\n"
        ));
    }
    script
}

/// The nanoseconds in a `stat` human time, `2001-02-03 04:05:06.123456789 +0000`.
fn nanos(human: &str) -> String {
    let fraction = human
        .split_once('.')
        .map(|(_, rest)| rest.split(' ').next().unwrap_or(""))
        .unwrap_or_else(|| panic!("no fraction in the guest's time {human:?}"));
    assert_eq!(fraction.len(), 9, "the guest's time {human:?}");
    fraction
        .trim_start_matches('0')
        .parse::<u64>()
        .unwrap_or(0)
        .to_string()
}

fn kind(raw_mode: u32) -> &'static str {
    match raw_mode & 0o170000 {
        0o100000 => "file",
        0o040000 => "dir",
        0o120000 => "symlink",
        0o020000 => "char",
        0o060000 => "block",
        0o010000 => "fifo",
        0o140000 => "socket",
        _ => "unknown",
    }
}

/// Every field the guest printed for each path, against `fs.ext4 stat`.
#[track_caller]
fn assert_agrees(img: &str, report: &str, paths: &[&str], times_stored: bool) {
    let mut seen = Vec::new();
    for line in report.lines() {
        let f: Vec<&str> = line.split('|').collect();
        assert_eq!(f.len(), 19, "a guest stat line: {line}");
        let path = f[0];
        seen.push(path.to_string());
        let ours = stdout(&ok(tool("fs.ext4").args([img, "stat", path])));
        let field = |k: &str| json_field(&ours, k);
        let raw_mode = u32::from_str_radix(f[2], 16).unwrap();
        let what = |k: &str| format!("{path}: {k}\nours:\n{ours}\nthe guest's: {line}");

        assert_eq!(field("path"), path, "{}", what("path"));
        assert_eq!(field("inode"), f[1], "{}", what("inode"));
        assert_eq!(field("type"), kind(raw_mode), "{}", what("type"));
        assert_eq!(
            field("mode"),
            format!("{:04o}", raw_mode & 0o7777),
            "{}",
            what("mode")
        );
        assert_eq!(field("uid"), f[3], "{}", what("uid"));
        assert_eq!(field("gid"), f[4], "{}", what("gid"));
        assert_eq!(field("nlink"), f[5], "{}", what("nlink"));
        assert_eq!(field("size"), f[6], "{}", what("size"));
        let allocated = f[7].parse::<u64>().unwrap() * f[8].parse::<u64>().unwrap();
        assert_eq!(
            field("allocated_bytes"),
            allocated.to_string(),
            "{}",
            what("allocated_bytes")
        );
        for (key, secs, human) in [
            ("atime", f[9], f[10]),
            ("mtime", f[11], f[12]),
            ("ctime", f[13], f[14]),
        ] {
            assert_eq!(field(key), secs, "{}", what(key));
            let nsec = format!("{key}_nsec");
            if times_stored {
                assert_eq!(field(&nsec), nanos(human), "{}", what(&nsec));
            } else {
                assert_eq!(nanos(human), "0", "{}", what(&nsec));
                assert_eq!(field(&nsec), "null", "{}", what(&nsec));
            }
        }
        if times_stored {
            assert_eq!(field("btime"), f[15], "{}", what("btime"));
            assert_eq!(field("btime_nsec"), nanos(f[16]), "{}", what("btime_nsec"));
        } else {
            assert_eq!(f[16], "-", "{}", what("btime"));
            assert_eq!(field("btime"), "null", "{}", what("btime"));
            assert_eq!(field("btime_nsec"), "null", "{}", what("btime_nsec"));
        }
        match kind(raw_mode) {
            "char" | "block" => {
                let major = u32::from_str_radix(f[17], 16).unwrap();
                let minor = u32::from_str_radix(f[18], 16).unwrap();
                assert_eq!(field("major"), major.to_string(), "{}", what("major"));
                assert_eq!(field("minor"), minor.to_string(), "{}", what("minor"));
            }
            "symlink" => assert!(ours.contains("\"target\": "), "{}", what("target")),
            _ => assert!(!ours.contains("\"target\": "), "{}", what("target")),
        }
    }
    assert_eq!(
        seen, paths,
        "the guest printed a line for each path:\n{report}"
    );
}

#[test]
fn fs_ext4_stat_reports_what_the_kernel_reports() {
    let img = sized_image("stat-kernel", 32 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256", &img])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));

    // Nothing reads a file once it is written, so no access time moves
    // between the guest's stat and the unmount that writes the inodes.
    let fill = "mkdir dir\n\
                chmod 1777 dir\n\
                head -c 5000 /dev/urandom > file\n\
                chown 1234:5678 file\n\
                chmod 0640 file\n\
                touch -m -d '2001-02-03 04:05:06.123456789' file\n\
                ln file hard\n\
                : > empty\n\
                printf x > old\n\
                touch -d '1960-01-02 03:04:05.5' old\n\
                ln -s file link\n\
                mkfifo fifo\n\
                mknod chr c 1 3\n\
                mknod blk b 7 300\n";
    let paths = [
        "/", "/dir", "/file", "/hard", "/empty", "/old", "/link", "/fifo", "/chr", "/blk",
    ];
    let out = guest_kernel_write(&img, &stat_script(fill, &paths));
    assert!(
        out.status.success(),
        "the kernel could not fill the volume:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert_agrees(&img, &stdout(&out), &paths, true);

    let link = stdout(&ok(tool("fs.ext4").args([&img, "stat", "/link"])));
    assert_eq!(json_field(&link, "type"), "symlink", "{link}");
    assert_eq!(json_field(&link, "target"), "file", "{link}");
    assert_eq!(json_field(&link, "size"), "4", "{link}");
}

#[test]
fn fs_ext4_stat_reports_null_for_times_a_128_byte_inode_cannot_hold() {
    let img = sized_image("stat-kernel-128", 32 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "128", &img])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));

    let fill = "head -c 100 /dev/urandom > file\nmkdir dir\n";
    let paths = ["/", "/file", "/dir"];
    let out = guest_kernel_write(&img, &stat_script(fill, &paths));
    assert!(
        out.status.success(),
        "the kernel could not fill the volume:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert_agrees(&img, &stdout(&out), &paths, false);
}
