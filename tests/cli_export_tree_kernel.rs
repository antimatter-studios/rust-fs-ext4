//! Recursive export of a tree the Linux kernel wrote in the harness VM,
//! more than 32 directories deep, every regular file digested by the
//! guest's `sha256sum` before the image comes back.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle, sha256_hex};
use std::path::Path;

/// Directories in the kernel-written chain below `/tree`.
const DEPTH: usize = 33;

fn kernel_tree(tag: &str) -> (String, Vec<(String, String)>) {
    let img = sized_image(tag, 64 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", &img])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));
    let out = guest_kernel_write(
        &img,
        &format!(
            "cd \"$MNT\"\n\
             mkdir tree\n\
             cd tree\n\
             : > empty\n\
             head -c 2097153 /dev/urandom > big\n\
             p=.\n\
             for i in $(seq 1 {DEPTH}); do\n\
               p=\"$p/d$i\"\n\
               mkdir \"$p\"\n\
               head -c $((i * 4099 + 1)) /dev/urandom > \"$p/f$i\"\n\
             done\n\
             mkfifo d1/pipe\n\
             sync\n\
             find . -type f -exec sha256sum {{}} +\n"
        ),
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_e2fsck_clean(&img, "the kernel-written tree");
    let sums: Vec<(String, String)> = stdout(&out)
        .lines()
        .filter_map(|l| l.split_once("  ./"))
        .filter(|(h, _)| h.len() == 64)
        .map(|(h, p)| (format!("/tree/{p}"), h.to_string()))
        .collect();
    assert_eq!(sums.len(), DEPTH + 2, "the guest digested every file");
    (img, sums)
}

/// The report's item for `path`: the text from its `"path"` key to the
/// next item's.
fn item<'a>(report: &'a str, path: &str) -> &'a str {
    let needle = format!("\"path\": \"{path}\"");
    let start = report
        .find(&needle)
        .unwrap_or_else(|| panic!("no item for {path} in:\n{report}"));
    let rest = &report[start + needle.len()..];
    &rest[..rest.find("\"path\": ").unwrap_or(rest.len())]
}

#[test]
fn recursive_export_matches_every_kernel_digest_and_reports_each_item() {
    let (img, sums) = kernel_tree("export-tree-kernel");
    let before = std::fs::read(&img).unwrap();
    let dest = image_path("export-tree");
    let _ = std::fs::remove_dir_all(&dest);

    // Without --recursive a directory keeps the single-file refusal.
    let plain = tool("fs.ext4")
        .args([&img, "export", "/tree", &dest])
        .output()
        .unwrap();
    assert_eq!(plain.status.code(), Some(1), "{}", stderr(&plain));
    assert!(!Path::new(&dest).exists());

    // The fifo is reported as its own item; every regular file still lands.
    let out = tool("fs.ext4")
        .args([&img, "export", "--recursive", "/tree", &dest])
        .output()
        .unwrap();
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
    assert_eq!(
        json_field(&report, "completed"),
        (sums.len() + DEPTH + 1).to_string()
    );
    assert_ne!(json_field(&report, "skipped"), "0", "{report}");
    assert!(!item(&report, "/tree/d1/pipe").contains("\"status\": \"completed\""));
    for (path, want) in &sums {
        let local = Path::new(&dest).join(path.strip_prefix("/tree/").unwrap());
        assert_eq!(sha256_hex(&std::fs::read(&local).unwrap()), *want, "{path}");
        let entry = item(&report, path);
        assert!(
            entry.contains("\"status\": \"completed\""),
            "{path}: {entry}"
        );
        assert!(entry.contains(want.as_str()), "{path}: {entry}");
    }
    let deepest = (1..=DEPTH)
        .map(|i| format!("d{i}"))
        .collect::<Vec<_>>()
        .join("/");
    assert!(Path::new(&dest).join(&deepest).is_dir());

    // An existing destination file is refused and left as it was.
    let kept = Path::new(&dest).join("big");
    std::fs::write(&kept, b"keep me").unwrap();
    let again = tool("fs.ext4")
        .args([&img, "export", "--recursive", "/tree", &dest])
        .output()
        .unwrap();
    let report = stdout(&again);
    assert_eq!(again.status.code(), Some(1), "{report}{}", stderr(&again));
    assert!(
        item(&report, "/tree/big").contains("\"status\": \"refused\""),
        "{report}"
    );
    assert_eq!(std::fs::read(&kept).unwrap(), b"keep me");
    assert_eq!(
        std::fs::read(&img).unwrap(),
        before,
        "export wrote the source"
    );
    std::fs::remove_dir_all(&dest).unwrap();
}

/// Access and modification seconds the guest's `touch -d` set, then read
/// back with `stat -c '%X %Y'`, beside the nanoseconds in `%x`/`%y`.
type GuestTimes = (i64, u32, i64, u32);

struct LinkTree {
    img: String,
    /// Path below `/tree`, guest inode number and guest link count.
    inodes: Vec<(String, u64, u64)>,
    /// Symbolic link name below `/tree` and the guest's `readlink`.
    targets: Vec<(String, String)>,
    /// Path below `/tree` and the times the guest recorded for it.
    times: Vec<(String, GuestTimes)>,
}

fn kernel_link_tree(tag: &str) -> LinkTree {
    let img = sized_image(tag, 64 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256", &img])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));
    let out = guest_kernel_write(
        &img,
        "cd \"$MNT\"\n\
         mkdir -p tree/sub tree/timed-dir\n\
         cd tree\n\
         head -c 70001 /dev/urandom > file\n\
         ln file hard\n\
         ln file sub/hard2\n\
         ln -s file fast\n\
         ln -s \"$(printf 'long/%.0s' $(seq 1 40))end\" slow\n\
         head -c 5000 /dev/urandom > timed-dir/timed\n\
         touch -a -d @981173106.123456789 timed-dir/timed\n\
         touch -m -d @1234567890.987654321 timed-dir/timed\n\
         touch -a -d @1000000000.5 timed-dir\n\
         touch -m -d @1100000000.25 timed-dir\n\
         sync\n\
         for f in file hard sub/hard2; do echo \"I $f $(stat -c '%i %h' $f)\"; done\n\
         for f in fast slow; do echo \"L $f $(readlink $f)\"; done\n\
         for f in timed-dir/timed timed-dir; do\n\
           echo \"T $f $(stat -c '%X %Y' $f) $(stat -c '%x' $f | cut -d. -f2 | cut -d' ' -f1) $(stat -c '%y' $f | cut -d. -f2 | cut -d' ' -f1)\"\n\
         done\n",
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_e2fsck_clean(&img, "the kernel-written link tree");
    let text = stdout(&out);
    let rows = |tag: &str| -> Vec<Vec<String>> {
        text.lines()
            .filter_map(|l| l.strip_prefix(tag))
            .map(|l| l.split(' ').map(str::to_string).collect())
            .collect()
    };
    let inodes: Vec<_> = rows("I ")
        .into_iter()
        .map(|r| (r[0].clone(), r[1].parse().unwrap(), r[2].parse().unwrap()))
        .collect();
    let targets: Vec<_> = rows("L ")
        .into_iter()
        .map(|r| (r[0].clone(), r[1].clone()))
        .collect();
    let nanos = |s: &str| s.parse::<u32>().unwrap();
    let times: Vec<_> = rows("T ")
        .into_iter()
        .map(|r| {
            (
                r[0].clone(),
                (
                    r[1].parse().unwrap(),
                    nanos(&r[3]),
                    r[2].parse().unwrap(),
                    nanos(&r[4]),
                ),
            )
        })
        .collect();
    assert_eq!(inodes.len(), 3, "{text}");
    assert_eq!(targets.len(), 2, "{text}");
    assert_eq!(times.len(), 2, "{text}");
    assert!(
        targets[0].1.len() < 60,
        "the fast symlink fits in the inode"
    );
    assert!(targets[1].1.len() > 60, "the slow symlink needs a block");
    LinkTree {
        img,
        inodes,
        targets,
        times,
    }
}

#[cfg(unix)]
#[test]
fn recursive_export_preserves_kernel_links_and_times() {
    use std::os::unix::fs::MetadataExt;
    let tree = kernel_link_tree("export-tree-links-kernel");
    let before = std::fs::read(&tree.img).unwrap();

    // Without --preserve each symlink is its own refused item, nothing is
    // created for it, and the hard links land as separate files.
    let plain = image_path("export-tree-links-plain");
    let _ = std::fs::remove_dir_all(&plain);
    let out = tool("fs.ext4")
        .args([&tree.img, "export", "--recursive", "/tree", &plain])
        .output()
        .unwrap();
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
    for (name, _) in &tree.targets {
        let entry = item(&report, &format!("/tree/{name}"));
        assert!(entry.contains("--preserve=links"), "{name}: {entry}");
        assert!(std::fs::symlink_metadata(Path::new(&plain).join(name)).is_err());
    }
    let ino = |root: &str, p: &str| std::fs::metadata(Path::new(root).join(p)).unwrap().ino();
    assert_ne!(ino(&plain, "file"), ino(&plain, "hard"));
    std::fs::remove_dir_all(&plain).unwrap();

    let dest = image_path("export-tree-links");
    let _ = std::fs::remove_dir_all(&dest);
    let out = tool("fs.ext4")
        .args([
            &tree.img,
            "export",
            "--recursive",
            "--preserve=links,times",
            "/tree",
            &dest,
        ])
        .output()
        .unwrap();
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{report}{}", stderr(&out));

    // Paths the guest gave one inode share one host inode, with the
    // guest's link count.
    let (first, guest_ino, nlink) = &tree.inodes[0];
    for (path, other_ino, other_nlink) in &tree.inodes {
        assert_eq!(other_ino, guest_ino, "the guest linked {path}");
        assert_eq!(other_nlink, nlink);
        let host = std::fs::metadata(Path::new(&dest).join(path)).unwrap();
        assert_eq!(host.ino(), ino(&dest, first), "{path} is not linked");
        assert_eq!(host.nlink(), *nlink, "{path}");
    }

    // Each symlink holds the guest's target, unfollowed.
    for (name, want) in &tree.targets {
        let local = Path::new(&dest).join(name);
        assert!(std::fs::symlink_metadata(&local)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(&local).unwrap(),
            Path::new(want),
            "{name}"
        );
    }

    // The file's and directory's times equal the guest's stat.
    for (path, (asec, ansec, msec, mnsec)) in &tree.times {
        let host = std::fs::metadata(Path::new(&dest).join(path)).unwrap();
        assert_eq!(
            (host.atime(), host.atime_nsec()),
            (*asec, i64::from(*ansec)),
            "{path} access time"
        );
        assert_eq!(
            (host.mtime(), host.mtime_nsec()),
            (*msec, i64::from(*mnsec)),
            "{path} modification time"
        );
    }
    assert_eq!(
        std::fs::read(&tree.img).unwrap(),
        before,
        "export wrote the source"
    );
    std::fs::remove_dir_all(&dest).unwrap();
}
