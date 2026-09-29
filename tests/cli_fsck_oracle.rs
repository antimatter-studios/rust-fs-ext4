//! `fsck.ext4`'s verdict held to e2fsck's (`e2fsck -fn`, in the harness
//! VM) on the same images: a fresh one both call clean; a wrong group
//! free count and a destroyed root extent header both call damaged, so
//! the damage is real and not something only our checker notices; and
//! after `fsck.ext4 -y` repairs the free count, e2fsck calls it clean
//! again. (fsck.ext4 itself misses the destroyed root today, #445: the
//! assertion on its status joins this test when that is fixed.)

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{assert_e2fsck_clean, oracle};

fn fresh(tag: &str) -> String {
    let img = image_path(tag);
    ok(tool("mkfs.ext4").args(["-q", "--size", "32M", "--text", &img]));
    img
}

fn our_status(args: &[&str]) -> Option<i32> {
    tool("fsck.ext4").args(args).output().unwrap().status.code()
}

#[test]
fn fsck_ext4_and_e2fsck_agree_on_clean_damaged_and_repaired_images() {
    let clean = fresh("oracle-clean");
    assert_eq!(our_status(&["-n", &clean]), Some(0));
    assert_e2fsck_clean(&clean, "a fresh image fsck.ext4 calls clean");

    let drift = fresh("oracle-drift");
    corrupt_group_free_blocks(&drift, 7);
    assert_eq!(our_status(&["-n", &drift]), Some(4));
    let said = oracle("e2fsck")
        .args(["-fn", &drift])
        .judged()
        .findings("a wrong group free count");
    assert!(
        said.to_lowercase().contains("free blocks count wrong"),
        "e2fsck found something else wrong:\n{said}"
    );
    assert_eq!(our_status(&["-y", &drift]), Some(1));
    assert_eq!(our_status(&["-n", &drift]), Some(0));
    assert_e2fsck_clean(&drift, "the free count fsck.ext4 -y repaired");

    let rootless = fresh("oracle-rootless");
    destroy_root_extent_header(&rootless);
    oracle("e2fsck")
        .args(["-fn", &rootless])
        .judged()
        .findings("a destroyed root extent header");
}
