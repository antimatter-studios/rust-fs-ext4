//! A path whose middle names a symbolic link, judged by THE KERNEL: a
//! volume made by `mkfs.ext4` in the harness VM and filled there by Linux
//! holds a directory, a symlink to it, a symlink to that symlink, and a
//! symlink inside the directory. A copy of the volume has names removed
//! through the links by the kernel's own `unlink` and `rmdir`, the original
//! has the same names removed by `fs.ext4`, and the kernel's mount must
//! show the two trees identical (type, links, inode number, content), the
//! link removed by its own name gone and its target kept (#531).
//!
//! The second test puts the loop bound to the kernel: a link to itself, two
//! links naming each other, and a chain one link longer than the kernel
//! follows are refused by the kernel and by `fs.ext4` for the same reason,
//! leaving the image byte-identical, and the chain the kernel does follow
//! resolves to the inode the kernel names.

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

fn listing(out: &std::process::Output) -> BTreeMap<String, String> {
    assert!(
        out.status.success(),
        "the kernel could not mount and list the volume:\n{}{}",
        stdout(out),
        stderr(out)
    );
    stdout(out)
        .lines()
        .map(|line| {
            let (name, rest) = line
                .split_once('|')
                .unwrap_or_else(|| panic!("a guest listing line: {line}"));
            (name.to_string(), rest.to_string())
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
fn a_symlink_mid_path_is_followed_as_the_kernel_follows_it() {
    let img = fresh_volume("path-symlink-kernel");
    let fill = "cd \"$MNT\"\n\
                mkdir full full/sub\n\
                head -c 5000 /dev/urandom > full/x\n\
                head -c 7000 /dev/urandom > full/y\n\
                ln -s y full/ylink\n\
                ln -s full dirlink\n\
                ln -s dirlink chain\n\
                sync\n";
    let filled = guest_kernel_write(&img, fill);
    assert!(
        filled.status.success(),
        "the kernel could not fill the volume:\n{}{}",
        stdout(&filled),
        stderr(&filled)
    );
    let kernel_img = format!("{img}.kernel");
    std::fs::copy(&img, &kernel_img).expect("copy the filled volume");

    // Each removal goes through at least one link mid-path; the last names
    // a symlink as its final component, which is removed, not followed.
    let removals = [
        ("rm", "/dirlink/x", "unlink dirlink/x"),
        ("rmdir", "/chain/sub", "rmdir chain/sub"),
        ("rm", "/chain/ylink", "unlink chain/ylink"),
    ];
    let mut script = String::from("set -e\ncd \"$MNT\"\n");
    for (_, _, command) in removals {
        script.push_str(command);
        script.push('\n');
    }
    script.push_str("sync\n");
    script.push_str(LISTING);
    let kernel = listing(&guest_kernel_write(&kernel_img, &script));

    for (verb, path, _) in removals {
        let report = stdout(&ok(tool("fs.ext4").args([&img, verb, path])));
        assert_eq!(json_field(&report, "path"), path, "{report}");
        assert_eq!(json_field(&report, "removed"), "true", "{report}");
    }
    assert_e2fsck_clean(&img, "the volume fs.ext4 removed names from through links");

    let ours = listing(&guest_kernel_write(
        &img,
        &format!("cd \"$MNT\"\n{LISTING}"),
    ));
    assert!(
        kernel.contains_key("/full/y") && !kernel.contains_key("/full/x"),
        "the kernel removed through the links: {kernel:?}"
    );
    assert_eq!(
        ours, kernel,
        "the tree after fs.ext4's removals through links (left) is not the tree after the \
         kernel's (right)"
    );
}

#[test]
fn a_symlink_loop_mid_path_is_refused_as_the_kernel_refuses_it() {
    let img = fresh_volume("path-symlink-loop-kernel");
    // l0 names `full`; each l<i> names l<i-1>, so `l<i>/x` follows i + 1
    // links. The kernel follows at most 40 in one lookup.
    let script = "cd \"$MNT\"\n\
                  mkdir full\n\
                  printf x > full/x\n\
                  ln -s self self\n\
                  ln -s b a\n\
                  ln -s a b\n\
                  ln -s full l0\n\
                  i=1\n\
                  while [ $i -le 40 ]; do ln -s l$((i - 1)) l$i; i=$((i + 1)); done\n\
                  sync\n\
                  printf 'inode|%s\\n' \"$(stat -c %i l39/x)\"\n\
                  for p in self/x a/x l40/x; do\n\
                  \x20   if out=\"$(LC_ALL=C unlink \"$p\" 2>&1)\"; then\n\
                  \x20       printf '%s|NOT REFUSED\\n' \"$p\"\n\
                  \x20   else\n\
                  \x20       printf '%s|%s\\n' \"$p\" \"${out##*: }\"\n\
                  \x20   fi\n\
                  done\n";
    let out = guest_kernel_write(&img, script);
    assert!(
        out.status.success(),
        "the kernel could not fill the volume or try the loops:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    let kernel = stdout(&out);
    let image = std::fs::read(&img).expect("read the image");

    let mut lines = kernel.lines();
    let deepest = lines
        .next()
        .and_then(|l| l.strip_prefix("inode|"))
        .unwrap_or_else(|| panic!("the kernel's inode for l39/x:\n{kernel}"));
    let report = stdout(&ok(tool("fs.ext4").args([&img, "stat", "/l39/x"])));
    assert_eq!(
        json_field(&report, "inode"),
        deepest,
        "40 links mid-path, which the kernel follows: {report}"
    );

    let mut tried = Vec::new();
    for line in lines {
        let (path, reason) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("a guest refusal line: {line}"));
        assert_eq!(
            reason, "Too many levels of symbolic links",
            "the kernel's refusal of {path}"
        );
        let path = format!("/{path}");
        let got = tool("fs.ext4")
            .args([&img, "rm", &path])
            .output()
            .expect("spawn fs.ext4");
        let err = stderr(&got);
        let what = format!("rm {path}: the kernel said {reason:?}; fs.ext4 said {err}");
        assert_eq!(got.status.code(), Some(1), "{what}");
        assert!(got.stdout.is_empty(), "{what}; stdout: {}", stdout(&got));
        assert!(err.contains("too many levels of symbolic links"), "{what}");
        assert!(
            std::fs::read(&img).expect("read the image") == image,
            "rm {path} was refused and still changed the image"
        );
        tried.push(path);
    }
    assert_eq!(
        tried,
        ["/self/x", "/a/x", "/l40/x"],
        "the guest answered every case:\n{kernel}"
    );
}
