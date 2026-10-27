//! Recursive import of a local tree more than 32 directories deep, every
//! name and byte read back by the independent Linux kernel in the harness
//! VM, every image judged by e2fsck.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle, sha256_hex};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Directories in the local chain below the source root.
const DEPTH: usize = 33;

/// A modification time the guest prints back as `%Y`.
const MTIME: u64 = 1_000_000_000;

fn fresh(tag: &str) -> String {
    let img = sized_image(tag, 64 << 20);
    let out = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", &img])
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

/// A local tree: `empty`, `big` (2 MiB + 1) and a chain `d1/.../d33`, each
/// directory holding one file of its own size. Returns each regular file's
/// path relative to the root with its SHA-256.
fn local_tree(root: &Path) -> Vec<(String, String)> {
    let mut sums = Vec::new();
    let mut write = |rel: &str, bytes: Vec<u8>| {
        std::fs::write(root.join(rel), &bytes).unwrap();
        sums.push((rel.to_string(), sha256_hex(&bytes)));
    };
    write("empty", Vec::new());
    write("big", pattern((2 << 20) + 1, 7));
    let mut rel = String::new();
    for i in 1..=DEPTH {
        rel = if rel.is_empty() {
            format!("d{i}")
        } else {
            format!("{rel}/d{i}")
        };
        std::fs::create_dir(root.join(&rel)).unwrap();
        write(&format!("{rel}/f{i}"), pattern(i * 4099 + 1, i as u32));
    }
    sums
}

/// The guest's `sha256sum` of every regular file below `$MNT/<dir>`, as
/// (relative path, digest), sorted.
fn kernel_sums(listing: &str) -> Vec<(String, String)> {
    let mut sums: Vec<(String, String)> = listing
        .lines()
        .filter_map(|l| l.split_once("  ./"))
        .filter(|(h, _)| h.len() == 64)
        .map(|(h, p)| (p.to_string(), h.to_string()))
        .collect();
    sums.sort();
    sums
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

#[test]
fn recursive_import_matches_every_kernel_digest_with_links_and_times() {
    let img = fresh("import-tree");
    let src = source_dir("import-tree-src");
    let mut want = local_tree(&src);
    // A second name for `big`, and a symlink to it, both preserved.
    std::fs::hard_link(src.join("big"), src.join("big.link")).unwrap();
    want.push(("big.link".into(), want[1].1.clone()));
    want.sort();
    symlink("big", src.join("sym")).unwrap();
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(MTIME);
    std::fs::File::options()
        .write(true)
        .open(src.join("d1/f1"))
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let src = src.to_str().unwrap();

    // Without --recursive a directory source is refused and nothing lands.
    let before = std::fs::read(&img).unwrap();
    let plain = tool("fs.ext4")
        .args([&img, "import", src, "/tree"])
        .output()
        .unwrap();
    assert_ne!(plain.status.code(), Some(0), "{}", stderr(&plain));
    assert_eq!(std::fs::read(&img).unwrap(), before);

    let report = stdout(&ok(tool("fs.ext4").args([
        &img,
        "import",
        "--recursive",
        "--preserve=links,times",
        src,
        "/tree",
    ])));
    // Every file, every directory (the root included) and the symlink.
    assert_eq!(
        json_field(&report, "completed"),
        (want.len() + DEPTH + 2).to_string(),
        "{report}"
    );
    for (rel, sum) in &want {
        let entry = item(&report, &format!("/tree/{rel}"));
        assert!(
            entry.contains("\"status\": \"completed\""),
            "{rel}: {entry}"
        );
        // The hard link is a second name, not a second copy.
        if rel != "big.link" {
            assert!(entry.contains(sum.as_str()), "{rel}: {entry}");
        }
    }

    let seen = guest_kernel_write(
        &img,
        "cd \"$MNT/tree\"\n\
         find . -type f -exec sha256sum {} +\n\
         echo dirs $(find . -type d | wc -l)\n\
         echo sym $(readlink sym)\n\
         echo links $(stat -c '%i:%h' big) $(stat -c '%i:%h' big.link)\n\
         echo mtime $(stat -c %Y d1/f1)\n\
         echo staging $(find \"$MNT\" -name '.dj-import-*' | wc -l)\n",
    );
    assert!(seen.status.success(), "{}", stderr(&seen));
    let listing = stdout(&seen);
    assert_eq!(kernel_sums(&listing), want, "{listing}");
    let line = |key: &str| {
        listing
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("no {key} in:\n{listing}"))
            .to_string()
    };
    assert_eq!(line("dirs"), (DEPTH + 1).to_string());
    assert_eq!(line("sym"), "big");
    let links = line("links");
    let links: Vec<&str> = links.split(' ').collect();
    assert_eq!(links[0], links[1], "big.link is a hard link to big");
    assert!(links[0].ends_with(":2"), "{links:?}");
    assert_eq!(line("mtime"), MTIME.to_string());
    assert_eq!(line("staging"), "0");
    assert_e2fsck_clean(&img, "recursive import with links and times");
    std::fs::remove_dir_all(src).unwrap();
}

#[test]
fn recursive_import_reports_each_failure_and_refusal_and_keeps_going() {
    let img = fresh("import-tree-refusals");
    let out = guest_kernel_write(
        &img,
        "mkdir -p \"$MNT/tree/sub\"\nprintf old > \"$MNT/tree/keep\"\n",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let src = source_dir("import-tree-refusals-src");
    std::fs::write(src.join("keep"), b"new").unwrap();
    std::fs::create_dir(src.join("sub")).unwrap();
    let a = pattern(5000, 3);
    let b = pattern(70_000, 4);
    std::fs::write(src.join("sub/a"), &a).unwrap();
    std::fs::write(src.join("b"), &b).unwrap();
    symlink("b", src.join("sym")).unwrap();
    let fifo = src.join("pipe");
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    let src = src.to_str().unwrap();

    // An existing file fails on its own; links without --preserve=links and
    // special files are refused; existing directories merge; the rest lands.
    let out = tool("fs.ext4")
        .args([&img, "import", "--recursive", src, "/tree"])
        .output()
        .unwrap();
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
    assert_eq!(json_field(&report, "merged_directories"), "2", "{report}");
    assert!(item(&report, "/tree/keep").contains("\"status\": \"failed\""));
    assert!(item(&report, "/tree/sym").contains("\"status\": \"refused\""));
    assert!(item(&report, "/tree/pipe").contains("\"status\": \"refused\""));
    for done in ["/tree/b", "/tree/sub/a"] {
        assert!(
            item(&report, done).contains("\"status\": \"completed\""),
            "{done}: {report}"
        );
    }
    let seen = guest_kernel_write(
        &img,
        "cd \"$MNT/tree\"\ncat keep; echo\nsha256sum b sub/a\n\
         ls -A\nfind \"$MNT\" -name '.dj-import-*'\n",
    );
    assert!(seen.status.success(), "{}", stderr(&seen));
    let listing = stdout(&seen);
    assert!(listing.starts_with("old\n"), "{listing}");
    assert!(
        listing.contains(&format!("{}  b\n", sha256_hex(&b))),
        "{listing}"
    );
    assert!(
        listing.contains(&format!("{}  sub/a\n", sha256_hex(&a))),
        "{listing}"
    );
    assert!(
        !listing.contains("sym") && !listing.contains("pipe"),
        "{listing}"
    );
    assert!(!listing.contains(".dj-import-"), "{listing}");

    // Sparse preservation is refused per file and publishes nothing.
    let out = tool("fs.ext4")
        .args([
            &img,
            "import",
            "--recursive",
            "--preserve=sparse",
            &format!("{src}/sub"),
            "/sparse",
        ])
        .output()
        .unwrap();
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
    let refused = item(&report, "/sparse/a");
    assert!(refused.contains("\"status\": \"refused\""), "{report}");
    assert!(refused.contains("sparse"), "{report}");

    // --replace swaps the existing file in one journaled rename.
    let out = tool("fs.ext4")
        .args([&img, "import", "--recursive", "--replace", src, "/tree"])
        .output()
        .unwrap();
    let report = stdout(&out);
    assert!(
        item(&report, "/tree/keep").contains("\"replaced\": true"),
        "{report}"
    );
    let seen = guest_kernel_write(
        &img,
        "cat \"$MNT/tree/keep\"; echo\nls -A \"$MNT/sparse\"\n\
         find \"$MNT\" -name '.dj-import-*'\n",
    );
    assert!(seen.status.success(), "{}", stderr(&seen));
    assert_eq!(stdout(&seen), "new\n");
    assert_e2fsck_clean(&img, "recursive import failures and refusals");
    std::fs::remove_dir_all(src).unwrap();
}
