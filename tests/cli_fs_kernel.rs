//! `fs.ext4` reads back what THE KERNEL wrote: a volume made by
//! `mkfs.ext4` in the harness VM, loop-mounted there and filled by Linux.
//! Every file `fs.ext4 read` returns must hash to what the guest's
//! `sha256sum` printed for it, and `ls` must show the tree the kernel made.

mod cli_support;

use cli_support::*;
use fs_ext4_test_support::{guest_kernel_write, oracle, sha256_hex};

#[test]
fn fs_ext4_reads_back_what_the_kernel_wrote() {
    let img = sized_image("kernel-made", 32 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-L", "KERNVOL", &img])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));

    let names = ["empty", "one", "f4095", "f4096", "f4097", "big", "d/e/deep"];
    let script = "cd \"$MNT\"\n\
                  mkdir -p d/e\n\
                  : > empty\n\
                  head -c 1 /dev/urandom > one\n\
                  head -c 4095 /dev/urandom > f4095\n\
                  head -c 4096 /dev/urandom > f4096\n\
                  head -c 4097 /dev/urandom > f4097\n\
                  head -c 1048576 /dev/urandom > big\n\
                  head -c 5000 /dev/urandom > d/e/deep\n\
                  ln -s big link\n\
                  sha256sum empty one f4095 f4096 f4097 big d/e/deep\n\
                  sync\n";
    let out = guest_kernel_write(&img, script);
    assert!(
        out.status.success(),
        "the kernel could not fill the volume:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    let sums = stdout(&out);
    for name in names {
        let want = sums
            .lines()
            .find_map(|l| l.strip_suffix(&format!("  {name}")))
            .filter(|h| h.len() == 64)
            .unwrap_or_else(|| panic!("the guest printed no sha256 for {name}:\n{sums}"));
        let read = ok(tool("fs.ext4").args([&img, "read", &format!("/{name}")]));
        assert_eq!(sha256_hex(&read.stdout), want, "/{name}");
    }

    let root = stdout(&ok(tool("fs.ext4").args([&img, "ls", "/"])));
    let link = &root[root.find("\"name\": \"link\"").expect("link listed")..];
    assert_eq!(json_field(link, "type"), "symlink");
    assert_eq!(json_field(link, "target"), "big");
    let deep = stdout(&ok(tool("fs.ext4").args([&img, "ls", "/d/e"])));
    assert_eq!(json_field(&deep, "name"), "deep");
    assert_eq!(json_field(&deep, "size"), "5000");
    let label = stdout(&ok(tool("fs.ext4").args([&img, "get", "label", "--text"])));
    assert_eq!(label, "KERNVOL\n");
}
