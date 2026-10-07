//! `cargo install` puts exactly one program on PATH: `rust-fs-ext4`, the
//! multi-call binary named for the repository, which nothing else can
//! shadow. The README promises that, and every tool (`mkfs.ext4`,
//! `fsck.ext4`, `fs.ext4`) is reached through it.
//!
//! Every `[[bin]]` in Cargo.toml is something `cargo install` installs,
//! so a second one is a second name on the user's PATH that the README,
//! `doctor` and the release tarball know nothing about. This reads the
//! manifest and refuses any `[[bin]]` but the one.

const ENTRY_POINT: &str = "rust-fs-ext4";

/// The `name` of every `[[bin]]` table in the manifest, in order.
fn bin_names(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_bin = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_bin = line == "[[bin]]";
            continue;
        }
        if !in_bin {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "name" {
                names.push(value.trim().trim_matches('"').to_string());
            }
        }
    }
    names
}

#[test]
fn the_manifest_declares_one_binary_named_for_the_repository() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let manifest = std::fs::read_to_string(path).expect("read Cargo.toml");
    let names = bin_names(&manifest);
    assert!(
        names.iter().any(|n| n == ENTRY_POINT),
        "the scan found no [[bin]] named {ENTRY_POINT}, so it is not reading the manifest it guards: {names:?}"
    );
    let strays: Vec<&String> = names.iter().filter(|n| *n != ENTRY_POINT).collect();
    assert!(
        strays.is_empty(),
        "these [[bin]] targets are installed by `cargo install` beside {ENTRY_POINT}: {strays:?}. \
         Reach the tool through the multi-call binary instead."
    );
}

#[test]
fn the_scan_finds_every_bin_table() {
    let manifest = "[package]\nname = \"x\"\n\n[[bin]]\nname = \"a\"\npath = \"a.rs\"\n\n\
                    [[bin]]\nname = \"b\"\n\n[features]\nname = \"not-a-bin\"\n";
    assert_eq!(bin_names(manifest), vec!["a", "b"]);
}
