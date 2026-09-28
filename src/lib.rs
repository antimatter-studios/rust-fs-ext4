//! ext4rs — pure-Rust ext4 filesystem driver.
//!
//! Exposes a stable C ABI (`fs_ext4_*`) via [`capi`] so FFI consumers
//! (Swift/C/Go/…) can link `libfs_ext4.a` and `#include "fs_ext4.h"`.
//!
//! Architecture (reading; the write path is in [`fs`], [`alloc`],
//! [`journal_writer`] and the `*_mut` modules):
//! - [`block_io`] — abstract trait for reading device blocks
//! - [`superblock`] — parse + validate the on-disk superblock
//! - [`features`] — feature flag inventory (COMPAT/INCOMPAT/RO_COMPAT)
//! - [`bgd`] — block group descriptor parsing
//! - [`inode`] — inode + extra fields parsing
//! - [`extent`] — extent tree traversal (leaf/internal nodes, uninitialized extents)
//! - [`dir`] — directory entries (linear and HTree)
//! - [`hash`] — htree hash functions (legacy / half_md4 / tea)
//! - [`fs`] — top-level filesystem handle, file/dir lookup, read API
//! - [`capi`] — C ABI exports matching `include/fs_ext4.h`

// `doc_lazy_continuation` disagrees with the existing numbered-list
// indentation in several module preambles; the content is fine.
#![allow(clippy::doc_lazy_continuation)]

pub mod acl;
pub mod alloc;
pub mod bgd;
pub mod block_cache;
pub mod block_io;
pub mod casefold;
pub mod checksum;
pub mod dir;
pub mod ea_inode;
pub mod error;
pub mod extent;
pub mod extent_mut;
pub mod features;
pub mod file_io;
pub mod file_mut;
pub mod fs;
pub mod fs_core_bridge;
pub mod fsck;
pub mod hash;
pub mod htree;
pub mod htree_mut;
pub mod indirect;
pub mod indirect_mut;
pub mod inline_data;
pub(crate) mod inline_mut;
pub mod inode;
pub mod jbd2;
pub mod journal;
pub mod journal_apply;
pub mod journal_writer;
pub mod mkfs;
pub mod path;
pub mod runtime;
pub mod superblock;
pub mod transaction;
pub mod verify;
pub mod xattr;

// C ABI exports — surface defined in `include/fs_ext4.h`.
pub mod capi;

pub use error::{Error, Result};
pub use fs::{Filesystem, InodeRef};
pub use superblock::Superblock;

// DOES THIS BUILD ACTUALLY TRAP AN ARITHMETIC OVERFLOW?
//
// Inline in `lib.rs` rather than a module of its own under `src/`: a
// separate file hangs off one `mod` line, and losing that line leaves
// the file present, uncompiled and asserting nothing, with no lint to
// say so. Inline, there is no declaration to lose. It cannot live in
// `tests/` either -- it has to be part of the library target the debug
// step in ci.yml builds, because the question it answers is about that
// build specifically.
#[cfg(test)]
mod overflow_checks {
    /// Set by the debug step in `ci.yml`, and by nothing else.
    ///
    /// The release step must NOT set it: overflow checks are off there
    /// deliberately, because that is what ships.
    const HANDSHAKE: &str = "EXPECT_OVERFLOW_CHECKS";

    /// Perform an overflow and report whether the program was stopped.
    ///
    /// The only question that matters, and the only one a text scan of
    /// Cargo.toml or the workflow cannot answer on its own: whichever
    /// spelling of "the checks are off" might exist -- a manifest key, a
    /// `CARGO_PROFILE_*` variable, a `.cargo/config.toml` -- this asks
    /// the build directly instead of enumerating them.
    ///
    /// The panic hook is left alone. It is process-wide, and the other
    /// library tests run beside this one on other threads, so silencing
    /// it here would silence their failures too (#331). The test harness
    /// already captures what the default hook prints for this thread and
    /// shows it only if this test fails; tests/scripts/test-no-panic-hook.sh
    /// keeps `set_hook` out of `src/`.
    fn this_build_traps_an_overflow() -> bool {
        std::panic::catch_unwind(|| {
            let big = std::hint::black_box(u64::MAX);
            std::hint::black_box(big + 1);
        })
        .is_err()
    }

    /// When the gate says it built a profile that traps, check that it
    /// did.
    ///
    /// With `HANDSHAKE` unset this asserts nothing -- the shape of a
    /// test that passes because its fixture is missing -- and it is not
    /// guarded here because it cannot be: a build cannot tell whether it
    /// was supposed to be the checking one. It is guarded in
    /// `tests/ci_profile.rs`, which reads `ci.yml` and refuses if no
    /// `cargo test` there runs without `--release` while setting this
    /// variable.
    #[test]
    fn the_build_the_gate_asked_to_check_does_check() {
        let asked = match std::env::var(HANDSHAKE) {
            Ok(value) if !value.is_empty() => value,
            _ => return,
        };

        assert!(
            this_build_traps_an_overflow(),
            "{HANDSHAKE}={asked} was set, so this run is the one that is \
             supposed to panic on arithmetic overflow -- and it did not. \
             The debug step is running and blind, which is the exact \
             state it exists to rule out."
        );
    }
}
