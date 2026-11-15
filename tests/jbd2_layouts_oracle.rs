//! Journal tag sizes and checksum declarations, against journals and
//! verdicts from the real toolchain.
//!
//! kernel.org's journal.html describes the tag records and the feature
//! bits, but not every size (the older tag's 16-bit checksum slot exists
//! only under CSUM_V2) nor which combinations of checksum declarations a
//! journal may make. Both are measured here:
//!
//! - `debugfs jo [-c -v N]` + `jw` write one transaction in each of the six
//!   tag layouts. The distance between consecutive tags in its descriptor
//!   block must be [`JournalSuperblock::tag_bytes`], `debugfs logdump` must
//!   find the four tags, and this crate must replay the transaction.
//! - A clean journal's superblock is patched to each checksum declaration
//!   (with its own checksum recomputed). The kernel mounts it or refuses to
//!   load the journal, `e2fsck -fn` checks it or calls the journal
//!   superblock corrupt, and [`JournalSuperblock::checksum_declaration_error`]
//!   must agree with both.
//!
//! All of it runs in the harness VM, and fails without it.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use fs_ext4::jbd2::{JournalSuperblock, JBD2_FEATURE_COMPAT_CHECKSUM};
use fs_ext4_test_support::oracle;
use std::sync::Arc;

const BS: u64 = 1024;
/// Blocks the transaction logs, chosen to be recognisable in a
/// descriptor block (and inside a 64 MiB volume of 1 KiB blocks).
const LOGGED: [u32; 4] = [300, 301, 302, 40000];

fn volume(tag: &str, mib: u64, features: &str) -> String {
    let image = fs_ext4_test_support::temp_path!("fs_ext4_jbd2_{tag}_{}.img", std::process::id());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(mib << 20))
        .unwrap();
    let out = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "1024", "-O", features, &image])
        .output();
    assert!(
        out.status.success(),
        "[{tag}] mkfs.ext4: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    image
}

/// Physical block of journal block `n`, as debugfs maps it.
fn journal_block(image: &str, n: u32) -> u64 {
    let out = oracle("debugfs")
        .args(["-R", &format!("bmap <8> {n}"), image])
        .output();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("bmap <8> {n}: {}", String::from_utf8_lossy(&out.stdout)))
}

fn read_at(image: &str, block: u64) -> Vec<u8> {
    let bytes = fs_ext4_test_support::read_small_file(image).unwrap();
    let at = (block * BS) as usize;
    bytes[at..at + BS as usize].to_vec()
}

fn payload() -> Vec<u8> {
    (0..LOGGED.len() * BS as usize)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).rotate_left(7) as u8)
        .collect()
}

fn tag_layout(tag: &str, features: &str, open: &str, expected: usize) {
    let image = volume(tag, 64, features);
    let data = fs_ext4_test_support::temp_path!("fs_ext4_jbd2_{tag}_{}.bin", std::process::id());
    std::fs::write(&data, payload()).unwrap();
    let blocks: Vec<String> = LOGGED.iter().map(u32::to_string).collect();
    let script = format!("{open}\njw -b {} {data}\njc\n", blocks.join(","));
    let out = oracle("debugfs")
        .args(["-w", "-f", "-", &image])
        .stdin(script)
        .output();
    assert!(out.status.success(), "[{tag}] debugfs journal write");

    let jsb = JournalSuperblock::parse(&read_at(&image, journal_block(&image, 0))).unwrap();
    assert!(
        !jsb.is_clean(),
        "[{tag}] debugfs left no transaction to measure"
    );
    assert_eq!(jsb.tag_bytes(), expected, "[{tag}] tag_bytes");

    // Measured: where each logged block number sits in the descriptor.
    let descriptor = read_at(&image, journal_block(&image, 1));
    let at: Vec<usize> = LOGGED
        .iter()
        .map(|n| {
            descriptor[12..]
                .windows(4)
                .position(|w| w == n.to_be_bytes())
                .map(|p| p + 12)
                .unwrap_or_else(|| panic!("[{tag}] block {n} is not in the descriptor"))
        })
        .collect();
    assert_eq!(at[0], 12, "[{tag}] the first tag follows the header");
    assert_eq!(
        at[1] - at[0],
        expected + 16,
        "[{tag}] first tag and its UUID"
    );
    for pair in at[1..].windows(2) {
        assert_eq!(pair[1] - pair[0], expected, "[{tag}] tag spacing");
    }

    let out = oracle("debugfs")
        .args(["-R", "logdump -a", &image])
        .output();
    let log = String::from_utf8_lossy(&out.stdout);
    for n in LOGGED {
        assert!(
            log.contains(&format!("FS block {n} logged")),
            "[{tag}] logdump does not list block {n}:\n{log}"
        );
    }

    // This crate replays it: the logged blocks now hold the payload.
    let want = payload();
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap()))
            .unwrap_or_else(|e| panic!("[{tag}] mount and replay: {e:?}"));
        for (i, &n) in LOGGED.iter().enumerate() {
            let got = fs.read_block(u64::from(n)).unwrap();
            assert!(
                got == want[i * BS as usize..(i + 1) * BS as usize],
                "[{tag}] block {n} after replay"
            );
        }
    }
    let _ = std::fs::remove_file(&image);
    let _ = std::fs::remove_file(&data);
}

#[test]
fn a_plain_32_bit_journal_has_8_byte_tags() {
    tag_layout("plain32", "^64bit,^metadata_csum", "jo", 8);
}

#[test]
fn a_plain_64_bit_journal_has_12_byte_tags() {
    tag_layout("plain64", "64bit,^metadata_csum", "jo", 12);
}

#[test]
fn a_csum_v2_32_bit_journal_has_10_byte_tags() {
    tag_layout("v2_32", "^64bit,metadata_csum", "jo -c -v 2", 10);
}

#[test]
fn a_csum_v2_64_bit_journal_has_14_byte_tags() {
    tag_layout("v2_64", "64bit,metadata_csum", "jo -c -v 2", 14);
}

#[test]
fn a_csum_v3_32_bit_journal_has_16_byte_tags() {
    tag_layout("v3_32", "^64bit,metadata_csum", "jo -c -v 3", 16);
}

#[test]
fn a_csum_v3_64_bit_journal_has_16_byte_tags() {
    tag_layout("v3_64", "64bit,metadata_csum", "jo -c -v 3", 16);
}

/// `(compat, incompat, s_checksum_type, accepted)` for a clean journal.
const DECLARATIONS: [(u32, u32, u8, bool); 14] = [
    (0, 0, 0, true),
    (0, 0, 4, true),
    (V1, 0, 0, true),
    (V1, 0, 1, true),
    (V1, 0, 4, true),
    (0, V2, 4, true),
    (0, V3, 4, true),
    (0, V2 | V3, 4, false),
    (V1, V2, 4, false),
    (V1, V3, 4, false),
    (0, V2, 1, false),
    (0, V3, 1, false),
    (0, V3, 2, false),
    (0, V3, 0, false),
];
const V1: u32 = JBD2_FEATURE_COMPAT_CHECKSUM;
const V2: u32 = 0x8;
const V3: u32 = 0x10;

#[test]
fn checksum_declarations_are_accepted_and_refused_as_the_kernel_and_e2fsck_do() {
    for (i, &(compat, incompat, kind, accepted)) in DECLARATIONS.iter().enumerate() {
        let tag = format!("decl{i}");
        let what = format!("compat {compat:#x} incompat {incompat:#x} type {kind}");
        let image = volume(&tag, 16, "metadata_csum");
        let at = journal_block(&image, 0) * BS;

        let mut bytes = fs_ext4_test_support::read_small_file(&image).unwrap();
        let sb = &mut bytes[at as usize..at as usize + 1024];
        sb[0x24..0x28].copy_from_slice(&compat.to_be_bytes());
        sb[0x28..0x2C].copy_from_slice(&incompat.to_be_bytes());
        sb[0x50] = kind;
        sb[0xFC..0x100].fill(0);
        let csum = fs_ext4::checksum::linux_crc32c(!0, sb);
        sb[0xFC..0x100].copy_from_slice(&csum.to_be_bytes());
        let jsb = JournalSuperblock::parse(sb).unwrap();
        std::fs::write(&image, &bytes).unwrap();

        assert_eq!(
            jsb.checksum_declaration_error().is_none(),
            accepted,
            "this crate on {what}: {:?}",
            jsb.checksum_declaration_error()
        );

        let judged = oracle("e2fsck").args(["-fn", &image]).judged();
        let report = judged.report();
        if accepted {
            judged.clean(&what);
        } else {
            assert!(
                report.contains("Journal superblock is corrupt"),
                "e2fsck on {what}:\n{report}"
            );
        }

        // The kernel oracle mounts before it runs anything, and ends the
        // call when the mount is refused; that ending is the answer here.
        let kernel = fs_ext4_test_support::guest_kernel_try_write(&image, "true");
        match (kernel, accepted) {
            (Ok(out), true) => assert!(
                out.status.success(),
                "the kernel on {what}:\n{}",
                String::from_utf8_lossy(&out.stderr)
            ),
            (Err(said), false) => {
                assert!(
                    said.contains("mount:") && said.contains("bad superblock"),
                    "the kernel on {what} failed, but not by refusing the mount:\n{said}"
                );
            }
            (Ok(_), false) => panic!("the kernel mounted {what}, which it should refuse"),
            (Err(said), true) => {
                panic!("the kernel refused {what}, which it should mount:\n{said}")
            }
        }
        let _ = std::fs::remove_file(&image);
    }
}
