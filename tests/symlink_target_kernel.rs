//! The library and CLI accept exactly the symlink target lengths accepted
//! by symlinkat on the kernel's mount, including room for its terminating
//! byte. Kernel-made 1 KiB and 4 KiB volumes cover the limit and both sides
//! of it; refused library and CLI calls must leave the image byte-identical.

mod cli_support;

use cli_support::*;
use fs_ext4::block_io::FileDevice;
use fs_ext4::{Error, Filesystem};
use fs_ext4_test_support::{assert_e2fsck_clean, guest_kernel_write, oracle};
use std::sync::Arc;

// A black-box syscall probe, compiled and run only in the harness guest.
// A successful creation must also return the exact target through readlinkat.
const PROBE: &str = r#"#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    for (int i = 1; i < argc; i++) {
        size_t n = strtoul(argv[i], NULL, 10);
        char *target = malloc(n + 1), *readback = malloc(n + 1);
        if (!target || !readback) return 2;
        memset(target, 'x', n);
        target[n] = 0;
        char name[64];
        snprintf(name, sizeof name, "kernel-%zu", n);
        if (symlinkat(target, AT_FDCWD, name) == 0) {
            ssize_t got = readlinkat(AT_FDCWD, name, readback, n + 1);
            if (got != (ssize_t)n || memcmp(target, readback, n) != 0) return 3;
            printf("%zu|OK\n", n);
        } else if (errno == ENAMETOOLONG) {
            printf("%zu|ENAMETOOLONG\n", n);
        } else {
            perror("symlinkat");
            return 4;
        }
        free(target);
        free(readback);
    }
    return 0;
}
"#;

fn compare_with_kernel(block_size: usize) {
    let img = sized_image(&format!("symlink-target-{block_size}"), 32 << 20);
    let made = oracle("mkfs.ext4")
        .args(["-q", "-F", "-b", &block_size.to_string(), "-I", "256", &img])
        .output();
    assert!(made.status.success(), "mkfs.ext4: {}", stderr(&made));

    let limit = block_size - 1;
    let lengths = [limit - 1, limit, limit + 1, limit + 2];
    let arguments = lengths.map(|n| n.to_string()).join(" ");
    let script = format!(
        "cd \"$MNT\"\n\
         build=\"$(mktemp -d)\"\n\
         cat > \"$build/probe.c\" <<'EOF'\n\
         {PROBE}\
         EOF\n\
         cc -o \"$build/probe\" \"$build/probe.c\"\n\
         \"$build/probe\" {arguments}\n\
         rm -rf \"$build\"\n"
    );
    let out = guest_kernel_write(&img, &script);
    assert!(out.status.success(), "kernel probe: {}", stderr(&out));
    let expected = lengths
        .map(|n| format!("{n}|{}\n", if n <= limit { "OK" } else { "ENAMETOOLONG" }))
        .concat();
    assert_eq!(
        stdout(&out),
        expected,
        "the kernel's boundary at {block_size}"
    );

    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&img).unwrap())).unwrap();
        for len in lengths {
            let target = "x".repeat(len);
            for prefix in ["inode", "path"] {
                let name = format!("{prefix}-{len}");
                let before = std::fs::read(&img).unwrap();
                let result = if prefix == "inode" {
                    fs.apply_symlink_at(2, name.as_bytes(), target.as_bytes())
                } else {
                    fs.apply_symlink(&target, &format!("/{name}"))
                };
                if len <= limit {
                    let ino = result.unwrap();
                    assert_eq!(fs.read_link(ino).unwrap(), target.as_bytes());
                } else {
                    assert!(
                        matches!(result, Err(Error::NameTooLong)),
                        "{prefix}, block size {block_size}, length {len}: {result:?}"
                    );
                    assert!(
                        before == std::fs::read(&img).unwrap(),
                        "refusal wrote to image"
                    );
                    assert!(matches!(
                        fs.lookup_at(2, name.as_bytes()),
                        Err(Error::NotFound)
                    ));
                }
            }
        }
    }

    // Keep the CLI's read-only precheck identical to the library's limit.
    for len in lengths {
        let target = "x".repeat(len);
        let name = format!("/cli-{len}");
        let before = std::fs::read(&img).unwrap();
        let out = tool("fs.ext4")
            .args([&img, "ln", "-s", &target, &name])
            .output()
            .expect("spawn fs.ext4");
        if len <= limit {
            assert!(out.status.success(), "CLI length {len}: {}", stderr(&out));
        } else {
            assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
            assert!(stderr(&out).contains("name too long"), "{}", stderr(&out));
            assert!(out.stdout.is_empty());
            assert!(
                before == std::fs::read(&img).unwrap(),
                "CLI refusal wrote to image"
            );
        }
    }

    assert_e2fsck_clean(&img, "symlink target boundary writes");
    let script = format!(
        "cd \"$MNT\"\n\
         for n in {} {}; do\n\
         \x20   target=\"$(printf '%*s' \"$n\" '' | tr ' ' x)\"\n\
         \x20   for prefix in kernel inode path cli; do\n\
         \x20       test \"$(readlink -- \"$prefix-$n\")\" = \"$target\"\n\
         \x20       printf '%s|%s|OK\\n' \"$prefix\" \"$n\"\n\
         \x20   done\n\
         done\n",
        limit - 1,
        limit
    );
    let readback = guest_kernel_write(&img, &script);
    assert!(
        readback.status.success(),
        "kernel readback: {}",
        stderr(&readback)
    );
    let expected: String = [limit - 1, limit]
        .into_iter()
        .flat_map(|n| ["kernel", "inode", "path", "cli"].map(|p| format!("{p}|{n}|OK\n")))
        .collect();
    assert_eq!(stdout(&readback), expected);
}

#[test]
fn symlink_target_limit_matches_kernel_on_1k_blocks() {
    compare_with_kernel(1024);
}

#[test]
fn symlink_target_limit_matches_kernel_on_4k_blocks() {
    compare_with_kernel(4096);
}
