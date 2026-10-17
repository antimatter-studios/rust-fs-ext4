//! `fs.ext4 ls --recursive /` walks the tree THE KERNEL walks: a volume
//! made by `mkfs.ext4` (e2fsprogs) in the harness VM, filled there by
//! Linux, and walked there by GNU `find` over the kernel's own mount. The
//! paths, types, sizes and inode numbers must be the ones `find` printed,
//! in depth-first order with each directory's names sorted as bytes, with
//! a symlink to a directory listed and not entered.
//!
//! The same volume, damaged, must still end: a directory linked twice and
//! a directory linked inside itself (both made by `debugfs ln`, which the
//! kernel refuses to do) are listed once more as `revisited` and not
//! entered again, and a directory whose contents cannot be found stops the
//! walk with status 1, the entries before it still on stdout.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{guest_kernel_write, oracle};
use std::collections::BTreeMap;

/// The guest's `find` type letter, as `ls` names the type.
fn kind(letter: &[u8]) -> &'static str {
    match letter {
        b"d" => "dir",
        b"f" => "file",
        b"l" => "symlink",
        b"p" => "fifo",
        b"c" => "char",
        b"b" => "block",
        b"s" => "socket",
        other => panic!("unexpected find type {other:?}"),
    }
}

/// One recursive `ls` entry, cut out of the pretty JSON report.
struct Entry {
    path: Vec<u8>,
    depth: usize,
    kind: String,
    size: u64,
    inode: u64,
    revisited: bool,
}

/// The entries of a recursive report, in the order it printed them. A path
/// that is not UTF-8 is read back from its exact bytes in `path_hex`.
fn entries(report: &str) -> Vec<Entry> {
    report
        .split("\n  {")
        .skip(1)
        .map(|row| {
            let path = if row.contains("\"path_hex\"") {
                let hex = json_field(row, "path_hex");
                (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                    .collect()
            } else {
                json_field(row, "path").into_bytes()
            };
            Entry {
                path,
                depth: json_field(row, "depth").parse().unwrap(),
                kind: json_field(row, "type"),
                size: json_field(row, "size").parse().unwrap(),
                inode: json_field(row, "inode").parse().unwrap(),
                revisited: row.contains("\"revisited\": true"),
            }
        })
        .collect()
}

fn components(path: &[u8]) -> Vec<&[u8]> {
    path.split(|b| *b == b'/')
        .filter(|c| !c.is_empty())
        .collect()
}

#[test]
fn ls_recursive_walks_what_the_kernel_walks_and_ends_on_damaged_graphs() {
    let img = sized_image("ls-recursive-kernel", 32 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256", &img])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));

    // A symlink to a directory, one to an ancestor, one to nothing, a hard
    // link, an empty directory, a FIFO, a name with a space and a name that
    // is not UTF-8. Then the kernel's own walk, NUL-separated so no name
    // can be mistaken for a separator.
    let script = r#"cd "$MNT"
mkdir -p a/deep empty z
printf payload > a/deep/file
printf raw > $'a/deep/raw-\xff'
printf space > 'two words'
ln a/deep/file hard
ln -s a dirlink
ln -s ../.. a/deep/up
ln -s absent dangling
mkfifo fifo
printf zz > z/inner
sync
find . -mindepth 1 -printf '/%P\0%y\0%s\0%i\0'
"#;
    let out = guest_kernel_write(&img, script);
    assert!(
        out.status.success(),
        "the kernel could not fill or walk the volume:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    let fields: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
    assert_eq!(fields.last(), Some(&&b""[..]), "find's output ends in NUL");
    assert_eq!((fields.len() - 1) % 4, 0, "find printed four fields a path");
    let number = |bytes: &[u8]| std::str::from_utf8(bytes).unwrap().parse::<u64>().unwrap();
    let mut expected = BTreeMap::new();
    for row in fields[..fields.len() - 1].chunks_exact(4) {
        let value = (kind(row[1]).to_string(), number(row[2]), number(row[3]));
        assert!(expected.insert(row[0].to_vec(), value).is_none());
    }
    assert_eq!(
        expected.len(),
        14,
        "the kernel's walk must cover the whole tree: {:?}",
        expected.keys().map(|p| show(p)).collect::<Vec<_>>()
    );

    let report = stdout(&ok(tool("fs.ext4").args([&img, "ls", "--recursive", "/"])));
    let listed = entries(&report);
    let mut actual = BTreeMap::new();
    for e in &listed {
        assert_eq!(
            e.depth,
            components(&e.path).len(),
            "{}: depth is the number of steps from /",
            show(&e.path)
        );
        assert!(!e.revisited, "{}: nothing is revisited", show(&e.path));
        let value = (e.kind.clone(), e.size, e.inode);
        assert!(
            actual.insert(e.path.clone(), value).is_none(),
            "{} listed twice",
            show(&e.path)
        );
    }
    assert_eq!(
        actual, expected,
        "ls --recursive / and the kernel's find disagree"
    );

    // Depth first, each directory's names sorted as bytes: the order of
    // the paths' component lists.
    let order: Vec<&[u8]> = listed.iter().map(|e| e.path.as_slice()).collect();
    let mut sorted = order.clone();
    sorted.sort_by(|a, b| components(a).cmp(&components(b)));
    assert_eq!(
        order.iter().map(|p| show(p)).collect::<Vec<_>>(),
        sorted.iter().map(|p| show(p)).collect::<Vec<_>>(),
        "the walk is depth first with names sorted as bytes"
    );
    assert!(
        order.iter().all(|p| !p.starts_with(b"/dirlink/")),
        "a symlink to a directory is listed, never entered"
    );
    let again = stdout(&ok(tool("fs.ext4").args([&img, "ls", "-R", "/"])));
    assert_eq!(report, again, "two runs print the same thing");

    // Damage. Each copy is walked with bounds the damage alone could never
    // reach, so a walk that loops fails on a bound rather than hanging.
    let bounded = |image: &str| {
        tool("fs.ext4")
            .args([image, "ls", "-R", "--limit", "60", "--max-depth", "12", "/"])
            .output()
            .unwrap()
    };
    let a = expected[&b"/a".to_vec()].2 as u32;
    for (fault, request, alias) in [
        ("twice", "ln /a /z/alias", "/z/alias"),
        ("cycle", "ln / /a/deep/root", "/a/deep/root"),
    ] {
        let damaged = image_path(&format!("ls-recursive-{fault}"));
        std::fs::copy(&img, &damaged).unwrap();
        oracle("debugfs")
            .args(["-w", "-R", request, &damaged])
            .output();
        let before = std::fs::read(&damaged).unwrap();
        let out = bounded(&damaged);
        assert!(
            out.status.success(),
            "{fault}: a revisited directory ends the walk cleanly: {}",
            stderr(&out)
        );
        let listed = entries(&stdout(&out));
        let alias_entry = listed
            .iter()
            .find(|e| e.path == alias.as_bytes())
            .unwrap_or_else(|| panic!("{fault}: {alias} is listed:\n{}", stdout(&out)));
        assert!(
            alias_entry.revisited,
            "{fault}: {alias} is marked revisited"
        );
        assert_eq!(alias_entry.kind, "dir", "{fault}: {alias}");
        assert_eq!(
            listed.iter().filter(|e| e.revisited).count(),
            1,
            "{fault}: only {alias} is revisited"
        );
        let inside = format!("{alias}/");
        assert!(
            listed
                .iter()
                .all(|e| !e.path.starts_with(inside.as_bytes())),
            "{fault}: {alias} is not entered again"
        );
        assert_eq!(
            listed.len(),
            expected.len() + 1,
            "{fault}: everything else is listed once"
        );
        assert_eq!(
            std::fs::read(&damaged).unwrap(),
            before,
            "{fault}: read-only"
        );
    }

    // A directory whose contents cannot be found: listed, then the walk
    // stops on it, keeping what came before.
    let unreadable = image_path("ls-recursive-unreadable");
    std::fs::copy(&img, &unreadable).unwrap();
    destroy_extent_header(&unreadable, a);
    let before = std::fs::read(&unreadable).unwrap();
    let out = bounded(&unreadable);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(json_field(&stderr(&out), "code"), "1", "{}", stderr(&out));
    assert!(
        stderr(&out).contains("/a"),
        "the error names /a: {}",
        stderr(&out)
    );
    let kept = entries(&stdout(&out));
    assert_eq!(
        kept.iter().map(|e| show(&e.path)).collect::<Vec<_>>(),
        ["/a"],
        "the entries before the stop stay on stdout"
    );
    assert_eq!(std::fs::read(&unreadable).unwrap(), before, "read-only");
}

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
