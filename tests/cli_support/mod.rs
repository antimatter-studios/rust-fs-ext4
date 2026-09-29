//! What the command-line tests share: the multi-call binary, reached
//! under each of its names, and a place to put images.
//!
//! The binary is built only with the `cli` feature. `scripts/test.sh`
//! turns it on for every tier; a bare `cargo test` does not, and then
//! these tests FAIL naming the fix rather than skipping.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

/// The repository-named entry point, as cargo built it.
///
/// `option_env!`, not `env!`: without the feature `env!` would fail the
/// compile of every test target in the run, where this fails only the
/// tests that need the binary, each with the fix in its message.
const BIN: Option<&str> = option_env!("CARGO_BIN_EXE_rust-fs-ext4");

const NO_BIN: &str = "the rust-fs-ext4 binary is built only with `--features cli`. Run the \
    tests through scripts/test.sh, which passes it, or add `--features cli` to cargo test.";

pub fn bin() -> &'static str {
    BIN.expect(NO_BIN)
}

/// The binary under its own (cargo's) name: the repository entry point.
pub fn entry() -> Command {
    Command::new(BIN.expect(NO_BIN))
}

/// The program as a user runs it under `name`: argv[0] is what an
/// installed symlink hands it, and what it dispatches on.
pub fn tool(name: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(BIN.expect(NO_BIN));
    cmd.arg0(name);
    cmd
}

/// A directory holding the binary under every name it answers to, as an
/// install links it: `rust-fs-ext4` and each dotted name, symlinks to
/// cargo's build.
pub fn names_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = PathBuf::from(fs_ext4_test_support::temp_path!(
            "cli-names-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the names directory");
        let mut names = dotted_names();
        names.push("rust-fs-ext4".to_string());
        for name in names {
            std::os::unix::fs::symlink(bin(), dir.join(&name))
                .unwrap_or_else(|e| panic!("link {name}: {e}"));
        }
        dir
    })
}

/// The dotted names, as the binary itself lists them for packaging.
pub fn dotted_names() -> Vec<String> {
    let out = entry()
        .args(["generate", "names"])
        .output()
        .expect("run rust-fs-ext4 generate names");
    assert!(out.status.success(), "generate names failed: {out:?}");
    String::from_utf8(out.stdout)
        .expect("names are UTF-8")
        .lines()
        .map(str::to_string)
        .collect()
}

/// A fresh image path in the scratch directory, not yet created.
pub fn image_path(tag: &str) -> String {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = fs_ext4_test_support::temp_path!("cli-{}-{n}-{tag}.img", std::process::id());
    let _ = std::fs::remove_file(&path);
    path
}

/// A sparse file of `size` bytes, ready to format.
pub fn sized_image(tag: &str, size: u64) -> String {
    let path = image_path(tag);
    std::fs::File::create(&path)
        .and_then(|f| f.set_len(size))
        .unwrap_or_else(|e| panic!("size {path}: {e}"));
    path
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Run and require success, returning the output.
#[track_caller]
pub fn ok(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{cmd:?} failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        stdout(&out),
        stderr(&out)
    );
    out
}

/// The value of `"key": ...` in a JSON report: enough of a reader for the
/// flat reports these tests check, without a JSON dependency. Strings
/// come back without their quotes; anything else as written.
#[track_caller]
pub fn json_field(json: &str, key: &str) -> String {
    let needle = format!("\"{key}\": ");
    let start = json
        .find(&needle)
        .unwrap_or_else(|| panic!("no {key:?} in:\n{json}"))
        + needle.len();
    let rest = &json[start..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"').expect("closing quote");
        stripped[..end].to_string()
    } else {
        rest.split([',', '\n', '}'])
            .next()
            .unwrap()
            .trim()
            .to_string()
    }
}

/// Damage `image` in a way the audit can repair: group 0's descriptor
/// claims `extra` more free blocks than its bitmap has, with the
/// descriptor's checksum restamped so the volume still mounts.
pub fn corrupt_group_free_blocks(image: &str, extra: u16) {
    use fs_ext4::block_io::{BlockDevice, FileDevice};
    let fs = fs_ext4::Filesystem::mount(std::sync::Arc::new(
        FileDevice::open(image).expect("open image"),
    ))
    .expect("mount image");
    let block = u64::from(fs.sb.block_size());
    let desc_size = usize::from(fs.sb.desc_size.max(32));
    let at = (u64::from(fs.sb.first_data_block) + 1) * block;
    let dev = FileDevice::open_rw(image).expect("open image rw");
    let mut desc = vec![0u8; desc_size];
    dev.read_at(at, &mut desc).expect("read descriptor 0");
    let free = u16::from_le_bytes([desc[0x0C], desc[0x0D]]);
    desc[0x0C..0x0E].copy_from_slice(&free.wrapping_add(extra).to_le_bytes());
    if let Some(csum) = fs_ext4::checksum::group_desc_csum(&fs.sb, &fs.csum, 0, &desc) {
        desc[0x1E..0x20].copy_from_slice(&csum.to_le_bytes());
    }
    dev.write_at(at, &desc).expect("write descriptor 0");
    dev.flush().expect("flush");
}

/// Damage `image` in a way nothing repairs: the root directory's extent
/// header is zeroed, with the inode's checksum restamped so it is the
/// extent tree, not the checksum, that is wrong.
pub fn destroy_root_extent_header(image: &str) {
    use fs_ext4::block_io::{BlockDevice, FileDevice};
    let fs = fs_ext4::Filesystem::mount(std::sync::Arc::new(
        FileDevice::open(image).expect("open image"),
    ))
    .expect("mount image");
    let (block, offset) =
        fs_ext4::bgd::locate_inode(&fs.sb, &fs.groups, 2).expect("locate the root inode");
    let at = block * u64::from(fs.sb.block_size()) + u64::from(offset);
    let dev = FileDevice::open_rw(image).expect("open image rw");
    let mut raw = vec![0u8; usize::from(fs.sb.inode_size)];
    dev.read_at(at, &mut raw).expect("read the root inode");
    raw[0x28..0x28 + 12].fill(0);
    let generation = u32::from_le_bytes(raw[0x64..0x68].try_into().unwrap());
    if let Some((lo, hi)) = fs.csum.compute_inode_checksum(2, generation, &raw) {
        raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
        let extra_isize = u16::from_le_bytes([raw[0x80], raw[0x81]]);
        if raw.len() > 0x84 && extra_isize >= 4 {
            raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
        }
    }
    dev.write_at(at, &raw).expect("write the root inode");
    dev.flush().expect("flush");
}
