## Checking

| Damage-tolerant reads for recovery (`fs_ext4::salvage`): every superblock copy, every copy of the group descriptor tables and inodes, each with its checksum verdict, and a damage report; geometry from a backup when the primary is unusable | Experimental | Unreleased (#646) | | `salvage_structures_oracle.rs`, `salvage::structures` unit tests |
