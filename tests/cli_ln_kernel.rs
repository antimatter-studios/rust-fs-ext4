//! `fs.ext4 ln`, judged by THE KERNEL and by e2fsprogs: a volume made by
//! `mkfs.ext4` in the harness VM and filled there by Linux is given hard
//! links (to a file twice, to a file in another directory, to a symlink)
//! and symbolic links (relative, absolute, dangling, fast, slow and the
//! longest target a 4 KiB block holds), all by `fs.ext4`. `e2fsck -fn` must
//! call it clean, which holds each inode's link count to the names that
//! reach it. The kernel's own mount must show every new name with the type,
//! inode, links and content it should have: a hard link the inode and bytes
//! of the name it was made from, with every name of that inode one link up,
//! and a symlink a new inode whose `readlink` is the text given. Every name
//! that was there before is as it was. A 1 KiB-block volume takes the
//! longest target its block holds, the same way.
//!
//! The second test puts every refusal to the kernel first: a small program
//! built in the guest makes `linkat` (no flags) or `symlinkat` on the
//! kernel's mount, and `fs.ext4` must refuse the same paths for the same
//! reason, leaving the image byte-identical. Where two reasons apply, the
//! kernel's choice of one is part of what is compared.

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

fn fresh_volume(tag: &str, block_size: u32) -> String {
    let img = sized_image(tag, 32 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", &block_size.to_string(), "-I", "256", &img])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));
    img
}

/// `fs.ext4 ln` (`-s` when `symbolic`) of `target` at `path`, required to
/// succeed and report `path`, `linked` and an inode, which it returns.
fn ln(img: &str, symbolic: bool, target: &str, path: &str) -> String {
    let mut ln = tool("fs.ext4");
    ln.args([img, "ln"]);
    if symbolic {
        ln.arg("-s");
    }
    let report = stdout(&ok(ln.args([target, path])));
    assert_eq!(json_field(&report, "path"), path, "{report}");
    assert_eq!(json_field(&report, "linked"), "true", "{report}");
    json_field(&report, "inode")
}

#[test]
fn ln_makes_links_the_kernel_then_sees() {
    let img = fresh_volume("ln-kernel", 4096);
    let fill = format!(
        "cd \"$MNT\"\n\
         mkdir a\n\
         head -c 5000 /dev/urandom > f\n\
         head -c 6000 /dev/urandom > a/g\n\
         ln -s f s\n\
         sync\n\
         {LISTING}"
    );
    let before = listing(&guest_kernel_write(&img, &fill));
    assert_eq!(before["/f"].links, 1, "{before:?}");

    let mut expected = before.clone();
    // Hard links: the new name is the inode the old one names, and every
    // name of that inode has one link more.
    for (from, to) in [
        ("/f", "/a/f2"),
        ("/f", "/f3"),
        ("/a/g", "/g2"),
        ("/s", "/a/s2"),
    ] {
        let inode = ln(&img, false, from, to);
        let mut seen = expected[from].clone();
        assert_eq!(inode, seen.inode, "ln {from} {to} reported another inode");
        seen.links += 1;
        for other in expected.values_mut().filter(|s| s.inode == seen.inode) {
            other.links = seen.links;
        }
        expected.insert(to.to_string(), seen);
    }
    // Symbolic links: a new inode each, one link, the text as given.
    let slow = format!("../{}", "x".repeat(197));
    let longest = "a".repeat(4095);
    for (text, at) in [
        ("f", "/rel"),
        ("/a/g", "/abs"),
        ("nowhere", "/dangle"),
        (slow.as_str(), "/a/slow"),
        (longest.as_str(), "/longest"),
    ] {
        let inode = ln(&img, true, text, at);
        assert!(
            expected.values().all(|s| s.inode != inode),
            "ln -s {at} reported inode {inode}, which another name already has"
        );
        let seen = Seen {
            kind: "symbolic link".to_string(),
            links: 1,
            inode,
            content: text.to_string(),
        };
        expected.insert(at.to_string(), seen);
    }
    let dirty = stdout(&ok(tool("fs.ext4").args([&img, "get", "dirty", "--text"])));
    assert_eq!(dirty.trim(), "false", "the links were not released clean");

    assert_e2fsck_clean(&img, "the volume fs.ext4 ln left");

    let after = listing(&guest_kernel_write(
        &img,
        &format!("cd \"$MNT\"\n{LISTING}"),
    ));
    assert_eq!(
        after, expected,
        "the kernel's view after the links (left) is not the view before them with each new \
         name added (right)"
    );
    assert_eq!(after["/a/f2"].links, 3, "{after:?}");
    assert_eq!(after["/a/s2"].content, "f", "{after:?}");

    // A 1 KiB block holds a target of 1023 bytes.
    let small = fresh_volume("ln-kernel-1k", 1024);
    let most = "b".repeat(1023);
    let inode = ln(&small, true, &most, "/most");
    assert_e2fsck_clean(&small, "the 1 KiB-block volume fs.ext4 ln -s left");
    let seen = listing(&guest_kernel_write(
        &small,
        &format!("cd \"$MNT\"\n{LISTING}"),
    ));
    assert_eq!(
        seen["/most"],
        Seen {
            kind: "symbolic link".to_string(),
            links: 1,
            inode,
            content: most,
        },
        "the kernel's view of a 1023-byte target on a 1 KiB-block volume"
    );
}

/// Our message for the kernel's reason: the text strerror gives the errno
/// `linkat` or `symlinkat` set in the guest.
fn ours(reason: &str) -> &'static str {
    match reason {
        "No such file or directory" => "not found",
        "Not a directory" => "not a directory",
        "File exists" => "already exists",
        "Operation not permitted" => "not permitted",
        "Too many links" => "too many links",
        "File name too long" => "name too long",
        "NOT REFUSED" => panic!("the kernel did not refuse it"),
        other => panic!("no message is mapped for the kernel's {other:?}"),
    }
}

/// `linkat(AT_FDCWD, argv[2], AT_FDCWD, argv[3], 0)` when argv[1] is `l`,
/// `symlinkat(argv[2], AT_FDCWD, argv[3])` when it is `s`. It prints `OK`,
/// or the text strerror gives the errno, and nothing else: the system
/// call's own answer, with no tool's checks in front of it. `m` makes
/// argv[4] more links to argv[2] in directory argv[3], to bring a file to
/// the most links it can have.
const LN_C: &str = r#"#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc < 4) {
        return 2;
    }
    if (argv[1][0] == 'm') {
        long n = atol(argv[4]);
        char name[64];
        for (long i = 0; i < n; i++) {
            snprintf(name, sizeof name, "%s/l%ld", argv[3], i);
            if (linkat(AT_FDCWD, argv[2], AT_FDCWD, name, 0) != 0) {
                printf("link %ld: %s\n", i, strerror(errno));
                return 1;
            }
        }
        return 0;
    }
    int r = argv[1][0] == 's' ? symlinkat(argv[2], AT_FDCWD, argv[3])
                              : linkat(AT_FDCWD, argv[2], AT_FDCWD, argv[3], 0);
    if (r == 0) {
        puts("OK");
        return 0;
    }
    puts(strerror(errno));
    return 1;
}
"#;

/// One refusal: whether the link is symbolic, then its target and path as
/// `fs.ext4` is given them.
type Case = (bool, String, String);

fn hard(target: &str, path: &str) -> Case {
    (false, target.to_string(), path.to_string())
}

fn symbolic(text: &str, path: &str) -> Case {
    (true, text.to_string(), path.to_string())
}

/// Fill `img` with `setup` on the kernel's mount, put each case to the
/// kernel there, then require `fs.ext4 ln` to refuse it for the reason
/// the kernel gave, leaving the image byte-identical.
fn refused_as_the_kernel_refuses(img: &str, setup: &str, cases: &[Case]) {
    // The guest runs from the root of the kernel's mount with the leading
    // `/` taken off; the root itself is `.` there, because naming the mount
    // point would leave the volume. A symlink's text is never a path to
    // translate: it is handed over as given.
    let guest_path = |p: &str| match p.trim_start_matches('/') {
        "" => ".".to_string(),
        rest => rest.to_string(),
    };
    let mut script = format!(
        "cd \"$MNT\"\n\
         {setup}\
         build=\"$(mktemp -d)\"\n\
         cat > \"$build/ln.c\" <<'EOF'\n\
         {LN_C}\
         EOF\n\
         cc -o \"$build/ln\" \"$build/ln.c\"\n\
         refuse() {{\n\
         \x20   label=\"$1\"\n\
         \x20   shift\n\
         \x20   if out=\"$(\"$build/ln\" \"$@\")\"; then\n\
         \x20       printf '%s|NOT REFUSED\\n' \"$label\"\n\
         \x20   else\n\
         \x20       printf '%s|%s\\n' \"$label\" \"$out\"\n\
         \x20   fi\n\
         }}\n"
    );
    if setup.contains("many") {
        script.push_str("\"$build/ln\" m many m 64999\nsync\n");
    }
    for (i, (is_symbolic, target, path)) in cases.iter().enumerate() {
        let (call, target) = if *is_symbolic {
            ("s", target.clone())
        } else {
            ("l", guest_path(target))
        };
        script.push_str(&format!(
            "refuse {i} {call} '{target}' '{}'\n",
            guest_path(path)
        ));
    }
    let out = guest_kernel_write(img, &script);
    assert!(
        out.status.success(),
        "the kernel could not fill the volume or try the refusals:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    let kernel = stdout(&out);
    let image = std::fs::read(img).expect("read the image");

    let mut tried = Vec::new();
    for line in kernel.lines() {
        let (index, reason) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("a guest refusal line: {line}"));
        let index: usize = index.parse().expect("a case number");
        let (is_symbolic, target, path) = &cases[index];
        let want = ours(reason);
        let mut ln = tool("fs.ext4");
        ln.args([img, "ln"]);
        if *is_symbolic {
            ln.arg("-s");
        }
        let got = ln.args([target, path]).output().expect("spawn fs.ext4");
        let err = stderr(&got);
        let shown = |s: &str| s.chars().take(40).collect::<String>();
        let what = format!(
            "ln{} {} {}: the kernel said {reason:?}; fs.ext4 said {err}",
            if *is_symbolic { " -s" } else { "" },
            shown(target),
            shown(path),
        );
        assert_eq!(got.status.code(), Some(1), "{what}");
        assert!(got.stdout.is_empty(), "{what}; stdout: {}", stdout(&got));
        assert!(err.contains("\"code\": 1"), "{what}");
        assert!(err.contains(want), "{what}; expected {want:?}");
        assert!(
            std::fs::read(img).expect("read the image") == image,
            "{what}: refused, and still changed the image"
        );
        tried.push(index);
    }
    let all: Vec<usize> = (0..cases.len()).collect();
    assert_eq!(tried, all, "the guest answered every case:\n{kernel}");
}

#[test]
fn ln_refuses_what_the_kernel_refuses() {
    let long = format!("/{}", "n".repeat(256));
    let long_slash = format!("{long}/");
    let missing_long = format!("/missing{long}");
    let file_long = format!("/f{long}");
    let past_path_max = "a".repeat(4096);
    let past_block = "a".repeat(1024);
    let in_block = "a".repeat(4095);

    // Not compared, because the CLI never follows a symlink and the guest
    // cannot stay inside the volume: a hard link to `dl/` or `dangle/`,
    // which the kernel follows to a directory or to nothing; and `..` of
    // the root, which in the guest leaves the mount (a cross-device link)
    // and in a volume of its own is the root.
    let cases = vec![
        // A hard link.
        hard("/missing", "/x"),
        hard("/missing/a", "/x"),
        hard("/f/x", "/y"),
        hard("/f", "/missing/x"),
        hard("/f", "/f/x"),
        hard("/f", "/g"),
        hard("/f", "/e"),
        hard("/f", "/dangle"),
        hard("/f", "/dl"),
        hard("/d", "/x"),
        hard("/", "/x"),
        hard("/d/.", "/x"),
        hard("/d/..", "/x"),
        hard("/d/sub/..", "/x"),
        hard("/f/", "/y"),
        hard("/fl/", "/y"),
        hard("/d/", "/y"),
        hard("/f", "/y/"),
        hard("/f", "/d/."),
        hard("/f", "/d/.."),
        hard("/f", "/"),
        hard("/f", "/g/"),
        hard("/f", "/e/"),
        hard("/f", "/d/sub/"),
        hard("/f", &long),
        hard("/many", "/x"),
        // Where two reasons apply, the one the kernel gives.
        hard("/missing", "/d/."),
        hard("/missing", "/g"),
        hard("/missing", &long),
        hard("/missing", "/missing/x"),
        hard("/missing/x", "/missing/y"),
        hard("/f/x", "/g"),
        hard("/f/", "/missing/x"),
        hard("/f/", &long),
        hard("/f", "/missing/x/"),
        hard("/f", &missing_long),
        hard("/f", &file_long),
        hard("/f", &long_slash),
        hard("/d", "/missing/x"),
        hard("/d", "/f/x"),
        hard("/d", "/d/."),
        hard("/d", "/g"),
        hard("/d", &long),
        hard("/d", "/y/"),
        hard("/many", "/g"),
        hard("/many", &long),
        hard("/many", "/y/"),
        hard("/many", "/missing/x"),
        // A symbolic link.
        symbolic("t", "/g"),
        symbolic("t", "/e"),
        symbolic("t", "/dangle"),
        symbolic("t", "/missing/x"),
        symbolic("t", "/f/x"),
        symbolic("t", "/y/"),
        symbolic("t", "/g/"),
        symbolic("t", "/e/"),
        symbolic("t", "/dl/"),
        symbolic("t", "/dangle/"),
        symbolic("t", "/d/sub/"),
        symbolic("t", "/"),
        symbolic("t", "/d/."),
        symbolic("t", "/d/.."),
        symbolic("t", &long),
        symbolic("", "/x"),
        symbolic(&past_path_max, "/x"),
        // Where two reasons apply, the one the kernel gives.
        symbolic("", "/g"),
        symbolic("", "/missing/x"),
        symbolic("", &long),
        symbolic("t", &missing_long),
        symbolic("t", &file_long),
        symbolic("t", &long_slash),
        symbolic("t", "/g/x"),
        symbolic(&past_path_max, "/g"),
        symbolic(&past_path_max, "/missing/x"),
        symbolic(&past_path_max, "/y/"),
        symbolic(&in_block, "/g"),
        symbolic(&in_block, "/missing/x"),
        symbolic(&in_block, &long),
    ];
    refused_as_the_kernel_refuses(
        &fresh_volume("ln-kernel-refusals", 4096),
        "mkdir d d/sub e m\n\
         printf f > f\n\
         printf g > g\n\
         printf m > many\n\
         ln -s d dl\n\
         ln -s f fl\n\
         ln -s missing dangle\n\
         sync\n",
        &cases,
    );

    // A 1 KiB block holds a target of 1023 bytes: a longer one is refused,
    // after the reasons a path gives, and before the 4 KiB a path may have.
    let cases = vec![
        symbolic(&past_block, "/x"),
        symbolic(&in_block, "/x"),
        symbolic(&past_block, "/g"),
        symbolic(&past_block, "/missing/x"),
        symbolic(&past_block, "/y/"),
        symbolic(&past_block, &long),
        symbolic(&past_path_max, "/g"),
    ];
    refused_as_the_kernel_refuses(
        &fresh_volume("ln-kernel-refusals-1k", 1024),
        "printf g > g\nsync\n",
        &cases,
    );
}
