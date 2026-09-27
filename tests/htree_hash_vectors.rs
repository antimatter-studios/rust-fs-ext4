//! The htree hash agrees with e2fsprogs, byte for byte.
//!
//! Every value below was produced by `debugfs -R "dx_hash -h <version> -s
//! <seed> <name>"` (e2fsprogs 1.47.0), which computes the hash with the same
//! `ext2fs_dirhash` code `mke2fs` and `e2fsck -D` use to build an index. The
//! old tests only compared this crate's hash to itself, so an implementation
//! that packed bytes in the wrong order and used the wrong legacy hash passed
//! all of them (#96).
//!
//! The names cover each boundary the algorithm has: a partial word, exactly
//! one TEA block (16 bytes) and one half_md4 block (32 bytes), one byte past
//! each, several blocks, bytes >= 0x80, where the signed and unsigned
//! variants part ways, and the empty name, which runs none of the rounds
//! and returns the seeded state as it stands (#331). Two seeds: all-zero (the default constants) and the
//! seed of `test-disks/ext4-htree.img`. The `live_debugfs_agrees` test
//! re-derives the table when `debugfs` is installed, so it cannot drift.

use fs_ext4::hash::{name_hash, HashVersion};

const ZERO_SEED: [u32; 4] = [0, 0, 0, 0];
/// `aa8a555b-f111-dd3a-e021-9aa07e8373d7` read as four little-endian words,
/// the way both the kernel and `Superblock::parse` read `s_hash_seed`.
const FIXTURE_SEED: [u32; 4] = [0x5B558AAA, 0x3ADD11F1, 0xA09A21E0, 0xD773837E];
const FIXTURE_SEED_UUID: &str = "aa8a555b-f111-dd3a-e021-9aa07e8373d7";

/// (seed, hash version, name, major, minor)
const VECTORS: &[(&str, u8, &[u8], u32, u32)] = &[
    ("zero", 0, b"a", 0xE74B53E2, 0x00000000),
    ("zero", 0, b"abcd", 0xFAFA23CA, 0x00000000),
    ("zero", 0, b"abcde", 0x2297902C, 0x00000000),
    ("zero", 0, b"document.txt", 0x976C8B3A, 0x00000000),
    ("zero", 0, b"sixteen_bytes_ok", 0x7ABBBB02, 0x00000000),
    ("zero", 0, b"seventeen_bytes_x", 0x4294E10A, 0x00000000),
    ("zero", 0, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x862591B4, 0x00000000),
    ("zero", 0, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0x360406FC, 0x00000000),
    ("zero", 0, b"ccccccccccccccccccccccccccccccccc", 0xC67A2FC4, 0x00000000),
    ("zero", 0, b"forty_byte_name_forty_byte_name_forty_bx", 0xA1038B36, 0x00000000),
    ("zero", 0, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0xF985A9F6, 0x00000000),
    ("zero", 0, b"caf\xc3\xa9", 0x96CA5A2C, 0x00000000),
    ("zero", 0, b"\xff\x80\x7f", 0xB6A1B8CC, 0x00000000),
    ("zero", 0, b"file_0123", 0xC08D0086, 0x00000000),
    ("zero", 0, b"", 0x00000000, 0x00000000),
    ("zero", 1, b"a", 0xD5FA7D7A, 0xACB48187),
    ("zero", 1, b"abcd", 0xAD7557A8, 0xB1DA437C),
    ("zero", 1, b"abcde", 0x5821840E, 0x1CE8A82C),
    ("zero", 1, b"document.txt", 0xAEC4DABA, 0xE3CC0BB9),
    ("zero", 1, b"sixteen_bytes_ok", 0xC84764E0, 0xF5A5AA7F),
    ("zero", 1, b"seventeen_bytes_x", 0x8F739670, 0x7B0BFB17),
    ("zero", 1, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x93A0250C, 0xAE407DAE),
    ("zero", 1, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0x8BE19AFE, 0x3D80276E),
    ("zero", 1, b"ccccccccccccccccccccccccccccccccc", 0x3C7DAC74, 0xE7A3AD67),
    ("zero", 1, b"forty_byte_name_forty_byte_name_forty_bx", 0x3A089372, 0x0E5D629D),
    ("zero", 1, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0x23AA98AC, 0x9A974DF1),
    ("zero", 1, b"caf\xc3\xa9", 0xFB9C5E5C, 0x0573E8B8),
    ("zero", 1, b"\xff\x80\x7f", 0x337FF96A, 0x4ECD4AC0),
    ("zero", 1, b"file_0123", 0x2FB451EE, 0x9E96C067),
    ("zero", 1, b"", 0x00000000, 0x00000000),
    ("zero", 2, b"a", 0x6D0EA4C0, 0xC18922DF),
    ("zero", 2, b"abcd", 0x5A24112E, 0x95442076),
    ("zero", 2, b"abcde", 0x6937ED68, 0xB66BD0F1),
    ("zero", 2, b"document.txt", 0xFB932DDA, 0x8E545387),
    ("zero", 2, b"sixteen_bytes_ok", 0x1F0A5D00, 0x18FB30A1),
    ("zero", 2, b"seventeen_bytes_x", 0xF350332E, 0xB9EECF7F),
    ("zero", 2, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x3DF9B80A, 0x1110A323),
    ("zero", 2, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0xCE9B05FA, 0x7AC0E97E),
    ("zero", 2, b"ccccccccccccccccccccccccccccccccc", 0xB75407F8, 0x23238490),
    ("zero", 2, b"forty_byte_name_forty_byte_name_forty_bx", 0x386F5A9A, 0x27A2706C),
    ("zero", 2, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0x7353AB0E, 0xCECC2718),
    ("zero", 2, b"caf\xc3\xa9", 0x105842EA, 0xFB9165CA),
    ("zero", 2, b"\xff\x80\x7f", 0x6BA38152, 0x1FED68FC),
    ("zero", 2, b"file_0123", 0x92D75966, 0xE00D31E3),
    ("zero", 2, b"", 0x00000000, 0x00000000),
    ("zero", 3, b"a", 0xE74B53E2, 0x00000000),
    ("zero", 3, b"abcd", 0xFAFA23CA, 0x00000000),
    ("zero", 3, b"abcde", 0x2297902C, 0x00000000),
    ("zero", 3, b"document.txt", 0x976C8B3A, 0x00000000),
    ("zero", 3, b"sixteen_bytes_ok", 0x7ABBBB02, 0x00000000),
    ("zero", 3, b"seventeen_bytes_x", 0x4294E10A, 0x00000000),
    ("zero", 3, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x862591B4, 0x00000000),
    ("zero", 3, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0x360406FC, 0x00000000),
    ("zero", 3, b"ccccccccccccccccccccccccccccccccc", 0xC67A2FC4, 0x00000000),
    ("zero", 3, b"forty_byte_name_forty_byte_name_forty_bx", 0xA1038B36, 0x00000000),
    ("zero", 3, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0xF985A9F6, 0x00000000),
    ("zero", 3, b"caf\xc3\xa9", 0x6DDE4230, 0x00000000),
    ("zero", 3, b"\xff\x80\x7f", 0xB32A9ECC, 0x00000000),
    ("zero", 3, b"file_0123", 0xC08D0086, 0x00000000),
    ("zero", 3, b"", 0x00000000, 0x00000000),
    ("zero", 4, b"a", 0xD5FA7D7A, 0xACB48187),
    ("zero", 4, b"abcd", 0xAD7557A8, 0xB1DA437C),
    ("zero", 4, b"abcde", 0x5821840E, 0x1CE8A82C),
    ("zero", 4, b"document.txt", 0xAEC4DABA, 0xE3CC0BB9),
    ("zero", 4, b"sixteen_bytes_ok", 0xC84764E0, 0xF5A5AA7F),
    ("zero", 4, b"seventeen_bytes_x", 0x8F739670, 0x7B0BFB17),
    ("zero", 4, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x93A0250C, 0xAE407DAE),
    ("zero", 4, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0x8BE19AFE, 0x3D80276E),
    ("zero", 4, b"ccccccccccccccccccccccccccccccccc", 0x3C7DAC74, 0xE7A3AD67),
    ("zero", 4, b"forty_byte_name_forty_byte_name_forty_bx", 0x3A089372, 0x0E5D629D),
    ("zero", 4, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0x23AA98AC, 0x9A974DF1),
    ("zero", 4, b"caf\xc3\xa9", 0x9D72AED6, 0xF6138C6A),
    ("zero", 4, b"\xff\x80\x7f", 0xF435CE8C, 0x2D0B3C11),
    ("zero", 4, b"file_0123", 0x2FB451EE, 0x9E96C067),
    ("zero", 4, b"", 0x00000000, 0x00000000),
    ("zero", 5, b"a", 0x6D0EA4C0, 0xC18922DF),
    ("zero", 5, b"abcd", 0x5A24112E, 0x95442076),
    ("zero", 5, b"abcde", 0x6937ED68, 0xB66BD0F1),
    ("zero", 5, b"document.txt", 0xFB932DDA, 0x8E545387),
    ("zero", 5, b"sixteen_bytes_ok", 0x1F0A5D00, 0x18FB30A1),
    ("zero", 5, b"seventeen_bytes_x", 0xF350332E, 0xB9EECF7F),
    ("zero", 5, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x3DF9B80A, 0x1110A323),
    ("zero", 5, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0xCE9B05FA, 0x7AC0E97E),
    ("zero", 5, b"ccccccccccccccccccccccccccccccccc", 0xB75407F8, 0x23238490),
    ("zero", 5, b"forty_byte_name_forty_byte_name_forty_bx", 0x386F5A9A, 0x27A2706C),
    ("zero", 5, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0x7353AB0E, 0xCECC2718),
    ("zero", 5, b"caf\xc3\xa9", 0x6621F032, 0xF86699C6),
    ("zero", 5, b"\xff\x80\x7f", 0x4907E268, 0xDC81D4B9),
    ("zero", 5, b"file_0123", 0x92D75966, 0xE00D31E3),
    ("zero", 5, b"", 0x00000000, 0x00000000),
    ("fixture", 0, b"a", 0xE74B53E2, 0x00000000),
    ("fixture", 0, b"abcd", 0xFAFA23CA, 0x00000000),
    ("fixture", 0, b"abcde", 0x2297902C, 0x00000000),
    ("fixture", 0, b"document.txt", 0x976C8B3A, 0x00000000),
    ("fixture", 0, b"sixteen_bytes_ok", 0x7ABBBB02, 0x00000000),
    ("fixture", 0, b"seventeen_bytes_x", 0x4294E10A, 0x00000000),
    ("fixture", 0, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x862591B4, 0x00000000),
    ("fixture", 0, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0x360406FC, 0x00000000),
    ("fixture", 0, b"ccccccccccccccccccccccccccccccccc", 0xC67A2FC4, 0x00000000),
    ("fixture", 0, b"forty_byte_name_forty_byte_name_forty_bx", 0xA1038B36, 0x00000000),
    ("fixture", 0, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0xF985A9F6, 0x00000000),
    ("fixture", 0, b"caf\xc3\xa9", 0x96CA5A2C, 0x00000000),
    ("fixture", 0, b"\xff\x80\x7f", 0xB6A1B8CC, 0x00000000),
    ("fixture", 0, b"file_0123", 0xC08D0086, 0x00000000),
    ("fixture", 0, b"", 0x00000000, 0x00000000),
    ("fixture", 1, b"a", 0xD1D4380E, 0x832F6DC9),
    ("fixture", 1, b"abcd", 0x787AA7F0, 0x830D603C),
    ("fixture", 1, b"abcde", 0x21E90E84, 0xB3C1681D),
    ("fixture", 1, b"document.txt", 0x0FD5D204, 0x3D6FCEF7),
    ("fixture", 1, b"sixteen_bytes_ok", 0x6A4AB3DC, 0x8834951F),
    ("fixture", 1, b"seventeen_bytes_x", 0x4C279658, 0xB64C0A66),
    ("fixture", 1, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x51843964, 0x0B2B4691),
    ("fixture", 1, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0x7D95B0CA, 0x5C7177D4),
    ("fixture", 1, b"ccccccccccccccccccccccccccccccccc", 0x52C5D336, 0x154A1811),
    ("fixture", 1, b"forty_byte_name_forty_byte_name_forty_bx", 0x70A38F70, 0x5E9D2F5A),
    ("fixture", 1, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0x5AF23298, 0x7CA71B80),
    ("fixture", 1, b"caf\xc3\xa9", 0xE180DCAE, 0xF6655F53),
    ("fixture", 1, b"\xff\x80\x7f", 0x53076A54, 0x71187582),
    ("fixture", 1, b"file_0123", 0xE83B032A, 0x19369104),
    ("fixture", 1, b"", 0x00000000, 0x00000000),
    ("fixture", 2, b"a", 0x52EB669E, 0x95D4D440),
    ("fixture", 2, b"abcd", 0x76D47D16, 0xFB56BE08),
    ("fixture", 2, b"abcde", 0x0FF572A8, 0xFD996CD0),
    ("fixture", 2, b"document.txt", 0x56B56F02, 0x64219D93),
    ("fixture", 2, b"sixteen_bytes_ok", 0x38967632, 0x31D8A5D1),
    ("fixture", 2, b"seventeen_bytes_x", 0x087EF02A, 0x4A138E96),
    ("fixture", 2, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x8AC9F4BA, 0xA34BDB9C),
    ("fixture", 2, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0x1C35B46C, 0x20ADB9B7),
    ("fixture", 2, b"ccccccccccccccccccccccccccccccccc", 0xC058CA6A, 0x24945D66),
    ("fixture", 2, b"forty_byte_name_forty_byte_name_forty_bx", 0xDD9BBBC4, 0x71072E7D),
    ("fixture", 2, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0x9282CEC6, 0x404E7923),
    ("fixture", 2, b"caf\xc3\xa9", 0x24B99EA6, 0x6455954A),
    ("fixture", 2, b"\xff\x80\x7f", 0x0B2BDA42, 0x67A61515),
    ("fixture", 2, b"file_0123", 0xB2F38B0E, 0x917D5BB7),
    ("fixture", 2, b"", 0x00000000, 0x00000000),
    ("fixture", 3, b"a", 0xE74B53E2, 0x00000000),
    ("fixture", 3, b"abcd", 0xFAFA23CA, 0x00000000),
    ("fixture", 3, b"abcde", 0x2297902C, 0x00000000),
    ("fixture", 3, b"document.txt", 0x976C8B3A, 0x00000000),
    ("fixture", 3, b"sixteen_bytes_ok", 0x7ABBBB02, 0x00000000),
    ("fixture", 3, b"seventeen_bytes_x", 0x4294E10A, 0x00000000),
    ("fixture", 3, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x862591B4, 0x00000000),
    ("fixture", 3, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0x360406FC, 0x00000000),
    ("fixture", 3, b"ccccccccccccccccccccccccccccccccc", 0xC67A2FC4, 0x00000000),
    ("fixture", 3, b"forty_byte_name_forty_byte_name_forty_bx", 0xA1038B36, 0x00000000),
    ("fixture", 3, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0xF985A9F6, 0x00000000),
    ("fixture", 3, b"caf\xc3\xa9", 0x6DDE4230, 0x00000000),
    ("fixture", 3, b"\xff\x80\x7f", 0xB32A9ECC, 0x00000000),
    ("fixture", 3, b"file_0123", 0xC08D0086, 0x00000000),
    ("fixture", 3, b"", 0x00000000, 0x00000000),
    ("fixture", 4, b"a", 0xD1D4380E, 0x832F6DC9),
    ("fixture", 4, b"abcd", 0x787AA7F0, 0x830D603C),
    ("fixture", 4, b"abcde", 0x21E90E84, 0xB3C1681D),
    ("fixture", 4, b"document.txt", 0x0FD5D204, 0x3D6FCEF7),
    ("fixture", 4, b"sixteen_bytes_ok", 0x6A4AB3DC, 0x8834951F),
    ("fixture", 4, b"seventeen_bytes_x", 0x4C279658, 0xB64C0A66),
    ("fixture", 4, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x51843964, 0x0B2B4691),
    ("fixture", 4, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0x7D95B0CA, 0x5C7177D4),
    ("fixture", 4, b"ccccccccccccccccccccccccccccccccc", 0x52C5D336, 0x154A1811),
    ("fixture", 4, b"forty_byte_name_forty_byte_name_forty_bx", 0x70A38F70, 0x5E9D2F5A),
    ("fixture", 4, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0x5AF23298, 0x7CA71B80),
    ("fixture", 4, b"caf\xc3\xa9", 0x393BD250, 0x38A5D475),
    ("fixture", 4, b"\xff\x80\x7f", 0x41856E60, 0x410EA688),
    ("fixture", 4, b"file_0123", 0xE83B032A, 0x19369104),
    ("fixture", 4, b"", 0x00000000, 0x00000000),
    ("fixture", 5, b"a", 0x52EB669E, 0x95D4D440),
    ("fixture", 5, b"abcd", 0x76D47D16, 0xFB56BE08),
    ("fixture", 5, b"abcde", 0x0FF572A8, 0xFD996CD0),
    ("fixture", 5, b"document.txt", 0x56B56F02, 0x64219D93),
    ("fixture", 5, b"sixteen_bytes_ok", 0x38967632, 0x31D8A5D1),
    ("fixture", 5, b"seventeen_bytes_x", 0x087EF02A, 0x4A138E96),
    ("fixture", 5, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0x8AC9F4BA, 0xA34BDB9C),
    ("fixture", 5, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0x1C35B46C, 0x20ADB9B7),
    ("fixture", 5, b"ccccccccccccccccccccccccccccccccc", 0xC058CA6A, 0x24945D66),
    ("fixture", 5, b"forty_byte_name_forty_byte_name_forty_bx", 0xDD9BBBC4, 0x71072E7D),
    ("fixture", 5, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", 0x9282CEC6, 0x404E7923),
    ("fixture", 5, b"caf\xc3\xa9", 0xE88D26D0, 0xD5135EBB),
    ("fixture", 5, b"\xff\x80\x7f", 0xAFC9ECB2, 0x36EAC684),
    ("fixture", 5, b"file_0123", 0xB2F38B0E, 0x917D5BB7),
    ("fixture", 5, b"", 0x00000000, 0x00000000),
];

fn seed(name: &str) -> [u32; 4] {
    match name {
        "zero" => ZERO_SEED,
        "fixture" => FIXTURE_SEED,
        other => panic!("unknown seed {other}"),
    }
}

#[test]
fn every_version_matches_e2fsprogs() {
    let mut wrong = Vec::new();
    for &(seed_name, version, name, major, minor) in VECTORS {
        let v = HashVersion::from_u8(version).unwrap();
        let got = name_hash(name, v, &seed(seed_name));
        if (got.major, got.minor) != (major, minor) {
            wrong.push(format!(
                "{seed_name} v{version} {:?}: got ({:#010x}, {:#010x}) want ({major:#010x}, {minor:#010x})",
                String::from_utf8_lossy(name),
                got.major,
                got.minor
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} vectors differ:\n{}",
        wrong.len(),
        VECTORS.len(),
        wrong.join("\n")
    );
}

/// Re-derive every vector from `debugfs` itself, so the table above can only
/// be regenerated, not hand-edited into agreement.
// debugfs is an e2fsprogs tool, and the name is passed as raw bytes.
#[cfg(unix)]
#[test]
fn live_debugfs_agrees() {
    let debugfs = "debugfs";
    let mut wrong = Vec::new();
    for &(seed_name, version, name, major, minor) in VECTORS {
        let uuid = if seed_name == "zero" {
            "00000000-0000-0000-0000-000000000000"
        } else {
            FIXTURE_SEED_UUID
        };
        let mut request = format!("dx_hash -h {version} -s {uuid} \"").into_bytes();
        request.extend_from_slice(name);
        request.push(b'"');
        use std::os::unix::ffi::OsStringExt;
        let out = fs_ext4_test_support::oracle(debugfs)
            .arg("-R")
            .arg(std::ffi::OsString::from_vec(request))
            .output();
        let text = String::from_utf8_lossy(&out.stdout);
        let want = format!("(minor {minor:#x})");
        let line = text.lines().last().unwrap_or_default();
        if !(line.contains(&format!("is {major:#x} ")) && line.ends_with(&want)) {
            wrong.push(format!(
                "{seed_name} v{version} {:?}: table ({major:#010x}, {minor:#010x}), debugfs: {line}",
                String::from_utf8_lossy(name)
            ));
        }
    }
    // Every disagreement at once, not the first: the table is regenerated
    // from this output, and one row a run is one CI run a row.
    assert!(
        wrong.is_empty(),
        "{} of {} vectors disagree with debugfs:\n{}",
        wrong.len(),
        VECTORS.len(),
        wrong.join("\n")
    );
}
