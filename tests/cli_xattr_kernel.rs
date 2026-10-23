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
//!
//! Linux takes any bytes but NUL in a name, and two of the names the guest
//! sets are not UTF-8 (#541): one on `/many` beside every other kind, and
//! one on `/raw`, an inline file whose tail is `system.data`. Each must be
//! listed lossily with its exact bytes in `name_hex`, read by the raw bytes
//! of NAME, and leave everything else on its inode readable, `/raw`'s
//! content included, which must be the bytes the guest's `od` read.

mod cli_support;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use cli_support::*;
use fs_ext4::block_io::FileDevice;
use fs_ext4::{xattr, Filesystem};
use fs_ext4_test_support::{guest_kernel_write, oracle};

/// The tree, filled by Linux: an empty file, a small inline file with one
/// attribute, a 5000-byte file whose attributes overflow the inode into an
/// external block and an EA inode, a directory with one of its own, and a
/// symlink with a trusted attribute its target does not have. `/many` and
/// `/raw`, a 70-byte inline file, each carry a name that is not UTF-8,
/// set after the content so the file stays inline.
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
setfattr -n "$(printf 'trusted.raw-\376')" -v 0x01fe many
printf 'seventy bytes: sixty of them in i_block, the last ten in system.data.\n' > raw
setfattr -n "$(printf 'user.raw-\377')" -v 0x7261770a raw
mkdir dir
setfattr -n user.dir -v dirvalue dir
ln -s many link
setfattr -h -n trusted.link -v 0x6c696e6b link
sync
"#;

const PATHS: [&str; 7] = ["/", "/none", "/small", "/many", "/raw", "/dir", "/link"];

/// The names the guest sets that are not UTF-8.
const RAW_MANY: &[u8] = b"trusted.raw-\xfe";
const RAW_INLINE: &[u8] = b"user.raw-\xff";

/// Per path: `@|PATH|LSATTR-FLAGS|SIZE`, then `D|` and each line of
/// `getfattr -d -m - -e hex`, then `V|` and each value `getfattr -n`
/// returned for every name `getfattr -m -` listed, and for a regular file
/// `C|` and its content in hex. Never through a symlink (`-h`), and a
/// symlink has no `lsattr` flags (`-`). getfattr prints a byte of a name
/// that is not printable as a backslash and three octal digits, which
/// `printf` turns back into the byte for `-n`.
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
    getfattr -h -n "$(printf "$name")" -e hex --absolute-names "$rel" | awk 'NF && !/^#/ {{ print "V|" $0 }}'
done
if [ -f "$rel" ] && [ ! -L "$rel" ]; then printf 'C|%s\n' "$(od -An -v -tx1 "$rel" | tr -d ' \n')"; fi
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
    dump: BTreeMap<Vec<u8>, Vec<u8>>,
    each: BTreeMap<Vec<u8>, Vec<u8>>,
    content: Option<Vec<u8>>,
}

/// A name as getfattr prints it, its bytes back: `\` and three octal
/// digits is one byte.
fn unquote(name: &str) -> Vec<u8> {
    let b = name.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = b
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match (b[i], octal) {
            (b'\\', Some(d)) => {
                out.push(d.iter().fold(0u8, |n, c| n.wrapping_mul(8) + (c - b'0')));
                i += 4;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A name as `fs.ext4 xattr` shows it.
fn lossy(name: &[u8]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

/// One `NAME=VALUE` line of `getfattr -e hex`: an empty value is printed
/// with no `=`, with nothing after it, or as `""`; any other is `0x` hex.
fn attribute(line: &str) -> (Vec<u8>, Vec<u8>) {
    let (name, value) = line.split_once('=').unwrap_or((line, ""));
    let value = match value {
        "" | "\"\"" => Vec::new(),
        hex => hex::decode(
            hex.strip_prefix("0x")
                .unwrap_or_else(|| panic!("a getfattr -e hex value: {line}")),
        )
        .unwrap_or_else(|e| panic!("{line}: {e}")),
    };
    (unquote(name), value)
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
        if kind == "C" {
            let content = hex::decode(rest).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(current.content.replace(content).is_none(), "twice: {line}");
            continue;
        }
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

/// The JSON `fs.ext4 xattr PATH` prints for these names and sizes: a name
/// that is not UTF-8 shown lossily, with its exact bytes in `name_hex`.
fn listing(sizes: &BTreeMap<Vec<u8>, usize>) -> String {
    if sizes.is_empty() {
        return "[]".to_string();
    }
    let items: Vec<String> = sizes
        .iter()
        .map(|(name, size)| {
            let hex = match std::str::from_utf8(name) {
                Ok(_) => String::new(),
                Err(_) => format!("\n    \"name_hex\": \"{}\",", hex_of(name)),
            };
            format!(
                "  {{\n    \"name\": \"{}\",{hex}\n    \"size\": {size}\n  }}",
                lossy(name)
            )
        })
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
        let mut sizes: BTreeMap<Vec<u8>, usize> = seen
            .dump
            .iter()
            .map(|(n, v)| (n.clone(), v.len()))
            .collect();
        if seen.inline {
            sizes.insert(
                b"system.data".to_vec(),
                seen.size.saturating_sub(60) as usize,
            );
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
        let lines: Vec<String> = sizes
            .iter()
            .map(|(n, s)| format!("{s}\t{}", lossy(n)))
            .collect();
        assert_eq!(
            stdout(&text).trim_end(),
            lines.join("\n"),
            "{}",
            what("--text")
        );

        for (name, value) in &seen.dump {
            let shown = lossy(name);
            namespaces.push(shown.split('.').next().unwrap().to_string());
            // NAME as its raw bytes, as the kernel holds it.
            let name = OsStr::from_bytes(name);
            let got = ok(tool("fs.ext4").args([img, "xattr", path]).arg(name));
            assert_eq!(got.stdout, *value, "{}", what(&shown));
            assert!(got.stderr.is_empty(), "{}", what(&shown));
            let _ = std::fs::remove_file(&value_file);
            let to_file = ok(tool("fs.ext4")
                .args([img, "xattr", "--text", path])
                .arg(name)
                .args(["-o", &value_file]));
            assert!(to_file.stdout.is_empty(), "{}", what(&shown));
            assert_eq!(
                std::fs::read(&value_file).unwrap(),
                *value,
                "{}",
                what(&shown)
            );
            assert!(!std::path::Path::new(&format!("{value_file}.partial")).exists());
        }
        if let Some(content) = &seen.content {
            let read = ok(tool("fs.ext4").args([img, "read", path]));
            assert_eq!(read.stdout, *content, "{}", what("content"));
        }
        if seen.inline {
            let tail = ok(tool("fs.ext4").args([img, "xattr", path, "system.data"]));
            assert_eq!(
                tail.stdout.len(),
                sizes[b"system.data".as_slice()],
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
    assert_eq!(
        many.dump[b"user.bin".as_slice()],
        b"\0\xff\n\0\xff",
        "{report}"
    );
    assert_eq!(many.dump[b"user.empty".as_slice()], b"", "{report}");
    assert_eq!(many.dump[b"user.huge".as_slice()].len(), 8000, "{report}");
    // The names that are not UTF-8 came back as the bytes the guest set:
    // a report read lossily would have lost them.
    assert_eq!(many.dump[RAW_MANY], b"\x01\xfe", "{report}");
    let raw = &seen.iter().find(|(p, _)| p == "/raw").unwrap().1;
    assert_eq!(raw.dump[RAW_INLINE], b"raw\n", "{report}");
    assert!(raw.inline, "lsattr did not mark /raw inline:\n{report}");
    assert_eq!(raw.content.as_ref().map(Vec::len), Some(70), "{report}");
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
