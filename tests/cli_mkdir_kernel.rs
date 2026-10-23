//! `fs.ext4 mkdir --parents`, judged by THE KERNEL and by e2fsprogs: a
//! volume made by `mkfs.ext4` in the harness VM and filled there by Linux,
//! a chain made by the guest's own `mkdir -p` among what it holds, has
//! chains made by `fs.ext4`: a new one from the root, one under a chain
//! already there, and one that `..` passes through. `e2fsck -fn` must call
//! it clean, and the kernel's own mount must show each directory made with
//! the type, mode and link count of its twin in the guest's chain, each
//! parent with one link more per directory made in it, and every other
//! name as it was. A chain that is already there is success, and the image
//! is not written at all.
//!
//! The second test puts every refusal to the guest's `mkdir -p` first, on
//! the kernel's mount, and `fs.ext4` must refuse the same paths for the
//! same reason, leaving the image byte-identical. Where the guest follows
//! a symlink on the way, `fs.ext4` follows none, so a symlink to a
//! directory and a dangling one with more path after it are not compared;
//! `tests/cli/test-write.sh` holds what `fs.ext4` does with them.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle};
use std::collections::BTreeMap;

/// Every path on the kernel's mount, the root included, one line each:
/// `path|type|links|inode|mode|content`, the content being a regular
/// file's SHA-256 and a symlink's target.
const LISTING: &str = r#"find . | LC_ALL=C sort | while read -r p; do
    case "$(stat -c %F -- "$p")" in
        regular*) content="$(sha256sum <"$p" | cut -d' ' -f1)" ;;
        symbolic*) content="$(readlink -- "$p")" ;;
        *) content=- ;;
    esac
    q="${p#.}"
    printf '%s|%s|%s\n' "${q:-/}" "$(stat -c '%F|%h|%i|%a' -- "$p")" "$content"
done
"#;

/// A path's line from [`LISTING`]: type, links, inode, mode, content.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    kind: String,
    links: u64,
    inode: String,
    mode: String,
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
            assert_eq!(f.len(), 6, "a guest listing line: {line}");
            let seen = Seen {
                kind: f[1].to_string(),
                links: f[2].parse().unwrap(),
                inode: f[3].to_string(),
                mode: f[4].to_string(),
                content: f[5].to_string(),
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

/// The strings of the report's `created` array, in order.
#[track_caller]
fn created(report: &str) -> Vec<String> {
    let needle = "\"created\": [";
    let start = report
        .find(needle)
        .unwrap_or_else(|| panic!("no created array in:\n{report}"))
        + needle.len();
    let end = start + report[start..].find(']').expect("the array closes");
    report[start..end]
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

#[test]
fn mkdir_parents_makes_a_chain_the_kernel_then_sees() {
    let img = fresh_volume("mkdir-p-kernel");
    let fill = format!(
        "cd \"$MNT\"\n\
         umask 022\n\
         mkdir -p have/deep twin/a/b/c\n\
         head -c 5000 /dev/urandom > have/file\n\
         ln -s have dirlink\n\
         sync\n\
         {LISTING}"
    );
    let before = listing(&guest_kernel_write(&img, &fill));
    assert_eq!(before["/twin/a/b/c"].links, 2, "{before:?}");
    assert_eq!(before["/twin/a"].links, 3, "{before:?}");

    // Each chain, the directories it must make, and the directory of the
    // guest's chain each one must look like.
    let chains: [(&str, &[(&str, &str)]); 3] = [
        (
            "/ours/a/b/c",
            &[
                ("/ours", "/twin"),
                ("/ours/a", "/twin/a"),
                ("/ours/a/b", "/twin/a/b"),
                ("/ours/a/b/c", "/twin/a/b/c"),
            ],
        ),
        (
            "/have/deep/x/y",
            &[
                ("/have/deep/x", "/twin/a/b"),
                ("/have/deep/x/y", "/twin/a/b/c"),
            ],
        ),
        (
            "/up/../down",
            &[("/up", "/twin/a/b/c"), ("/up/../down", "/twin/a/b/c")],
        ),
    ];
    for (path, made) in chains {
        let report = stdout(&ok(tool("fs.ext4").args([
            &img,
            "mkdir",
            "--parents",
            path,
        ])));
        assert_eq!(json_field(&report, "path"), path, "{report}");
        let names: Vec<&str> = made.iter().map(|(name, _)| *name).collect();
        assert_eq!(created(&report), names, "{report}");
    }

    // A chain already there, all the way: success, nothing made, and the
    // image not written.
    let image = std::fs::read(&img).expect("read the image");
    for path in ["/twin/a/b/c", "/have/deep/x/y", "/have/./deep/..", "/"] {
        let report = stdout(&ok(tool("fs.ext4").args([&img, "mkdir", "-p", path])));
        assert_eq!(json_field(&report, "path"), path, "{report}");
        assert!(created(&report).is_empty(), "{path}: {report}");
        assert!(
            std::fs::read(&img).expect("read the image") == image,
            "mkdir -p {path}, which was all there, wrote to the image"
        );
    }
    let dirty = stdout(&ok(tool("fs.ext4").args([&img, "get", "dirty", "--text"])));
    assert_eq!(dirty.trim(), "false", "mkdir -p was not released clean");

    assert_e2fsck_clean(&img, "the volume fs.ext4 mkdir --parents left");

    let after = listing(&guest_kernel_write(
        &img,
        &format!("cd \"$MNT\"\n{LISTING}"),
    ));
    // Every directory made is a directory the kernel sees as its twin in
    // the guest's chain: the same type, mode and links, and an inode of
    // its own. `/up/../down` is `/down` on the kernel's mount.
    let mut expected = before.clone();
    for (_, made) in chains {
        for (name, twin) in made {
            let shown = name.replace("/up/../", "/");
            let seen = after
                .get(&shown)
                .unwrap_or_else(|| panic!("the kernel does not see {shown}: {after:?}"));
            assert_eq!(
                (&seen.kind, seen.links, &seen.mode),
                (
                    &before[*twin].kind,
                    before[*twin].links,
                    &before[*twin].mode
                ),
                "{shown} (left) is not like the guest's {twin} (right)"
            );
            assert!(
                before.values().all(|b| b.inode != seen.inode),
                "{shown} reuses an inode that was in use: {seen:?}"
            );
            expected.insert(shown, seen.clone());
        }
    }
    let inodes: std::collections::BTreeSet<&str> =
        after.values().map(|s| s.inode.as_str()).collect();
    assert_eq!(
        inodes.len(),
        after.len(),
        "two paths share an inode: {after:?}"
    );
    // The root gained /ours, /up and /down; /have/deep gained x.
    expected.get_mut("/").unwrap().links += 3;
    expected.get_mut("/have/deep").unwrap().links += 1;
    assert_eq!(
        after, expected,
        "the kernel's view after mkdir --parents (left) is not the view before it plus the \
         directories made (right)"
    );
}

/// Our message for the kernel's reason: the text strerror gives the errno
/// the guest's `mkdir -p` reported.
fn ours(reason: &str) -> &'static str {
    match reason {
        "File exists" => "already exists",
        "Not a directory" => "not a directory",
        "File name too long" => "name too long",
        "No such file or directory" => "not found",
        "NOT REFUSED" => panic!("the kernel did not refuse it"),
        other => panic!("no message is mapped for the kernel's {other:?}"),
    }
}

#[test]
fn mkdir_parents_refuses_what_the_kernel_refuses() {
    let img = fresh_volume("mkdir-p-kernel-refusals");
    let long = "n".repeat(256);
    // Each path, as fs.ext4 is given it; the guest is given it without the
    // leading `/`, from the root of its mount. Where two reasons apply the
    // kernel names the first name on the way that fails, so `/file/<long>`
    // is not a directory rather than a name too long.
    let cases = [
        "/file".to_string(),
        "/file/".to_string(),
        "/file/x".to_string(),
        "/file/x/y".to_string(),
        "/file/.".to_string(),
        "/file/../z".to_string(),
        "/have/file/x".to_string(),
        "/filelink".to_string(),
        "/filelink/".to_string(),
        "/filelink/x".to_string(),
        "/dangle".to_string(),
        "/dangle/".to_string(),
        format!("/{long}"),
        format!("/{long}/x"),
        format!("/have/{long}"),
        format!("/file/{long}"),
        String::new(),
    ];
    let mut script = String::from(
        "cd \"$MNT\"\n\
         mkdir -p have/deep\n\
         printf x > file\n\
         printf y > have/file\n\
         ln -s file filelink\n\
         ln -s nowhere dangle\n\
         sync\n\
         refuse() {\n\
         \x20   if out=\"$(LC_ALL=C mkdir -p -- \"$1\" 2>&1)\"; then\n\
         \x20       printf '%s|NOT REFUSED\\n' \"$1\"\n\
         \x20   else\n\
         \x20       printf '%s|%s\\n' \"$1\" \"${out##*: }\"\n\
         \x20   fi\n\
         }\n",
    );
    for path in &cases {
        let relative = path.strip_prefix('/').unwrap_or(path);
        script.push_str(&format!("refuse '{relative}'\n"));
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
        let (relative, reason) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("a guest refusal line: {line}"));
        let want = ours(reason);
        let path = if relative.is_empty() {
            String::new()
        } else {
            format!("/{relative}")
        };
        let got = tool("fs.ext4")
            .args([img.as_str(), "mkdir", "-p", path.as_str()])
            .output()
            .expect("spawn fs.ext4");
        let err = stderr(&got);
        let label = &path[..path.len().min(40)];
        let what = format!("mkdir -p {label:?}: the kernel said {reason:?}; fs.ext4 said {err}");
        let code = refusal_code(reason);
        assert_eq!(got.status.code(), Some(code), "{what}");
        assert!(got.stdout.is_empty(), "{what}; stdout: {}", stdout(&got));
        assert!(err.contains(&format!("\"code\": {code}")), "{what}");
        assert!(err.contains(want), "{what}; expected {want:?}");
        assert!(
            std::fs::read(&img).expect("read the image") == image,
            "mkdir -p {label:?} was refused and still changed the image"
        );
        tried.push(path);
    }
    assert_eq!(tried, cases, "the guest answered every case:\n{kernel}");
}
