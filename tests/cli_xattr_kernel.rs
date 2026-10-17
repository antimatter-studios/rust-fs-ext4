//! `fs.ext4 xattr` lists and reads what THE KERNEL set: a volume made by
//! `mkfs.ext4` (e2fsprogs) in the harness VM with 256-byte inodes, EA
//! inodes and inline data, filled there by Linux with `setfattr`, and every
//! path put to the guest's own `getfattr` -- the whole set with
//! `getfattr -d -m -`, then each value by name with `getfattr -n`. Names
//! and sizes must be the ones the kernel listed and every value the bytes
//! it returned, across the user, trusted and security namespaces, binary
//! and empty values, attributes kept in the inode, in an external block
//! and in an EA inode, and a symlink's own attributes, never its
//! target's.
//!
//! The one name the kernel does not list is `system.data`, the tail of an
//! inline file or directory, which the format stores as an attribute and
//! `fs.ext4 xattr` reports like any other. It must be there on exactly the
//! inodes the guest's `lsattr` marks inline (`N`), holding the bytes past
//! the first 60 (the ext4 documentation, "Inline Data").

mod cli_support;

use std::collections::BTreeMap;
use std::sync::Arc;

use cli_support::*;
use fs_ext4::block_io::FileDevice;
use fs_ext4::{xattr, Filesystem};
use fs_ext4_test_support::{guest_kernel_write, oracle};

/// The tree, filled by Linux: an empty file, a small inline file with one
/// attribute, a 5000-byte file whose attributes overflow the inode into an
/// external block and an EA inode, a directory with one of its own, and a
/// symlink with a trusted attribute its target does not have.
const FILL: &str = r#"cd "$MNT"
hex() { head -c "$1" /dev/urandom | od -An -v -tx1 | tr -d ' \n'; }
: > none
printf hi > small
setfattr -n user.a -v 0x0001ff small
head -c 5000 /dev/urandom > many
setfattr -n user.comment -v 'hello world' many
setfattr -n user.bin -v 0x00ff0a00ff many
setfattr -n user.empty -v '""' many
setfattr -n trusted.audit -v 0x7472757374656400ff many
setfattr -n security.capability -v 0x0100000200040000000000000000000000000000 many
setfattr -n user.medium -v "0x$(hex 1500)" many
setfattr -n user.huge -v "0x$(hex 8000)" many
mkdir dir
setfattr -n user.dir -v dirvalue dir
ln -s many link
setfattr -h -n trusted.link -v 0x6c696e6b link
sync
"#;

const PATHS: [&str; 6] = ["/", "/none", "/small", "/many", "/dir", "/link"];

/// Per path: `@|PATH|LSATTR-FLAGS|SIZE`, then `D|` and each line of
/// `getfattr -d -m - -e hex`, then `V|` and each value `getfattr -n`
/// returned for every name `getfattr -m -` listed. Never through a
/// symlink (`-h`), and a symlink has no `lsattr` flags (`-`).
fn report_script() -> String {
    let mut script = String::from(FILL);
    for path in PATHS {
        script.push_str(&format!(
            r#"rel=".{path}"
size="$(stat -c %s "$rel")"
if [ -L "$rel" ]; then flags=-; else flags="$(lsattr -d "$rel" | cut -d' ' -f1)"; fi
printf '@|%s|%s|%s\n' {path} "$flags" "$size"
getfattr -h -d -m - -e hex --absolute-names "$rel" | awk 'NF && !/^#/ {{ print "D|" $0 }}'
for name in $(getfattr -h -m - --absolute-names "$rel" | awk 'NF && !/^#/'); do
    getfattr -h -n "$name" -e hex --absolute-names "$rel" | awk 'NF && !/^#/ {{ print "V|" $0 }}'
done
"#
        ));
    }
    script
}

/// What the guest said about one path.
#[derive(Default)]
struct Seen {
    inline: bool,
    size: u64,
    dump: BTreeMap<String, Vec<u8>>,
    each: BTreeMap<String, Vec<u8>>,
}

/// One `NAME=VALUE` line of `getfattr -e hex`: an empty value is printed
/// with no `=`, with nothing after it, or as `""`; any other is `0x` hex.
fn attribute(line: &str) -> (String, Vec<u8>) {
    let (name, value) = line.split_once('=').unwrap_or((line, ""));
    let value = match value {
        "" | "\"\"" => Vec::new(),
        hex => hex::decode(
            hex.strip_prefix("0x")
                .unwrap_or_else(|| panic!("a getfattr -e hex value: {line}")),
        )
        .unwrap_or_else(|e| panic!("{line}: {e}")),
    };
    (name.to_string(), value)
}

fn parse(report: &str) -> Vec<(String, Seen)> {
    let mut seen: Vec<(String, Seen)> = Vec::new();
    for line in report.lines() {
        let (kind, rest) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("a report line: {line}"));
        if kind == "@" {
            let f: Vec<&str> = rest.split('|').collect();
            assert_eq!(f.len(), 3, "a path line: {line}");
            let seen_now = Seen {
                inline: f[1].contains('N'),
                size: f[2].parse().unwrap(),
                ..Seen::default()
            };
            seen.push((f[0].to_string(), seen_now));
            continue;
        }
        let (_, current) = seen
            .last_mut()
            .unwrap_or_else(|| panic!("an attribute before any path: {line}"));
        let (name, value) = attribute(rest);
        let into = match kind {
            "D" => &mut current.dump,
            "V" => &mut current.each,
            _ => panic!("a report line: {line}"),
        };
        assert!(into.insert(name, value).is_none(), "twice: {line}");
    }
    seen
}

/// The JSON `fs.ext4 xattr PATH` prints for these names and sizes.
fn listing(sizes: &BTreeMap<String, usize>) -> String {
    if sizes.is_empty() {
        return "[]".to_string();
    }
    let items: Vec<String> = sizes
        .iter()
        .map(|(name, size)| format!("  {{\n    \"name\": \"{name}\",\n    \"size\": {size}\n  }}"))
        .collect();
    format!("[\n{}\n]", items.join(",\n"))
}

/// The attributes `/many` holds sit in all three places the format keeps
/// them: the inode, the external block and an EA inode. Read with this
/// crate's parser, which reports where an entry is without following it,
/// so the comparison with the kernel covers each.
fn assert_every_place_is_used(img: &str) {
    let fs = Filesystem::mount(Arc::new(FileDevice::open(img).expect("open"))).expect("mount");
    let ino = fs.lookup_path_bytes(b"/many").expect("/many");
    let (inode, raw) = fs.read_inode_verified(ino).expect("read /many");
    let dev = FileDevice::open(img).expect("open");
    let (inode_size, block_size) = (fs.sb.inode_size, fs.sb.block_size());
    let all = xattr::read_all(&dev, &inode, &raw, inode_size, block_size).expect("parse");
    assert_ne!(inode.file_acl, 0, "/many has no external attribute block");
    assert!(
        all.iter().any(|e| e.value_inum != 0),
        "no attribute of /many is in an EA inode: {all:?}"
    );
    let mut inode_only = inode.clone();
    inode_only.file_acl = 0;
    let in_inode = xattr::read_all(&dev, &inode_only, &raw, inode_size, block_size).unwrap();
    assert!(
        !in_inode.is_empty(),
        "no attribute of /many is in its inode"
    );
}

#[test]
fn fs_ext4_xattr_lists_and_reads_what_the_kernel_set() {
    let image = sized_image("xattr-kernel", 32 << 20);
    let img = image.as_str();
    let made = oracle("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-b",
            "4096",
            "-I",
            "256",
            "-O",
            "ea_inode,inline_data",
            img,
        ])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));

    let out = guest_kernel_write(img, &report_script());
    assert!(
        out.status.success(),
        "the kernel could not fill the volume:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    let report = stdout(&out);
    let seen = parse(&report);
    assert_eq!(
        seen.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
        PATHS,
        "the guest reported each path:\n{report}"
    );
    assert_every_place_is_used(img);
    let before = std::fs::read(img).unwrap();
    let value_file = image_path("xattr-value");
    let mut namespaces = Vec::new();

    for (path, seen) in &seen {
        assert_eq!(
            seen.dump, seen.each,
            "{path}: the guest's getfattr -d and -n disagree:\n{report}"
        );
        let mut sizes: BTreeMap<String, usize> = seen
            .dump
            .iter()
            .map(|(n, v)| (n.clone(), v.len()))
            .collect();
        if seen.inline {
            sizes.insert("system.data".into(), seen.size.saturating_sub(60) as usize);
        }
        let what = |k: &str| format!("{path}: {k}\nthe guest's report:\n{report}");

        let list = ok(tool("fs.ext4").args([img, "xattr", path]));
        assert_eq!(
            stdout(&list).trim_end(),
            listing(&sizes),
            "{}",
            what("listing")
        );
        assert!(list.stderr.is_empty(), "{}", what("listing stderr"));
        let text = ok(tool("fs.ext4").args([img, "xattr", "--text", path]));
        let lines: Vec<String> = sizes.iter().map(|(n, s)| format!("{s}\t{n}")).collect();
        assert_eq!(
            stdout(&text).trim_end(),
            lines.join("\n"),
            "{}",
            what("--text")
        );

        for (name, value) in &seen.dump {
            namespaces.push(name.split('.').next().unwrap().to_string());
            let got = ok(tool("fs.ext4").args([img, "xattr", path, name]));
            assert_eq!(got.stdout, *value, "{}", what(name));
            assert!(got.stderr.is_empty(), "{}", what(name));
            let _ = std::fs::remove_file(&value_file);
            let to_file =
                ok(tool("fs.ext4").args([img, "xattr", "--text", path, name, "-o", &value_file]));
            assert!(to_file.stdout.is_empty(), "{}", what(name));
            assert_eq!(
                std::fs::read(&value_file).unwrap(),
                *value,
                "{}",
                what(name)
            );
            assert!(!std::path::Path::new(&format!("{value_file}.partial")).exists());
        }
        if seen.inline {
            let tail = ok(tool("fs.ext4").args([img, "xattr", path, "system.data"]));
            assert_eq!(
                tail.stdout.len(),
                sizes["system.data"],
                "{}",
                what("system.data")
            );
        }
    }
    for namespace in ["user", "trusted", "security"] {
        assert!(
            namespaces.iter().any(|n| n == namespace),
            "the kernel set no {namespace}. attribute:\n{report}"
        );
    }
    let many = &seen.iter().find(|(p, _)| p == "/many").unwrap().1;
    assert_eq!(many.dump["user.bin"], b"\0\xff\n\0\xff", "{report}");
    assert_eq!(many.dump["user.empty"], b"", "{report}");
    assert_eq!(many.dump["user.huge"].len(), 8000, "{report}");
    assert!(
        seen.iter().any(|(_, s)| s.inline),
        "lsattr marked nothing inline, so system.data went unchecked:\n{report}"
    );

    // The symlink's own attributes, never its target's.
    let through = tool("fs.ext4")
        .args([img, "xattr", "/link", "user.comment"])
        .output()
        .unwrap();
    assert_eq!(through.status.code(), Some(1), "{}", stderr(&through));
    assert!(through.stdout.is_empty());

    // --offset is where the filesystem starts in TARGET, as for every verb.
    let embedded = image_path("xattr-embedded");
    let mut bytes = vec![0u8; 4096];
    bytes.extend_from_slice(&before);
    std::fs::write(&embedded, bytes).unwrap();
    let offset =
        ok(tool("fs.ext4").args(["--offset", "4096", &embedded, "xattr", "/many", "user.bin"]));
    assert_eq!(offset.stdout, b"\0\xff\n\0\xff");

    assert!(
        std::fs::read(img).unwrap() == before,
        "fs.ext4 xattr changed the image"
    );
}
