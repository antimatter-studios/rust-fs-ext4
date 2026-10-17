//! `fs.ext4 write --at`, judged by THE KERNEL and by e2fsprogs: on a
//! volume made by `mkfs.ext4` in the harness VM and filled there by
//! Linux, every file `fs.ext4` writes into at a position has a twin,
//! written the same way, into which the guest's own `dd conv=notrunc`
//! writes the same bytes at the same position: inside one block, across a
//! block boundary, whole blocks, across the end, at it, past it, into a
//! hole, into an empty file, and a MiB at once. `e2fsck -fn` must call the
//! volume clean, and the kernel's own mount must show each file with its
//! twin's size, its twin's allocated blocks and its twin's content.
//!
//! The second test puts every refusal to the guest's `dd` first, on the
//! kernel's mount, and `fs.ext4` must refuse the same paths for the same
//! reason, leaving the image byte-identical. The guest follows a symlink
//! and creates a missing file in a directory that is there; `fs.ext4`
//! does neither, so those are not compared, and `tests/cli/test-write.sh`
//! holds what it does with them.
//!
//! The third is the shape the driver cannot write into: a block-mapped
//! file, on a volume made without extents, answers exit 3 and leaves the
//! image byte-identical. The fourth writes into files the kernel keeps
//! inline, one write staying inside the inode and one past what it holds.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Output, Stdio};

/// Every regular file in the root of the kernel's mount, one line each:
/// `name|size|blocks|content`, the blocks being what `stat` reports
/// allocated and the content the file's SHA-256.
const LISTING: &str = r#"for f in *; do
    [ -f "$f" ] && [ ! -L "$f" ] || continue
    printf '%s|%s|%s\n' "$f" "$(stat -c '%s|%b' -- "$f")" \
        "$(sha256sum <"$f" | cut -d' ' -f1)"
done
"#;

/// A file's line from [`LISTING`]: size, allocated blocks, content.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    size: u64,
    blocks: u64,
    content: String,
}

fn listing(out: &Output) -> BTreeMap<String, Seen> {
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
            assert_eq!(f.len(), 4, "a guest listing line: {line}");
            let seen = Seen {
                size: f[1].parse().unwrap(),
                blocks: f[2].parse().unwrap(),
                content: f[3].to_string(),
            };
            (f[0].to_string(), seen)
        })
        .collect()
}

fn fresh_volume(tag: &str, features: &[&str]) -> String {
    let img = sized_image(tag, 32 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256"])
        .args(features)
        .arg(&img)
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));
    img
}

/// `fs.ext4 IMG write --at AT PATH` with `bytes` on stdin; the output.
fn write_at(img: &str, path: &str, at: u64, bytes: &[u8]) -> Output {
    let mut child = tool("fs.ext4")
        .args([img, "write", "--at", &at.to_string(), path])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fs.ext4 write --at");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(bytes)
        .expect("feed stdin");
    child
        .wait_with_output()
        .expect("wait for fs.ext4 write --at")
}

/// [`write_at`], required to succeed and to report the path, the
/// position, the count of bytes and the size, and nothing else.
#[track_caller]
fn written_at(img: &str, path: &str, at: u64, bytes: &[u8]) {
    let out = write_at(img, path, at, bytes);
    assert!(
        out.status.success(),
        "write --at {at} {path}: {}",
        stderr(&out)
    );
    let report = stdout(&out);
    assert_eq!(json_field(&report, "path"), path, "{report}");
    assert_eq!(json_field(&report, "at"), at.to_string(), "{report}");
    assert_eq!(
        json_field(&report, "bytes"),
        bytes.len().to_string(),
        "{report}"
    );
    assert_eq!(report.matches("\": ").count(), 4, "{report}");
}

/// Where the patches are cut from in the guest's `src`: past every byte
/// a file is first written from, so no write puts back what was there.
const PATCHES: usize = 1 << 20;

/// How a file and its twin are first written in the guest, from the same
/// random bytes.
enum Made {
    /// The first `n` bytes.
    Head(u64),
    /// One block of them at the start and one 100 blocks in, a hole
    /// between.
    Holes,
}

/// The guest's lines that write `name`'s two files the way `made` says.
fn made(name: &str, made: &Made) -> String {
    let mut lines = String::new();
    for file in [format!("ours-{name}"), format!("twin-{name}")] {
        lines.push_str(&match made {
            Made::Head(n) => format!("head -c {n} src > {file}\n"),
            Made::Holes => format!("holes {file}\n"),
        });
    }
    lines
}

/// The guest's line that writes `len` bytes of `src`, from [`PATCHES`] on,
/// into `file` at `at`, truncating nothing.
fn dd(file: &str, at: u64, len: usize) -> String {
    format!(
        "dd if=src of={file} bs=4096 iflag=skip_bytes,count_bytes oflag=seek_bytes \
         skip={PATCHES} count={len} seek={at} conv=notrunc status=none\n"
    )
}

/// Each `ours-` file in `after` is its twin, as the kernel sees both, and
/// nothing that is not an `ours-` file changed from `before`.
#[track_caller]
fn assert_twins(before: &BTreeMap<String, Seen>, after: &BTreeMap<String, Seen>, names: &[&str]) {
    for name in names {
        let ours = format!("ours-{name}");
        let twin = format!("twin-{name}");
        let seen = after
            .get(&ours)
            .unwrap_or_else(|| panic!("the kernel does not see {ours}: {after:?}"));
        assert_eq!(
            seen, &after[&twin],
            "the kernel sees {ours} (left) unlike the guest's {twin} (right)"
        );
    }
    for (file, seen) in before {
        if !file.starts_with("ours-") {
            assert_eq!(&after[file], seen, "{file} changed");
        }
    }
    assert_eq!(
        after.len(),
        before.len(),
        "the files changed: {before:?} became {after:?}"
    );
}

/// `src` as `fs.ext4` reads it from the image the guest filled.
fn source(img: &str) -> Vec<u8> {
    let src = ok(tool("fs.ext4").args([img, "read", "/src"])).stdout;
    assert_eq!(src.len(), 2 * PATCHES, "fs.ext4 read the guest's src short");
    src
}

#[test]
fn write_at_writes_what_dd_writes_where_the_kernel_then_sees_it() {
    let img = fresh_volume("pwrite-kernel", &[]);
    // Each file, how it is first written, and where the write goes and
    // how long it is.
    let cases: [(&str, Made, u64, usize); 9] = [
        ("inside", Made::Head(20000), 1000, 100),
        ("across", Made::Head(20000), 3000, 3000),
        ("blocks", Made::Head(20000), 4096, 8192),
        ("tail", Made::Head(20000), 18000, 5000),
        ("end", Made::Head(5000), 5000, 1000),
        ("past", Made::Head(5000), 50000, 100),
        ("hole", Made::Holes, 200000, 6000),
        ("empty", Made::Head(0), 0, 4097),
        ("large", Made::Head(20000), 10000, PATCHES),
    ];
    let mut fill = format!(
        "cd \"$MNT\"\n\
         head -c {} /dev/urandom > src\n\
         holes() {{\n\
         \x20   dd if=src of=\"$1\" bs=4096 count=1 status=none\n\
         \x20   dd if=src of=\"$1\" bs=4096 count=1 seek=100 conv=notrunc status=none\n\
         }}\n",
        2 * PATCHES
    );
    for (name, how, _, _) in &cases {
        fill.push_str(&made(name, how));
    }
    fill.push_str("sync\n");
    for (name, _, at, len) in &cases {
        fill.push_str(&dd(&format!("twin-{name}"), *at, *len));
    }
    fill.push_str(&format!("sync\n{LISTING}"));
    let before = listing(&guest_kernel_write(&img, &fill));
    assert_eq!(before["twin-hole"].size, 413696, "{before:?}");
    assert_eq!(before["twin-past"].size, 50100, "{before:?}");

    let src = source(&img);
    for (name, _, at, len) in &cases {
        written_at(
            &img,
            &format!("/ours-{name}"),
            *at,
            &src[PATCHES..PATCHES + len],
        );
    }
    let dirty = stdout(&ok(tool("fs.ext4").args([&img, "get", "dirty", "--text"])));
    assert_eq!(dirty.trim(), "false", "write --at was not released clean");

    assert_e2fsck_clean(&img, "the volume fs.ext4 wrote into at positions");

    let after = listing(&guest_kernel_write(
        &img,
        &format!("cd \"$MNT\"\n{LISTING}"),
    ));
    let names: Vec<&str> = cases.iter().map(|(name, ..)| *name).collect();
    assert_twins(&before, &after, &names);
}

/// Our message for the kernel's reason: the text strerror gives the errno
/// the guest's `dd` reported.
fn ours(reason: &str) -> &'static str {
    match reason {
        "Is a directory" => "is a directory",
        "Not a directory" => "not a directory",
        "File name too long" => "name too long",
        "No such file or directory" => "not found",
        "Operation not permitted" => "operation not permitted",
        "NOT REFUSED" => panic!("the kernel did not refuse it"),
        other => panic!("no message is mapped for the kernel's {other:?}"),
    }
}

#[test]
fn write_at_refuses_what_the_kernel_refuses() {
    let img = fresh_volume("pwrite-kernel-refusals", &[]);
    let long = "n".repeat(256);
    // Each path, as fs.ext4 is given it; the guest is given it without the
    // leading `/`, from the root of its mount. Where two reasons apply the
    // kernel names the first name on the way that fails, so `/file/<long>`
    // is not a directory rather than a name too long. An immutable and an
    // append-only file cannot be opened to write at a position, even by
    // root.
    let cases = [
        "/have".to_string(),
        "/have/".to_string(),
        "/have/..".to_string(),
        "/file/".to_string(),
        "/file/x".to_string(),
        "/have/file/x".to_string(),
        "/nodir/x".to_string(),
        format!("/{long}"),
        format!("/{long}/x"),
        format!("/have/{long}"),
        format!("/file/{long}"),
        "/locked".to_string(),
        "/appending".to_string(),
        String::new(),
    ];
    let mut script = String::from(
        "cd \"$MNT\"\n\
         mkdir have\n\
         printf x > file\n\
         printf y > have/file\n\
         printf z > locked\n\
         chattr +i locked\n\
         printf w > appending\n\
         chattr +a appending\n\
         sync\n\
         refuse() {\n\
         \x20   if out=\"$(printf v | LC_ALL=C dd of=\"$1\" conv=notrunc status=none 2>&1)\"; then\n\
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
        let got = write_at(&img, &path, 0, b"v");
        let err = stderr(&got);
        let label = &path[..path.len().min(40)];
        let what = format!("write --at {label:?}: the kernel said {reason:?}; fs.ext4 said {err}");
        assert_eq!(got.status.code(), Some(1), "{what}");
        assert!(got.stdout.is_empty(), "{what}; stdout: {}", stdout(&got));
        assert!(err.contains("\"code\": 1"), "{what}");
        assert!(err.contains(want), "{what}; expected {want:?}");
        assert!(
            std::fs::read(&img).expect("read the image") == image,
            "write --at {label:?} was refused and still changed the image"
        );
        tried.push(path);
    }
    assert_eq!(tried, cases, "the guest answered every case:\n{kernel}");
}

#[test]
fn write_at_into_a_block_mapped_file_is_refused() {
    let img = fresh_volume("pwrite-kernel-blockmap", &["-O", "^extent,^64bit"]);
    let fill = format!(
        "cd \"$MNT\"\n\
         head -c 20000 /dev/urandom > ours\n\
         sync\n\
         {LISTING}"
    );
    let before = listing(&guest_kernel_write(&img, &fill));
    assert_eq!(before["ours"].size, 20000, "{before:?}");

    // The driver cannot write into a block-mapped file, inside it or past
    // its end: exit 3, nothing on stdout, and the image as it was.
    let image = std::fs::read(&img).expect("read the image");
    for at in [100, 50000] {
        let got = write_at(&img, "/ours", at, &pattern(10, 9));
        let err = stderr(&got);
        assert_eq!(got.status.code(), Some(3), "at {at}: {err}");
        assert!(got.stdout.is_empty(), "at {at}: stdout: {}", stdout(&got));
        assert!(err.contains("\"code\": 3"), "at {at}: {err}");
        assert!(err.contains("block-mapped"), "at {at}: {err}");
        assert!(
            std::fs::read(&img).expect("read the image") == image,
            "the refused write at {at} changed the image"
        );
    }
}

#[test]
fn write_at_into_inline_data_matches_the_kernel() {
    let img = fresh_volume("pwrite-kernel-inline", &["-O", "inline_data"]);
    // Each file starts as 40 bytes the kernel keeps in the inode; one write
    // stays inside the inode, the other lands past anything it can hold.
    let cases: [(&str, Made, u64, usize); 2] = [
        ("stays", Made::Head(40), 10, 20),
        ("spills", Made::Head(40), 5000, 100),
    ];
    let mut fill = format!(
        "cd \"$MNT\"\n\
         head -c {} /dev/urandom > src\n",
        2 * PATCHES
    );
    for (name, how, _, _) in &cases {
        fill.push_str(&made(name, how));
    }
    fill.push_str("sync\n");
    for (name, _, at, len) in &cases {
        fill.push_str(&dd(&format!("twin-{name}"), *at, *len));
    }
    fill.push_str(&format!("sync\n{LISTING}"));
    let before = listing(&guest_kernel_write(&img, &fill));
    assert_eq!(
        before["ours-stays"].blocks, 0,
        "the kernel did not keep the file inline: {before:?}"
    );

    let src = source(&img);
    for (name, _, at, len) in &cases {
        written_at(
            &img,
            &format!("/ours-{name}"),
            *at,
            &src[PATCHES..PATCHES + len],
        );
    }
    assert_e2fsck_clean(&img, "the volume fs.ext4 wrote into inline files on");
    let after = listing(&guest_kernel_write(
        &img,
        &format!("cd \"$MNT\"\n{LISTING}"),
    ));
    let names: Vec<&str> = cases.iter().map(|(name, ..)| *name).collect();
    assert_twins(&before, &after, &names);
}
