//! A kernel oracle call stopped by a signal leaves nothing behind in the
//! guest (#623).
//!
//! Every kernel test mounts a copy of its image on a loop device under
//! `/var/tmp/fs-ext4-kernel.*`. A run stopped while that mount was in use
//! left the mount, the loop device and the directory in the guest, and
//! they added up until the guest was rebooted. Each case here stops a
//! call while a process is inside its mount, and asks the guest what is
//! left once the call has ended: nothing may be.

use fs_ext4_test_support::{fixture, guest_kernel_stopped, Stopped};

fn image() -> String {
    fixture(env!("CARGO_MANIFEST_DIR"), "ext4-basic.img")
}

#[track_caller]
fn assert_nothing_left(signal: &str, who: Stopped) {
    let left = guest_kernel_stopped(&image(), signal, who);
    assert!(
        left.trim().is_empty(),
        "{signal} to the kernel oracle call ({who:?}) left this in the guest:\n{left}"
    );
}

#[test]
fn term_to_the_whole_call_leaves_no_mount_loop_device_or_directory() {
    assert_nothing_left("TERM", Stopped::Group);
}

#[test]
fn term_to_the_calls_shell_alone_leaves_no_mount_loop_device_or_directory() {
    assert_nothing_left("TERM", Stopped::ShellAlone);
}

#[test]
fn hangup_to_the_whole_call_leaves_no_mount_loop_device_or_directory() {
    assert_nothing_left("HUP", Stopped::Group);
}
