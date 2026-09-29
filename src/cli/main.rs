//! `rust-fs-ext4`: the command-line tools for ext4, one multi-call binary.
//!
//! Installed as `rust-fs-ext4` and linked as each dotted name; see
//! `common` for the dispatch and the output contract every tool shares,
//! and `ext4` for the tools themselves.

// The shared plumbing is a library in waiting (see its module docs): its
// API is whole, and a piece ext4 does not call yet is not dead, it is the
// part another driver's tools will.
#[allow(dead_code)]
mod common;
mod ext4;

use std::process::ExitCode;

static FAMILY: common::Family = common::Family {
    repo: "rust-fs-ext4",
    crate_name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    about: "ext4 tools: work on an ext4 image or device directly, without mounting it",
    install_hints: &[
        "`chore cli:install` from a checkout of this repository",
        "`brew install antimatter-studios/tap/rust-fs-ext4`",
    ],
    tools: &[ext4::mkfs::TOOL],
};

fn main() -> ExitCode {
    common::main(&FAMILY)
}
