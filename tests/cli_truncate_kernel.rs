//! `fs.ext4 truncate`, judged by THE KERNEL and by e2fsprogs: on a volume
//! made by `mkfs.ext4` in the harness VM and filled there by Linux, every
//! file `fs.ext4` truncates has a twin, written the same way, that the
//! guest's own `truncate` takes through the same lengths: to zero, across
//! a block boundary, to one, longer, shorter and then longer again, to the
//! length it has, and into a hole. `e2fsck -fn` must call the volume
//! clean, and the kernel's own mount must show each file with its twin's
//! size, its twin's allocated blocks and its twin's content: the bytes
//! before the new end, and zeros past an old one.
//!
//! The second test puts every refusal to the guest's `truncate` first, on
//! the kernel's mount, and `fs.ext4` must refuse the same paths for the
//! same reason, leaving the image byte-identical. The guest follows a
//! symlink and creates a missing file in a directory that is there;
//! `fs.ext4` does neither, so those are not compared, and
//! `tests/cli/test-write.sh` holds what it does with them.
//!
//! The third is the shape the driver cannot shrink: a block-mapped file,
//! on a volume made without extents. The shrink answers exit 3 and leaves
//! the image byte-identical; a grow, which only sets the new length, goes
//! through, and the kernel sees the file as it sees its twin.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle};
use std::collections::BTreeMap;

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
    fresh_volume_with_block_size(tag, 4096, features)
}

fn fresh_volume_with_block_size(tag: &str, block_size: u32, features: &[&str]) -> String {
    let img = sized_image(tag, 32 << 20);
    let block_size = block_size.to_string();
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", block_size.as_str(), "-I", "256"])
        .args(features)
        .arg(&img)
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));
    img
}

/// `fs.ext4 truncate PATH SIZE`, required to succeed and to report the
/// path and the size it was given, and nothing else.
#[track_caller]
fn truncate(img: &str, path: &str, size: u64) {
    let report = stdout(&ok(tool("fs.ext4").args([
        img,
        "truncate",
        path,
        &size.to_string(),
    ])));
    assert_eq!(json_field(&report, "path"), path, "{report}");
    assert_eq!(json_field(&report, "size"), size.to_string(), "{report}");
    assert_eq!(report.matches("\": ").count(), 2, "{report}");
}

/// How a file and its twin are first written in the guest, from the same
/// 20000 random bytes.
enum Made {
    /// The first `n` bytes.
    Head(u64),
    /// One block of them at the start and one 100 blocks in, a hole
    /// between.
    Holes,
}

#[test]
fn truncate_sets_lengths_the_kernel_then_sees() {
    let img = fresh_volume("truncate-kernel", &[]);
    // Each file, how it is written, and the lengths it is taken through.
    let cases: [(&str, Made, &[u64]); 7] = [
        ("zero", Made::Head(20000), &[0]),
        ("across", Made::Head(20000), &[5000]),
        ("edge", Made::Head(20000), &[8192]),
        ("grow", Made::Head(5000), &[1 << 20]),
        ("regrow", Made::Head(20000), &[4097, 12288]),
        ("same", Made::Head(5000), &[5000]),
        ("hole", Made::Holes, &[300000]),
    ];
    let mut fill = String::from(
        "cd \"$MNT\"\n\
         head -c 20000 /dev/urandom > src\n\
         holes() {\n\
         \x20   dd if=src of=\"$1\" bs=4096 count=1 status=none\n\
         \x20   dd if=src of=\"$1\" bs=4096 count=1 seek=100 conv=notrunc status=none\n\
         }\n",
    );
    for (name, made, _) in &cases {
        for file in [format!("ours-{name}"), format!("twin-{name}")] {
            fill.push_str(&match made {
                Made::Head(n) => format!("head -c {n} src > {file}\n"),
                Made::Holes => format!("holes {file}\n"),
            });
        }
    }
    fill.push_str("sync\n");
    for (name, _, lengths) in &cases {
        for size in *lengths {
            fill.push_str(&format!("truncate -s {size} twin-{name}\n"));
        }
    }
    fill.push_str(&format!("sync\n{LISTING}"));
    let before = listing(&guest_kernel_write(&img, &fill));
    assert_eq!(before["twin-hole"].size, 300000, "{before:?}");

    for (name, _, lengths) in &cases {
        for size in *lengths {
            truncate(&img, &format!("/ours-{name}"), *size);
        }
    }
    let dirty = stdout(&ok(tool("fs.ext4").args([&img, "get", "dirty", "--text"])));
    assert_eq!(dirty.trim(), "false", "truncate was not released clean");

    assert_e2fsck_clean(&img, "the volume fs.ext4 truncate left");

    let after = listing(&guest_kernel_write(
        &img,
        &format!("cd \"$MNT\"\n{LISTING}"),
    ));
    for (name, _, lengths) in &cases {
        let ours = format!("ours-{name}");
        let twin = format!("twin-{name}");
        let seen = after
            .get(&ours)
            .unwrap_or_else(|| panic!("the kernel does not see {ours}: {after:?}"));
        assert_eq!(
            seen.size,
            *lengths.last().unwrap(),
            "{ours} is not the length it was given"
        );
        assert_eq!(
            seen, &after[&twin],
            "the kernel sees {ours} (left) unlike the guest's {twin} (right)"
        );
    }
    // Nothing but the files fs.ext4 truncated changed.
    for (file, seen) in &before {
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

/// Our message for the kernel's reason: the text strerror gives the errno
/// the guest's `truncate` reported.
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
fn truncate_refuses_what_the_kernel_refuses() {
    let img = fresh_volume("truncate-kernel-refusals", &[]);
    let long = "n".repeat(256);
    // Each path, as fs.ext4 is given it; the guest is given it without the
    // leading `/`, from the root of its mount. Where two reasons apply the
    // kernel names the first name on the way that fails, so `/file/<long>`
    // is not a directory rather than a name too long. An immutable and an
    // append-only file cannot be truncated, even by root.
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
         \x20   if out=\"$(LC_ALL=C truncate -s 0 -- \"$1\" 2>&1)\"; then\n\
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
            .args([img.as_str(), "truncate", path.as_str(), "0"])
            .output()
            .expect("spawn fs.ext4");
        let err = stderr(&got);
        let label = &path[..path.len().min(40)];
        let what = format!("truncate {label:?}: the kernel said {reason:?}; fs.ext4 said {err}");
        assert_eq!(got.status.code(), Some(1), "{what}");
        assert!(got.stdout.is_empty(), "{what}; stdout: {}", stdout(&got));
        assert!(err.contains("\"code\": 1"), "{what}");
        assert!(err.contains(want), "{what}; expected {want:?}");
        assert!(
            std::fs::read(&img).expect("read the image") == image,
            "truncate {label:?} was refused and still changed the image"
        );
        tried.push(path);
    }
    assert_eq!(tried, cases, "the guest answered every case:\n{kernel}");
}

#[test]
fn truncate_of_a_block_mapped_file_grows_and_will_not_shrink() {
    let img = fresh_volume("truncate-kernel-blockmap", &["-O", "^extent,^64bit"]);
    let fill = format!(
        "cd \"$MNT\"\n\
         head -c 20000 /dev/urandom > src\n\
         head -c 20000 src > ours\n\
         head -c 20000 src > twin\n\
         sync\n\
         truncate -s 65536 twin\n\
         sync\n\
         {LISTING}"
    );
    let before = listing(&guest_kernel_write(&img, &fill));
    assert_eq!(before["twin"].size, 65536, "{before:?}");

    // The driver cannot shrink a block-mapped file: exit 3, nothing on
    // stdout, and the image as it was.
    let image = std::fs::read(&img).expect("read the image");
    let got = tool("fs.ext4")
        .args([img.as_str(), "truncate", "/ours", "5000"])
        .output()
        .expect("spawn fs.ext4");
    let err = stderr(&got);
    assert_eq!(got.status.code(), Some(3), "{err}");
    assert!(got.stdout.is_empty(), "stdout: {}", stdout(&got));
    assert!(err.contains("\"code\": 3"), "{err}");
    assert!(err.contains("block-mapped"), "{err}");
    assert!(
        std::fs::read(&img).expect("read the image") == image,
        "the refused shrink changed the image"
    );

    truncate(&img, "/ours", 65536);
    assert_e2fsck_clean(&img, "the volume fs.ext4 grew a block-mapped file on");
    let after = listing(&guest_kernel_write(
        &img,
        &format!("cd \"$MNT\"\n{LISTING}"),
    ));
    assert_eq!(
        after["ours"], after["twin"],
        "the kernel sees the grown block-mapped file (left) unlike its twin (right)"
    );
    assert_eq!(after["twin"], before["twin"], "the twin changed");
}

#[test]
fn truncate_refuses_sizes_past_the_mapping_limit_the_kernel_enforces() {
    // The largest length is measured, not assumed. Queue runs 39741203200
    // and 39777565017 showed the kernel refusing 2^32 blocks of 1 KiB on an
    // extent volume without huge_file, which the block-address arithmetic
    // alone allows. So the guest kernel is asked for every length by
    // bisection, and fs.ext4 must accept exactly the last length the kernel
    // accepted and refuse the next byte. Every layout is measured before
    // anything about the driver is asserted, so one run reports the table.
    let mut table = Vec::new();
    let mut wrong = Vec::new();
    for (block_size, extents, huge_file) in [
        (1024u32, true, false),
        (1024, true, true),
        (4096, true, false),
        (4096, true, true),
        (1024, false, false),
        (1024, false, true),
        (4096, false, false),
        (4096, false, true),
    ] {
        let tag = format!(
            "truncate-limit-{block_size}-{}-{}",
            if extents { "extent" } else { "blockmap" },
            if huge_file { "huge" } else { "ordinary" }
        );
        let map_feature = if extents {
            "extent,^64bit"
        } else {
            "^extent,^64bit"
        };
        let huge_feature = if huge_file {
            ",huge_file"
        } else {
            ",^huge_file"
        };
        let features = format!("{map_feature}{huge_feature}");
        let img = fresh_volume_with_block_size(&tag, block_size, &["-O", &features]);

        // Bisect the kernel's largest length on the tiny sparse volume
        // (2^50 bytes is past every layout here), then require the next
        // byte to be refused as too large.
        let kernel = guest_kernel_write(
            &img,
            "cd \"$MNT\"\n\
             : > kernel-limit\n\
             : > ours\n\
             lo=0; hi=$((1 << 50))\n\
             while [ $((hi - lo)) -gt 1 ]; do\n\
               mid=$((lo + (hi - lo) / 2))\n\
               if truncate -s $mid kernel-limit 2>/dev/null; then lo=$mid; else hi=$mid; fi\n\
             done\n\
             truncate -s $lo kernel-limit\n\
             if message=$(LC_ALL=C truncate -s $((lo + 1)) kernel-limit 2>&1); then\n\
               echo 'kernel accepted a length it refused while bisecting' >&2; exit 1;\n\
             fi\n\
             case \"$message\" in *'File too large'*) ;; *) echo \"$message\" >&2; exit 1 ;; esac\n\
             test \"$(stat -c %s kernel-limit)\" = $lo\n\
             echo \"kernel-limit=$lo\"\n",
        );
        assert!(
            kernel.status.success(),
            "kernel truncate boundary for {tag}: {}{}",
            stdout(&kernel),
            stderr(&kernel)
        );
        let max_size: u64 = stdout(&kernel)
            .lines()
            .find_map(|line| line.trim().strip_prefix("kernel-limit="))
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no kernel-limit line for {tag}: {}", stdout(&kernel)));
        assert_e2fsck_clean(&img, "the kernel's truncate-size boundary image");
        table.push(format!(
            "{tag}: {max_size} bytes, {} blocks and {} bytes",
            max_size / u64::from(block_size),
            max_size % u64::from(block_size)
        ));

        // The driver must accept the same exact boundary. Its refusal one
        // byte later is checked on the read-only mount, before opening the
        // image for writing, so the rejected request cannot change a byte.
        let max = max_size.to_string();
        let accepted = tool("fs.ext4")
            .args([img.as_str(), "truncate", "/ours", max.as_str()])
            .output()
            .expect("spawn fs.ext4 truncate");
        if !accepted.status.success() {
            wrong.push(format!(
                "{tag}: fs.ext4 refused the kernel's largest length: {}",
                stderr(&accepted).trim()
            ));
            continue;
        }
        assert_e2fsck_clean(&img, "the volume fs.ext4 grew to the mapping limit");
        let image = std::fs::read(&img).expect("read the image before the refusal");
        let too_large = (max_size + 1).to_string();
        let refused = tool("fs.ext4")
            .args([img.as_str(), "truncate", "/ours", too_large.as_str()])
            .output()
            .expect("spawn fs.ext4 truncate");
        if refused.status.code() != Some(1) || !stderr(&refused).contains("file too large") {
            wrong.push(format!(
                "{tag}: fs.ext4 did not refuse one byte past it as too large: {:?} {}",
                refused.status.code(),
                stderr(&refused).trim()
            ));
            continue;
        }
        assert!(refused.stdout.is_empty(), "stdout: {}", stdout(&refused));
        assert!(
            std::fs::read(&img).expect("read the image after the refusal") == image,
            "truncate one byte past the mapping limit changed the image"
        );
    }
    assert!(
        wrong.is_empty(),
        "the kernel's largest lengths:\n{}\nwhere fs.ext4 disagrees:\n{}",
        table.join("\n"),
        wrong.join("\n")
    );
}
