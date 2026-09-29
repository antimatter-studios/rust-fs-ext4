//! The ext4 tools: what this repository fills the shared contract in
//! with. Everything filesystem-specific lives here, and nothing here is
//! plumbing (that is `common`).

pub mod device;
pub mod fs;
pub mod fsck;
pub mod mkfs;
