//! `fs.ext4 mv`, judged by THE KERNEL and by e2fsprogs: a volume made by
//! `mkfs.ext4` in the harness VM and filled there by Linux has a file
//! renamed in place and then moved to another directory, a directory with a
//! tree in it moved to another parent, a file put over a file and a
//! directory over an empty directory with `--replace`, a symlink and one
//! name of a hard-linked file moved, all by `fs.ext4`. `e2fsck -fn` must
//! call it clean, which holds each moved directory's `..` to its new parent
//! and every parent's link count to its subdirectories. The kernel's own
//! mount must show each moved name at its new place only, with the type,
//! links, inode number and content it had before, every name that was not
//! moved as it was, and each parent with the links its subdirectories give
//! it.
//!
//! The second test puts every refusal to the kernel first: a small program
//! built in the guest makes `renameat2` on the kernel's mount, with
//! `RENAME_NOREPLACE` unless the case gives `--replace` (which is rename(2)),
//! and `fs.ext4` must refuse the same paths for the same reason, leaving the
//! image byte-identical. Where two reasons apply, the kernel's choice of one
//! is part of what is compared.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle};
use std::collections::BTreeMap;

/// Every path on the kernel's mount, the root included, one line each:
/// `path|type|links|inode|content`, the content being a regular file's
/// SHA-256 and a symlink's target.
const LISTING: &str = r#"find . | LC_ALL=C sort | while read -r p; do
    case "$(stat -c %F -- "$p")" in
        regular*) content="$(sha256sum <"$p" | cut -d' ' -f1)" ;;
        symbolic*) content="$(readlink -- "$p")" ;;
        *) content=- ;;
    esac
    q="${p#.}"
    printf '%s|%s|%s\n' "${q:-/}" "$(stat -c '%F|%h|%i' -- "$p")" "$content"
done
"#;

/// A path's line from [`LISTING`]: type, links, inode, content.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    kind: String,
    links: u64,
    inode: String,
    content: String,
}

fn listing(out: &std::process::Output) -> BTreeMap<String, Seen> {
    assert!(
        out.status.success(),
        "the kernel could not mount and list the volume:\n{}{}",
        stdout(out),
        stderr(out)
    );
    stdout(out)
        .lines()
        .map(|line| {
            let f: Vec<&str> = line.split('|').collect();
            assert_eq!(f.len(), 5, "a guest listing line: {line}");
            let seen = Seen {
                kind: f[1].to_string(),
                links: f[2].parse().unwrap(),
                inode: f[3].to_string(),
                content: f[4].to_string(),
            };
            (f[0].to_string(), seen)
        })
        .collect()
}

fn fresh_volume(tag: &str) -> String {
    let img = sized_image(tag, 32 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256", &img])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));
    img
}

/// `view` after a rename of `from` to `to`: whatever was at `to`, and under
/// it, is gone, and `from` and everything under it is filed under `to`.
fn rename_in(view: &mut BTreeMap<String, Seen>, from: &str, to: &str) {
    let under = |path: &str, top: &str| path == top || path.starts_with(&format!("{top}/"));
    view.retain(|path, _| !under(path, to));
    let moving: Vec<String> = view.keys().filter(|p| under(p, from)).cloned().collect();
    for path in moving {
        let seen = view.remove(&path).unwrap();
        view.insert(format!("{to}{}", &path[from.len()..]), seen);
    }
}

#[test]
fn mv_moves_names_the_kernel_then_sees_at_their_new_place() {
    let img = fresh_volume("mv-kernel");
    let fill = format!(
        "cd \"$MNT\"\n\
         mkdir a a/sub b empty moving\n\
         head -c 5000 /dev/urandom > f\n\
         head -c 6000 /dev/urandom > a/g\n\
         head -c 3000 /dev/urandom > a/sub/y\n\
         head -c 7000 /dev/urandom > old\n\
         head -c 4000 /dev/urandom > moving/inner\n\
         head -c 2000 /dev/urandom > keep\n\
         ln keep twin\n\
         ln -s a link\n\
         sync\n\
         {LISTING}"
    );
    let before = listing(&guest_kernel_write(&img, &fill));
    assert_eq!(before["/twin"], before["/keep"], "{before:?}");
    assert_eq!(
        before["/keep"].links, 2,
        "the kernel's hard link: {before:?}"
    );

    // Each move: the source, the new path, and whether it replaces a name.
    let moves = [
        ("/f", "/f2", false),
        ("/f2", "/b/f3", false),
        ("/a", "/b/a", false),
        ("/b/f3", "/old", true),
        ("/moving", "/empty", true),
        ("/link", "/b/link", false),
        ("/twin", "/b/twin", false),
    ];
    let mut expected = before.clone();
    for (from, to, replace) in moves {
        let mut mv = tool("fs.ext4");
        mv.args([&img, "mv"]);
        if replace {
            mv.arg("--replace");
        }
        let report = stdout(&ok(mv.args([from, to])));
        assert_eq!(json_field(&report, "path"), to, "{report}");
        assert_eq!(json_field(&report, "moved"), "true", "{report}");
        rename_in(&mut expected, from, to);
    }
    let dirty = stdout(&ok(tool("fs.ext4").args([&img, "get", "dirty", "--text"])));
    assert_eq!(dirty.trim(), "false", "the moves were not released clean");

    assert_e2fsck_clean(&img, "the volume fs.ext4 mv left");

    // The root lost `a` to `b` and `empty` to `moving`'s arrival over it.
    expected.get_mut("/").unwrap().links -= 2;
    expected.get_mut("/b").unwrap().links += 1;
    let after = listing(&guest_kernel_write(
        &img,
        &format!("cd \"$MNT\"\n{LISTING}"),
    ));
    assert_eq!(
        after, expected,
        "the kernel's view after the moves (left) is not the view before them with each name \
         at its new place (right)"
    );
    assert_eq!(after["/old"].inode, before["/f"].inode, "{after:?}");
    assert_eq!(after["/b/twin"].links, 2, "{after:?}");
}

/// Our message for the kernel's reason: the text strerror gives the errno
/// `renameat2` set in the guest.
fn ours(reason: &str) -> &'static str {
    match reason {
        "No such file or directory" => "not found",
        "Not a directory" => "not a directory",
        "File exists" => "already exists",
        "Is a directory" => "is a directory",
        "Directory not empty" => "directory not empty",
        "Invalid argument" => "invalid argument",
        "Device or resource busy" => "resource busy",
        "File name too long" => "name too long",
        "NOT REFUSED" => panic!("the kernel did not refuse it"),
        other => panic!("no message is mapped for the kernel's {other:?}"),
    }
}

/// `renameat2(AT_FDCWD, argv[2], AT_FDCWD, argv[3], flags)`, flags being
/// `RENAME_NOREPLACE` when argv[1] is `n` and none (rename(2)) when it is
/// `r`. It prints `OK`, or the text strerror gives the errno, and nothing
/// else: the system call's own answer, with no tool's checks in front of
/// it.
const RENAME_C: &str = r#"#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc != 4) {
        return 2;
    }
    unsigned flags = argv[1][0] == 'n' ? RENAME_NOREPLACE : 0;
    if (renameat2(AT_FDCWD, argv[2], AT_FDCWD, argv[3], flags) == 0) {
        puts("OK");
        return 0;
    }
    puts(strerror(errno));
    return 1;
}
"#;

#[test]
fn mv_refuses_what_the_kernel_refuses() {
    let img = fresh_volume("mv-kernel-refusals");
    let long = format!("/{}", "n".repeat(256));
    let long_in_d = format!("/d{long}");
    // Each case: whether `--replace` is given, then the source and the new
    // path as fs.ext4 is given them. The guest runs from the root of the
    // kernel's mount with the leading `/` taken off; the root itself is `.`
    // there, because naming the mount point would cross a mount, which the
    // kernel refuses for that reason instead.
    let cases: Vec<(bool, &str, &str)> = vec![
        (false, "/missing", "/x"),
        (false, "/f", "/missing/x"),
        (false, "/f/x", "/y"),
        (false, "/f", "/f/x"),
        (false, "/f/", "/y"),
        (false, "/f", "/y/"),
        (false, "/f", "/g"),
        (false, "/f", "/f"),
        (false, "/d", "/e"),
        (true, "/f", "/e"),
        (true, "/d", "/f"),
        (true, "/dl", "/d"),
        (true, "/d", "/dl"),
        (true, "/e", "/full"),
        (true, "/d/sub", "/d"),
        (false, "/d", "/d/sub/z"),
        (false, "/d", "/d/z"),
        (true, "/d", "/d/sub"),
        (false, "/d", "/d/sub"),
        (true, "/d", "/d/sub/y"),
        (false, "/d/.", "/z"),
        (false, "/d/..", "/z"),
        (true, "/d/sub/..", "/z"),
        (false, "/", "/z"),
        (true, "/f", "/d/."),
        (false, "/f", "/d/."),
        (true, "/f", "/d/sub/.."),
        (true, "/f", "/"),
        (false, "/f", "/"),
        (false, "/f", &long),
        // Where two reasons apply, the one the kernel gives.
        (false, "/missing", "/d/."),
        (true, "/missing", "/d/."),
        (false, "/missing", "/f/x"),
        (false, "/f/x", "/missing/y"),
        (false, "/d/.", "/missing/x"),
        (false, "/missing", &long),
        (false, "/d/.", &long),
        (false, "/d", &long_in_d),
        (true, "/f/", "/e"),
        (false, "/f", "/g/"),
        (true, "/f", "/g/"),
        (true, "/f/", &long),
        (false, "/missing/.", "/z"),
        (false, "/f/.", "/z"),
        (false, "/e/", "/e"),
        (false, "/dl/", "/z"),
        (true, "/e", "/dl/"),
    ];
    let guest_path = |p: &str| match p.trim_start_matches('/') {
        "" => ".".to_string(),
        rest => rest.to_string(),
    };
    let mut script = format!(
        "cd \"$MNT\"\n\
         mkdir d d/sub e full\n\
         printf f > f\n\
         printf g > g\n\
         printf x > full/x\n\
         printf y > d/sub/y\n\
         ln -s d dl\n\
         sync\n\
         build=\"$(mktemp -d)\"\n\
         cat > \"$build/rename.c\" <<'EOF'\n\
         {RENAME_C}\
         EOF\n\
         cc -o \"$build/rename\" \"$build/rename.c\"\n\
         refuse() {{\n\
         \x20   label=\"$1\"\n\
         \x20   shift\n\
         \x20   if out=\"$(\"$build/rename\" \"$@\")\"; then\n\
         \x20       printf '%s|NOT REFUSED\\n' \"$label\"\n\
         \x20   else\n\
         \x20       printf '%s|%s\\n' \"$label\" \"$out\"\n\
         \x20   fi\n\
         }}\n"
    );
    for (i, (replace, from, to)) in cases.iter().enumerate() {
        let flags = if *replace { "r" } else { "n" };
        script.push_str(&format!(
            "refuse {i} {flags} '{}' '{}'\n",
            guest_path(from),
            guest_path(to)
        ));
    }
    let out = guest_kernel_write(&img, &script);
    assert!(
        out.status.success(),
        "the kernel could not fill the volume or try the refusals:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    let kernel = stdout(&out);
    let image = std::fs::read(&img).expect("read the image");

    let mut tried = Vec::new();
    for line in kernel.lines() {
        let (index, reason) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("a guest refusal line: {line}"));
        let index: usize = index.parse().expect("a case number");
        let (replace, from, to) = cases[index];
        let want = ours(reason);
        let mut mv = tool("fs.ext4");
        mv.args([&img, "mv"]);
        if replace {
            mv.arg("--replace");
        }
        let got = mv.args([from, to]).output().expect("spawn fs.ext4");
        let err = stderr(&got);
        let what = format!(
            "mv{} {from} {to}: the kernel said {reason:?}; fs.ext4 said {err}",
            if replace { " --replace" } else { "" }
        );
        let code = refusal_code(reason);
        assert_eq!(got.status.code(), Some(code), "{what}");
        assert!(got.stdout.is_empty(), "{what}; stdout: {}", stdout(&got));
        assert!(err.contains(&format!("\"code\": {code}")), "{what}");
        assert!(err.contains(want), "{what}; expected {want:?}");
        assert!(
            std::fs::read(&img).expect("read the image") == image,
            "{what}: refused, and still changed the image"
        );
        tried.push(index);
    }
    let all: Vec<usize> = (0..cases.len()).collect();
    assert_eq!(tried, all, "the guest answered every case:\n{kernel}");
}
