//! `fs.ext4 rm` and `rmdir`, judged by THE KERNEL and by e2fsprogs: a
//! volume made by `mkfs.ext4` in the harness VM and filled there by Linux
//! has a file, one name of a hard-linked file, a symlink to a directory and
//! two empty directories removed by `fs.ext4`. `e2fsck -fn` must call it
//! clean, and the kernel's own mount must show every other name as it did
//! before (type, links, inode number, content) and none of the removed
//! ones, the hard-linked file's other name with one link left and each
//! parent of a removed directory with one link fewer.
//!
//! The second test puts every refusal to the kernel first: the guest's
//! `unlink` and `rmdir` commands, which make the system calls of those
//! names, try each one on the kernel's mount, and `fs.ext4` must refuse
//! the same paths for the same reason, leaving the image byte-identical.

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

#[test]
fn rm_and_rmdir_remove_names_the_kernel_then_does_not_see() {
    let img = fresh_volume("rm-kernel");
    let fill = format!(
        "cd \"$MNT\"\n\
         mkdir full full/sub empty\n\
         head -c 5000 /dev/urandom > full/inner\n\
         head -c 9000 /dev/urandom > gone\n\
         head -c 7000 /dev/urandom > kept\n\
         ln kept twin\n\
         ln -s full dirlink\n\
         printf x > small\n\
         sync\n\
         {LISTING}"
    );
    let before = listing(&guest_kernel_write(&img, &fill));
    assert_eq!(
        before["/kept"].links, 2,
        "the kernel's hard link: {before:?}"
    );
    assert_eq!(before["/twin"], before["/kept"], "{before:?}");

    let removals = [
        ("rm", "/gone"),
        ("rm", "/twin"),
        ("rm", "/dirlink"),
        ("rmdir", "/empty"),
        ("rmdir", "/full/sub"),
    ];
    for (verb, path) in removals {
        let report = stdout(&ok(tool("fs.ext4").args([&img, verb, path])));
        assert_eq!(json_field(&report, "path"), path, "{report}");
        assert_eq!(json_field(&report, "removed"), "true", "{report}");
    }
    let dirty = stdout(&ok(tool("fs.ext4").args([&img, "get", "dirty", "--text"])));
    assert_eq!(
        dirty.trim(),
        "false",
        "the removals were not released clean"
    );

    assert_e2fsck_clean(&img, "the volume fs.ext4 rm and rmdir left");

    let mut expected = before.clone();
    for (_, path) in removals {
        expected
            .remove(path)
            .expect("a removed path was listed before");
    }
    expected.get_mut("/kept").unwrap().links = 1;
    expected.get_mut("/").unwrap().links -= 1;
    expected.get_mut("/full").unwrap().links -= 1;
    let after = listing(&guest_kernel_write(
        &img,
        &format!("cd \"$MNT\"\n{LISTING}"),
    ));
    assert_eq!(
        after, expected,
        "the kernel's view after the removals (left) is not the view before them less the \
         removed names (right)"
    );
}

/// Our message for the kernel's reason: the text strerror gives the errno
/// the guest's command reported.
fn ours(reason: &str) -> &'static str {
    match reason {
        "No such file or directory" => "not found",
        "Is a directory" => "is a directory",
        "Not a directory" => "not a directory",
        "Directory not empty" => "directory not empty",
        "Invalid argument" => "invalid argument",
        "Device or resource busy" => "the root directory cannot be removed",
        "NOT REFUSED" => panic!("the kernel did not refuse it"),
        other => panic!("no message is mapped for the kernel's {other:?}"),
    }
}

#[test]
fn rm_and_rmdir_refuse_what_the_kernel_refuses() {
    let img = fresh_volume("rm-kernel-refusals");
    // Each case: our verb and path, then the guest command that makes the
    // same system call on the kernel's mount. `rmdir "$MNT"` is the root.
    let cases = [
        ("rm /missing", "unlink missing"),
        ("rmdir /missing", "rmdir missing"),
        ("rm /missing/x", "unlink missing/x"),
        ("rm /full", "unlink full"),
        ("rmdir /file", "rmdir file"),
        ("rmdir /dirlink", "rmdir dirlink"),
        ("rm /file/", "unlink file/"),
        ("rm /file/x", "unlink file/x"),
        ("rmdir /full", "rmdir full"),
        ("rmdir /full/..", "rmdir full/.."),
        ("rmdir /full/.", "rmdir full/."),
        ("rmdir /", "rmdir \"$MNT\""),
    ];
    let mut script = String::from(
        "cd \"$MNT\"\n\
         mkdir full\n\
         printf x > full/inner\n\
         printf y > file\n\
         ln -s full dirlink\n\
         sync\n\
         refuse() {\n\
         \x20   label=\"$1\"\n\
         \x20   shift\n\
         \x20   if out=\"$(LC_ALL=C \"$@\" 2>&1)\"; then\n\
         \x20       printf '%s|NOT REFUSED\\n' \"$label\"\n\
         \x20   else\n\
         \x20       printf '%s|%s\\n' \"$label\" \"${out##*: }\"\n\
         \x20   fi\n\
         }\n",
    );
    for (label, command) in cases {
        script.push_str(&format!("refuse '{label}' {command}\n"));
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
        let (label, reason) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("a guest refusal line: {line}"));
        let want = ours(reason);
        let (verb, path) = label.split_once(' ').unwrap();
        let got = tool("fs.ext4")
            .args([&img, verb, path])
            .output()
            .expect("spawn fs.ext4");
        let err = stderr(&got);
        let what = format!("{label}: the kernel said {reason:?}; fs.ext4 said {err}");
        assert_eq!(got.status.code(), Some(1), "{what}");
        assert!(got.stdout.is_empty(), "{what}; stdout: {}", stdout(&got));
        assert!(err.contains("\"code\": 1"), "{what}");
        assert!(err.contains(want), "{what}; expected {want:?}");
        assert!(
            std::fs::read(&img).expect("read the image") == image,
            "{label} was refused and still changed the image"
        );
        tried.push(label.to_string());
    }
    let labels: Vec<String> = cases.iter().map(|(l, _)| l.to_string()).collect();
    assert_eq!(tried, labels, "the guest answered every case:\n{kernel}");
}
