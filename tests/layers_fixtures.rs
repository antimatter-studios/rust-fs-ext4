//! rust-fs-core's three-layer conformance checks on every committed
//! fixture: the volume reports itself clean, the primitives and salvage
//! verbs keep their contracts, the file layer reads every file back
//! consistently, and every file it lists is a salvage candidate whose walk
//! reads back the same bytes.
//!
//! The fixtures are kernel-made (`chore fixtures`); what they hold is
//! checked against the kernel by `tests/layers_oracle.rs`, and here the
//! contracts are checked on the layouts they cover: htree, inline data,
//! extended attributes, deep extent trees, `csum_seed`, no checksums and
//! ACLs.

use fs_core::layers::conformance::{check_files_against_candidates, check_healthy, Limits};
use fs_core::layers::OpenVolume;
use fs_core::FileDevice;
use fs_ext4::layers::Ext4Volume;
use fs_ext4_test_support::fixture;

/// Every fixture but `ext4-largedir.img`, whose 70,000-entry directory is
/// past what the conformance checks walk ([`Limits`]); its hashed-directory
/// reading has tests of its own (`tests/htree_*`).
const FIXTURES: [&str; 8] = [
    "ext4-basic.img",
    "ext4-htree.img",
    "ext4-inline.img",
    "ext4-xattr.img",
    "ext4-deep-extents.img",
    "ext4-csum-seed.img",
    "ext4-no-csum.img",
    "ext4-acl.img",
];

#[test]
fn every_fixture_conforms() {
    let limits = Limits::default();
    let mut failed = Vec::new();
    for name in FIXTURES {
        let path = fixture(env!("CARGO_MANIFEST_DIR"), name);
        let volume = Ext4Volume::open(FileDevice::open(&path).unwrap())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut report = check_healthy(&volume, &[], &limits);
        report.merge(check_files_against_candidates(&volume, &limits));
        if !report.passed() {
            failed.push(format!("{name}:\n{report}"));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
