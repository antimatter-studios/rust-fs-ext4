//! Case-folded directory lookup (E12, Phase 5).
//!
//! A volume with `INCOMPAT_CASEFOLD` records its encoding in `s_encoding`,
//! and a directory with `EXT4_CASEFOLD_FL` compares names case-insensitively.
//!
//! ### Hashing
//!
//! A casefolded directory that is not encrypted hashes a name with **its
//! ordinary htree hash** (the version in `dx_root`, adjusted by
//! `EXT2_FLAGS_UNSIGNED_HASH`, keyed by `s_hash_seed`), computed over the
//! **folded** name rather than the raw bytes (#438). `debugfs dx_hash -c -e
//! utf8` shows it: the casefolded hash of `HELLO` is the ordinary hash of
//! `hello`, of `Straße` the ordinary hash of `strasse`, for every hash
//! version 0-5; `tests/casefold_hash_differential.rs` checks that against
//! debugfs. Hash version 6 (SipHash) is only for directories that are both
//! encrypted and casefolded, whose entries carry their hash because it
//! needs the encryption key (format documentation, "Hash Tree Directories"
//! and `ext4_extended_dir_entry_2`); nothing here recomputes it.
//!
//! ### Folding
//!
//! A name is compared in a normalised, case-folded form, using the Unicode
//! version the volume declares. We compute that form with:
//!
//! 1. Decode UTF-8 → codepoints (invalid sequences fall back to ASCII fold).
//! 2. NFD-decompose with the `unicode-normalization` crate.
//! 3. Apply Unicode full case fold with the `caseless` crate
//!    (`caseless::default_case_fold_str`). `char::to_lowercase()` is not
//!    used — it is simple lowercase, not full case fold (e.g. ß → "ss").
//! 4. Re-encode as UTF-8. The result is what the hash is computed over.
//!
//! This matches the kernel for the overwhelming majority of real filenames.
//! The kernel uses frozen tables for a specific Unicode version
//! (`s_encoding_flags`), so there can be differences for codepoints added
//! after that version; those are edge cases in practice.

use unicode_normalization::UnicodeNormalization;

use crate::hash::{name_hash, HashVersion, NameHash};

/// Produce the NFD + case-folded form of `name`, which the htree hash of a
/// casefolded directory is computed over.
///
/// The folding (Unicode Standard Annex #15 for NFD, `CaseFolding.txt` for
/// the fold):
/// 1. NFD-decompose using the `unicode-normalization` crate.
/// 2. Apply Unicode full case fold to each NFD codepoint using `caseless`.
///    `char::to_lowercase()` is NOT used because it doesn't implement case
///    fold (e.g. ß → "ß" via lowercase, but ß → "ss" via case fold).
/// 3. Re-encode as UTF-8. The result is what the directory hash hashes.
///
/// Invalid UTF-8 falls back to byte-level ASCII fold so lookup is still
/// deterministic and doesn't panic (corrupt images should not produce this).
pub fn fold_name(name: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(name) {
        Ok(s) => {
            // NFD first, then full Unicode case fold.
            let nfd: String = s.nfd().collect();
            caseless::default_case_fold_str(&nfd).into_bytes()
        }
        Err(_) => {
            // Invalid UTF-8: ASCII-only fold so lookup is still deterministic.
            name.iter()
                .map(|&b| {
                    if (0x41..=0x5A).contains(&b) {
                        b + 0x20
                    } else {
                        b
                    }
                })
                .collect()
        }
    }
}

/// The htree hash of `name` in a casefolded directory that is not
/// encrypted: the directory's ordinary hash `version` (see
/// [`crate::hash::effective_version`]) under `seed`, the superblock's
/// `s_hash_seed`, computed over [`fold_name`] of `name`.
pub fn casefold_name_hash(name: &[u8], version: HashVersion, seed: &[u32; 4]) -> NameHash {
    name_hash(&fold_name(name), version, seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_ascii_uppercase() {
        assert_eq!(fold_name(b"HELLO"), b"hello".to_vec());
        assert_eq!(fold_name(b"Hello"), b"hello".to_vec());
    }

    #[test]
    fn fold_unicode_sharp_s() {
        // 'ß' (U+00DF, UTF-8 0xC3 0x9F) folds to "ss" in full Unicode case fold.
        assert_eq!(fold_name(&[0xC3, 0x9F]), b"ss".to_vec());
    }

    #[test]
    fn fold_unicode_latin_upper() {
        // 'Ñ' (U+00D1) NFD = N + U+0303. Case fold of N = n; combining tilde unchanged.
        // The result is "n\u{0303}" (NFD form), same as folding 'ñ' (U+00F1).
        let folded_upper = fold_name("Ñ".as_bytes());
        let folded_lower = fold_name("ñ".as_bytes());
        assert_eq!(
            folded_upper, folded_lower,
            "Ñ and ñ must produce the same hash input"
        );
    }

    #[test]
    fn fold_invalid_utf8_ascii_folds() {
        // Invalid UTF-8 falls back to byte-level ASCII fold.
        assert_eq!(fold_name(&[0xFF, 0x41, 0x42]), vec![0xFF, 0x61, 0x62]);
    }

    #[test]
    fn fold_empty_name() {
        assert_eq!(fold_name(b""), Vec::<u8>::new());
    }

    /// `s_hash_seed` 01234567-89ab-cdef-0123-456789abcdef as the four
    /// little-endian words the superblock stores.
    const DEBUGFS_SEED: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x6745_2301, 0xefcd_ab89];

    /// `debugfs -R 'dx_hash -s 01234567-89ab-cdef-0123-456789abcdef -h
    /// <version> -c -e utf8 -- <name>'` (e2fsprogs 1.47.4, encoding
    /// utf8-12.1): (version, name, major, minor). The names cover ASCII
    /// case, ß folding to two letters, a precomposed accent, a dotted
    /// capital I, a ligature and a titlecase digraph.
    const DEBUGFS_CASEFOLD: &[(u8, &str, u32, u32)] = &[
        (0, "HELLO", 0x32252546, 0x00000000),
        (0, "Straße", 0xad2ed8da, 0x00000000),
        (0, "Café", 0xb57af1a2, 0x00000000),
        (0, "İstanbul", 0x7993d1e8, 0x00000000),
        (0, "ﬁle", 0x10c30fca, 0x00000000),
        (0, "ǅ", 0xf3cbc72e, 0x00000000),
        (1, "HELLO", 0xa26e4a80, 0x97e5b7f7),
        (1, "Straße", 0xfd935386, 0x310eec3f),
        (1, "Café", 0xb4f033e6, 0xc53b7dc9),
        (1, "İstanbul", 0xea3181a8, 0x95cdbb2d),
        (1, "ﬁle", 0x2fa08550, 0xa2347c70),
        (1, "ǅ", 0xecb9513e, 0x53db85db),
        (2, "HELLO", 0x6f5bb1a8, 0x231917c2),
        (2, "Straße", 0xd6e5379a, 0x7d22f805),
        (2, "Café", 0x65e6583e, 0x6e22bfb6),
        (2, "İstanbul", 0xea09454c, 0x000466d6),
        (2, "ﬁle", 0x53fcf74e, 0x15e4b547),
        (2, "ǅ", 0x60e7aab0, 0xddc780d6),
        (3, "HELLO", 0x32252546, 0x00000000),
        (3, "Straße", 0xad2ed8da, 0x00000000),
        (3, "Café", 0x710181a6, 0x00000000),
        (3, "İstanbul", 0x32e89dbc, 0x00000000),
        (3, "ﬁle", 0x10c30fca, 0x00000000),
        (3, "ǅ", 0xb34beb2c, 0x00000000),
        (4, "HELLO", 0xa26e4a80, 0x97e5b7f7),
        (4, "Straße", 0xfd935386, 0x310eec3f),
        (4, "Café", 0xb5763ba0, 0x9b8175dc),
        (4, "İstanbul", 0x1c5d5e6e, 0x36372989),
        (4, "ﬁle", 0x2fa08550, 0xa2347c70),
        (4, "ǅ", 0xd79841e0, 0xc3767231),
        (5, "HELLO", 0x6f5bb1a8, 0x231917c2),
        (5, "Straße", 0xd6e5379a, 0x7d22f805),
        (5, "Café", 0xc1ea8e5e, 0xf3ae206f),
        (5, "İstanbul", 0x6be30f56, 0x0d8d60e7),
        (5, "ﬁle", 0x53fcf74e, 0x15e4b547),
        (5, "ǅ", 0x1d244eee, 0xc7690204),
    ];

    /// A casefolded directory hashes the folded name with its ordinary
    /// htree hash, not SipHash (#438).
    #[test]
    fn casefold_hash_matches_debugfs_for_every_hash_version() {
        for &(version, name, major, minor) in DEBUGFS_CASEFOLD {
            let v = HashVersion::from_u8(version).unwrap();
            let h = casefold_name_hash(name.as_bytes(), v, &DEBUGFS_SEED);
            assert_eq!(
                (h.major, h.minor),
                (major, minor),
                "v{version} {name:?}: got ({:#010x}, {:#010x})",
                h.major,
                h.minor
            );
        }
    }

    /// Names that differ only in case hash alike under every version.
    #[test]
    fn casefold_hash_is_case_insensitive() {
        let seed = [1u32, 2, 3, 4];
        for version in 0..=5 {
            let v = HashVersion::from_u8(version).unwrap();
            let a = casefold_name_hash(b"README", v, &seed);
            let b = casefold_name_hash(b"readme", v, &seed);
            let c = casefold_name_hash(b"ReadMe", v, &seed);
            assert_eq!(a, b, "v{version}");
            assert_eq!(a, c, "v{version}");
        }
    }

    #[test]
    fn casefold_hash_differs_on_different_names() {
        let seed = [1u32, 2, 3, 4];
        let a = casefold_name_hash(b"hello", HashVersion::HalfMd4, &seed);
        let b = casefold_name_hash(b"hellw", HashVersion::HalfMd4, &seed);
        assert_ne!(a.major, b.major);
    }

    #[test]
    fn casefold_low_bit_is_zero() {
        let seed = [0xdeadbeefu32, 0, 0, 0];
        for version in 0..=5 {
            let v = HashVersion::from_u8(version).unwrap();
            for name in [b"foo".as_slice(), b"BAR", b"mixedCase"] {
                let h = casefold_name_hash(name, v, &seed);
                assert_eq!(h.major & 1, 0);
            }
        }
    }
}
