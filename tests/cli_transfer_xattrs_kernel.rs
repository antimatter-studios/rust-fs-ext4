//! `export` and `import --recursive --preserve=xattrs` carry extended
//! attributes, checked against the independent Linux kernel in the harness
//! VM: what the guest's `getfattr` lists on a file it attributed with
//! `setfattr` is what the host holds after export, and what the host set
//! before import is what the guest's `getfattr` lists afterwards, name for
//! name and byte for byte.
//!
//! A namespace the other side cannot hold is refused for its own item in
//! the report, never dropped: the test runs unprivileged, so the host
//! cannot hold a `trusted.` attribute the kernel set on the volume.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

type Attributes = BTreeMap<Vec<u8>, Vec<u8>>;

fn fresh(tag: &str) -> String {
    let img = sized_image(tag, 64 << 20);
    let out = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256", &img])
        .output();
    assert!(out.status.success(), "mkfs.ext4: {}", stderr(&out));
    img
}

/// A fresh host directory on a filesystem that holds `user.` attributes.
/// The harness guest's 9p share does not, so there it is in `/var/tmp`.
fn host_dir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(path_where(
        tag,
        "holds user. extended attributes, which --preserve=xattrs needs",
        holds_user_attributes,
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Whether a file in `dir` keeps a `user.` attribute set on it.
fn holds_user_attributes(dir: &Path) -> bool {
    let probe = dir.join(format!(".xattr-probe-{}", std::process::id()));
    let kept = std::fs::create_dir_all(dir).is_ok()
        && std::fs::write(&probe, b"").is_ok()
        && xattr::set(&probe, "user.probe", b"v").is_ok()
        && xattr::get(&probe, "user.probe").is_ok_and(|v| v.as_deref() == Some(&b"v"[..]));
    let _ = std::fs::remove_file(&probe);
    kept
}

/// Every attribute of each of `paths` below `$MNT`, as the guest's
/// `getfattr -d -m - -e hex` prints them: `PATH|NAME=VALUE` per line.
fn dump(paths: &[&str]) -> String {
    paths
        .iter()
        .map(|p| {
            format!(
                "getfattr -h -d -m - -e hex --absolute-names \"$MNT/{p}\" \
                 | awk -v p='{p}' 'NF && !/^#/ {{ print p \"|\" $0 }}'\n"
            )
        })
        .collect()
}

/// What the guest printed for `path`: an empty value is printed with no
/// `=`, with nothing after it, or as `""`; any other is `0x` hex.
fn guest(listing: &str, path: &str) -> Attributes {
    let mut out = Attributes::new();
    for line in listing.lines() {
        let Some(line) = line.strip_prefix(&format!("{path}|")) else {
            continue;
        };
        let (name, value) = line.split_once('=').unwrap_or((line, ""));
        let value = match value {
            "" | "\"\"" => Vec::new(),
            hex => hex::decode(
                hex.strip_prefix("0x")
                    .unwrap_or_else(|| panic!("a getfattr -e hex value: {line}")),
            )
            .unwrap_or_else(|e| panic!("{line}: {e}")),
        };
        assert!(
            out.insert(name.as_bytes().to_vec(), value).is_none(),
            "twice: {line}"
        );
    }
    out
}

/// Every attribute the host holds on `path`, symbolic links not followed.
fn host(path: &Path) -> Attributes {
    xattr::list(path)
        .unwrap_or_else(|e| panic!("list {}: {e}", path.display()))
        .map(|name| {
            let value = xattr::get(path, &name)
                .unwrap()
                .unwrap_or_else(|| panic!("{name:?} vanished"));
            (name.into_encoded_bytes(), value)
        })
        .collect()
}

/// The report's item whose `key` is `value`: the text from that key to the
/// next item's.
fn item<'a>(report: &'a str, key: &str, value: &str) -> &'a str {
    let needle = format!("\"{key}\": \"{value}\"");
    let start = report
        .find(&needle)
        .unwrap_or_else(|| panic!("no item for {value} in:\n{report}"));
    let rest = &report[start + needle.len()..];
    &rest[..rest.find(&format!("\"{key}\": ")).unwrap_or(rest.len())]
}

#[test]
fn export_carries_what_the_kernel_set_and_refuses_a_namespace_the_host_cannot_hold() {
    let img = fresh("xattr-export");
    let made = guest_kernel_write(
        &img,
        &format!(
            "cd \"$MNT\"\n\
             mkdir tree tree/sub\n\
             echo data > tree/f\n\
             echo private > tree/priv\n\
             setfattr -n user.comment -v 'hello world' tree/f\n\
             setfattr -n user.bin -v 0x00ff0a00ff tree/f\n\
             setfattr -n user.empty -v '\"\"' tree/f\n\
             setfattr -n user.medium -v \"0x$(head -c 1500 /dev/zero | tr '\\0' 'a' | od -An -v -tx1 | tr -d ' \\n')\" tree/f\n\
             setfattr -n user.dir -v dirvalue tree/sub\n\
             setfattr -n user.ok -v fine tree/priv\n\
             setfattr -n trusted.audit -v 0x7472757374656400ff tree/priv\n\
             sync\n\
             {}",
            dump(&["tree/f", "tree/sub", "tree/priv"])
        ),
    );
    assert!(made.status.success(), "{}{}", stdout(&made), stderr(&made));
    let listing = stdout(&made);
    let file = guest(&listing, "tree/f");
    assert_eq!(file.len(), 4, "the guest set four attributes: {listing}");
    assert_eq!(file[b"user.medium".as_slice()].len(), 1500, "{listing}");
    let dir = guest(&listing, "tree/sub");
    assert_eq!(dir.len(), 1, "{listing}");
    assert!(
        guest(&listing, "tree/priv").contains_key(b"trusted.audit".as_slice()),
        "{listing}"
    );
    assert_e2fsck_clean(&img, "the kernel-attributed tree");

    let dest = host_dir("xattr-export-dest");
    let out = tool("fs.ext4")
        .args([
            &img,
            "export",
            "--recursive",
            "--preserve=xattrs",
            "/tree",
            dest.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
    assert_eq!(json_field(&report, "refused"), "1", "{report}");
    assert_eq!(json_field(&report, "failed"), "0", "{report}");
    for path in ["/tree/f", "/tree/sub"] {
        assert!(
            item(&report, "path", path).contains("\"status\": \"completed\""),
            "{path}: {report}"
        );
    }
    let refused = item(&report, "path", "/tree/priv");
    assert!(refused.contains("\"status\": \"refused\""), "{report}");
    assert!(refused.contains("trusted.audit"), "named: {report}");

    assert_eq!(host(&dest.join("f")), file, "{listing}");
    assert_eq!(host(&dest.join("sub")), dir, "{listing}");
    assert_eq!(std::fs::read(dest.join("f")).unwrap(), b"data\n");
    std::fs::remove_dir_all(&dest).unwrap();
}

#[test]
fn import_carries_what_the_host_set_to_what_the_kernel_lists() {
    let img = fresh("xattr-import");
    let src = host_dir("xattr-import-src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("f"), b"data\n").unwrap();
    let medium: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    let mut file = Attributes::new();
    for (name, value) in [
        (&b"user.comment"[..], &b"hello world"[..]),
        (b"user.bin", b"\x00\xff\n\x00\xff"),
        (b"user.empty", b""),
        (b"user.medium", &medium),
    ] {
        xattr::set(src.join("f"), std::str::from_utf8(name).unwrap(), value).unwrap();
        file.insert(name.to_vec(), value.to_vec());
    }
    xattr::set(src.join("sub"), "user.dir", b"dirvalue").unwrap();
    let dir = Attributes::from([(b"user.dir".to_vec(), b"dirvalue".to_vec())]);
    assert_eq!(host(&src.join("f")), file, "the host holds what was set");

    let report = stdout(&ok(tool("fs.ext4").args([
        &img,
        "import",
        "--recursive",
        "--preserve=xattrs",
        src.to_str().unwrap(),
        "/tree",
    ])));
    for dest in ["/tree", "/tree/f", "/tree/sub"] {
        assert!(
            item(&report, "destination", dest).contains("\"status\": \"completed\""),
            "{dest}: {report}"
        );
    }

    let seen = guest_kernel_write(&img, &dump(&["tree/f", "tree/sub"]));
    assert!(seen.status.success(), "{}", stderr(&seen));
    let listing = stdout(&seen);
    assert_eq!(guest(&listing, "tree/f"), file, "{listing}");
    assert_eq!(guest(&listing, "tree/sub"), dir, "{listing}");
    assert_e2fsck_clean(&img, "import --preserve=xattrs");
    std::fs::remove_dir_all(&src).unwrap();
}
