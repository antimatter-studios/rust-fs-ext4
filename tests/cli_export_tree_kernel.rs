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
