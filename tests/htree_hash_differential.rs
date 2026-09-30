//! Differential test of the htree name hash against `debugfs dx_hash`.
//!
//! `htree_hash_vectors.rs` pins a hand-picked table. This file asks
//! `debugfs` about a few thousand pseudo-random names instead — every
//! length from 0 to 255, the block boundaries of both block algorithms
//! (16 bytes for TEA, 32 for half MD4) and their neighbours, bytes >= 0x80
//! (where the signed and unsigned versions part ways), several random
//! seeds plus the all-zero seed — and requires major and minor to agree
//! for all six hash versions. It also asks `dx_hash -c -e utf8` for the
//! hash a casefolded directory gives a name, and requires
//! `casefold_name_hash` to agree (#438). The requests go to one `debugfs -f -`
//! script per batch of a few hundred, so the run costs a few dozen tool
//! calls rather than one per hash.
//!
//! debugfs reads each request with its own command-line parser, so a name
//! is sent inside double quotes and may not contain `"`, `\`, NUL, CR or
//! LF (the parser cannot carry those through). The name follows `--`,
//! since a quoted name starting with `-` is still read as an option. `/` is left out too: no
//! directory entry can contain it.

use fs_ext4::casefold::casefold_name_hash;
use fs_ext4::hash::{name_hash, HashVersion};

/// A small deterministic generator (SplitMix64), so every run asks the
/// same questions and a failure can be reproduced.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// True for a byte debugfs can take inside a quoted argument.
fn sendable(byte: u8) -> bool {
    !matches!(byte, 0 | b'\n' | b'\r' | b'"' | b'\\' | b'/')
}

fn random_name(rng: &mut SplitMix, len: usize) -> Vec<u8> {
    // Half the names are drawn from printable ASCII, half from the whole
    // byte range, so both plain and high-bit names are well represented.
    let high = rng.below(2) == 0;
    let mut name = Vec::with_capacity(len);
    while name.len() < len {
        let byte = if high {
            rng.below(256) as u8
        } else {
            0x20 + rng.below(0x5F) as u8
        };
        if sendable(byte) {
            name.push(byte);
        }
    }
    name
}

/// The seed as debugfs's `-s` takes it: the 16 superblock bytes as a UUID,
/// which are the four seed words stored little-endian.
fn seed_uuid(seed: &[u32; 4]) -> String {
    let bytes: Vec<u8> = seed.iter().flat_map(|w| w.to_le_bytes()).collect();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

struct Case {
    seed: [u32; 4],
    version: u8,
    name: Vec<u8>,
    /// Ask for the hash a casefolded directory gives the name
    /// (`-c -e utf8`), not the plain one.
    casefold: bool,
}

/// Parse `Hash of <name> is 0x<major> (minor 0x<minor>)`.
fn parse_answer(line: &[u8]) -> Option<(u32, u32)> {
    let text = String::from_utf8_lossy(line);
    let at = text.rfind(" is 0x")?;
    let rest = &text[at + " is 0x".len()..];
    let (major, rest) = rest.split_once(" (minor 0x")?;
    let minor = rest.strip_suffix(')')?;
    Some((
        u32::from_str_radix(major, 16).ok()?,
        u32::from_str_radix(minor, 16).ok()?,
    ))
}

fn ask_debugfs(cases: &[Case]) -> Vec<(u32, u32)> {
    let mut script = Vec::new();
    for case in cases {
        script.extend_from_slice(
            format!(
                "dx_hash -h {} -s {}{} -- \"",
                case.version,
                seed_uuid(&case.seed),
                if case.casefold { " -c -e utf8" } else { "" }
            )
            .as_bytes(),
        );
        script.extend_from_slice(&case.name);
        script.extend_from_slice(b"\"\n");
    }
    let out = fs_ext4_test_support::oracle("debugfs")
        .arg("-f")
        .arg("-")
        .stdin(script)
        .output();
    let answers: Vec<(u32, u32)> = out
        .stdout
        .split(|&b| b == b'\n')
        .filter(|line| line.starts_with(b"Hash of "))
        .map(|line| {
            parse_answer(line).unwrap_or_else(|| {
                panic!(
                    "unreadable debugfs answer: {:?}",
                    String::from_utf8_lossy(line)
                )
            })
        })
        .collect();
    assert_eq!(
        answers.len(),
        cases.len(),
        "debugfs answered {} of {} requests; stderr:\n{}",
        answers.len(),
        cases.len(),
        String::from_utf8_lossy(&out.stderr)
    );
    answers
}

/// Requests per `debugfs` call. The script reaches the tool through a
/// file (`Oracle::stdin`), so this is not a size limit: it keeps each
/// call's script and answers small enough to read when one goes wrong.
const BATCH: usize = 400;

#[test]
fn random_names_agree_with_debugfs() {
    let mut rng = SplitMix(0x6874_7265_6568_6173);

    let mut seeds = vec![[0u32; 4]];
    for _ in 0..5 {
        seeds.push([
            rng.next() as u32,
            rng.next() as u32,
            rng.next() as u32,
            rng.next() as u32,
        ]);
    }

    // Every length once, the block boundaries several times over, and
    // then random lengths up to the 255-byte name limit.
    let boundaries = [
        0usize, 1, 3, 4, 5, 15, 16, 17, 31, 32, 33, 63, 64, 65, 254, 255,
    ];
    let mut lengths: Vec<usize> = (0..=255).collect();
    for _ in 0..8 {
        lengths.extend_from_slice(&boundaries);
    }
    while lengths.len() < 2400 {
        lengths.push(rng.below(256) as usize);
    }

    let mut cases = Vec::new();
    for (i, &len) in lengths.iter().enumerate() {
        let name = random_name(&mut rng, len);
        let seed = seeds[i % seeds.len()];
        for version in 0..=5 {
            cases.push(Case {
                seed,
                version,
                name: name.clone(),
                casefold: false,
            });
        }
    }

    let mut wrong = Vec::new();
    for batch in cases.chunks(BATCH) {
        let answers = ask_debugfs(batch);
        for (case, &(major, minor)) in batch.iter().zip(&answers) {
            let version = HashVersion::from_u8(case.version).unwrap();
            let got = name_hash(&case.name, version, &case.seed);
            let major = remap_reserved(major);
            if (got.major, got.minor) != (major, minor) {
                wrong.push(format!(
                    "v{} seed {} len {} {:02x?}: got ({:#010x}, {:#010x}) debugfs ({major:#010x}, {minor:#010x})",
                    case.version,
                    seed_uuid(&case.seed),
                    case.name.len(),
                    case.name,
                    got.major,
                    got.minor
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} hashes differ from debugfs (first 20):\n{}",
        wrong.len(),
        cases.len(),
        wrong[..wrong.len().min(20)].join("\n")
    );
    println!(
        "{} names, {} hashes checked against debugfs",
        lengths.len(),
        cases.len()
    );
}

/// The value a directory index stores for a name `debugfs dx_hash` says
/// hashes to `major`.
///
/// 0xFFFFFFFE is the index's end-of-directory marker, so a name that
/// hashes to it is stored as 0xFFFFFFFC instead (the BSD references do
/// this, and so does `name_hash`). `debugfs dx_hash` (e2fsprogs 1.47.0 and
/// 1.47.2) prints the value before that remap, so its answer for this one
/// value is translated here; every other value is compared as printed.
fn remap_reserved(major: u32) -> u32 {
    if major == 0xFFFF_FFFE {
        0xFFFF_FFFC
    } else {
        major
    }
}

/// A major hash whose value before the low bit is cleared is 0xFFFFFFFF.
/// TEA and half MD4 on the empty name report seed words directly, so the
/// reserved value is reachable on purpose: debugfs prints 0xFFFFFFFE, and
/// `name_hash` must give 0xFFFFFFFC.
#[test]
fn reserved_end_of_directory_major_is_remapped() {
    // Word 0 feeds the TEA major, word 1 the half MD4 major.
    let seed = [0xFFFF_FFFF, 0xFFFF_FFFF, 0x1234_5678, 0];
    let cases: Vec<Case> = (0..=5)
        .map(|version| Case {
            seed,
            version,
            name: Vec::new(),
            casefold: false,
        })
        .collect();
    let answers = ask_debugfs(&cases);
    let mut remapped = 0;
    for (case, &(major, minor)) in cases.iter().zip(&answers) {
        if major == 0xFFFF_FFFE {
            remapped += 1;
        }
        let got = name_hash(b"", HashVersion::from_u8(case.version).unwrap(), &seed);
        assert_eq!(
            (got.major, got.minor),
            (remap_reserved(major), minor),
            "v{}",
            case.version
        );
    }
    // debugfs really does print the reserved value for the four
    // TEA and half MD4 cases, so the remap above is exercised.
    assert_eq!(remapped, 4, "debugfs answers: {answers:x?}");
    for version in [HashVersion::Tea, HashVersion::HalfMd4Unsigned] {
        assert_eq!(name_hash(b"", version, &seed).major, 0xFFFF_FFFC);
    }
}

/// Characters for casefolded names, all assigned by Unicode 12.1 (the
/// `utf8-12.1` encoding `mke2fs -E encoding=utf8` records), so a newer
/// Unicode table in the folding crates cannot account for a difference.
/// Plain and accented Latin, Greek and Cyrillic in both cases, combining
/// accents (so decomposed and precomposed spellings meet), and the
/// characters whose full case fold is not a one-to-one lowercase: ß and
/// ẞ, the dotted and dotless i, ligatures, titlecase digraphs, Greek
/// iota subscripts, final sigma, the Kelvin, Angstrom and Ohm signs,
/// Cherokee and Georgian Mtavruli, and a Hangul syllable that
/// decomposes into jamo.
fn casefold_alphabet() -> Vec<char> {
    let mut chars: Vec<char> = ('A'..='Z').chain('a'..='z').chain('0'..='9').collect();
    chars.extend([' ', '-', '_', '.']);
    let ranges = [
        (0x00C0, 0x00FF),
        (0x0100, 0x017F),
        (0x0300, 0x0314),
        (0x0390, 0x03C9),
        (0x0400, 0x045F),
        (0x13A0, 0x13F5),
        (0x1C90, 0x1CBA),
    ];
    for (lo, hi) in ranges {
        chars.extend((lo..=hi).filter_map(char::from_u32));
    }
    chars.extend([
        '\u{1E9E}', '\u{0130}', '\u{0131}', '\u{FB01}', '\u{FB00}', '\u{FB06}', '\u{01C4}',
        '\u{01C5}', '\u{01C8}', '\u{01CB}', '\u{01F1}', '\u{01F2}', '\u{1FBC}', '\u{1FB3}',
        '\u{1F88}', '\u{212A}', '\u{212B}', '\u{2126}', '\u{0149}', '\u{01F0}', '\u{00B5}',
        '\u{0345}', '\u{0587}', '\u{AC00}', '\u{6F22}',
    ]);
    // U+03A2 is unassigned: a code point with no character is not a name
    // anyone can type, and the fold of one is not what this checks.
    chars.retain(|&c| c != '\u{03A2}');
    chars
}

/// A casefolded directory hashes the folded name with its ordinary htree
/// hash (#438). `debugfs dx_hash -c -e utf8` folds the name the way the
/// on-disk encoding defines and then hashes it, so every hash version,
/// several seeds, and names built from [`casefold_alphabet`] must give
/// the same major and minor as `casefold_name_hash`.
#[test]
fn casefolded_names_agree_with_debugfs() {
    let mut rng = SplitMix(0x6361_7365_666f_6c64);
    let alphabet = casefold_alphabet();

    let mut seeds = vec![[0u32; 4]];
    for _ in 0..3 {
        seeds.push([
            rng.next() as u32,
            rng.next() as u32,
            rng.next() as u32,
            rng.next() as u32,
        ]);
    }

    // Named cases first, so a failure on one of them reads plainly.
    let mut names: Vec<String> = [
        "HELLO",
        "hello",
        "Straße",
        "STRASSE",
        "Café",
        "CAFÉ",
        "Cafe\u{301}",
        "ReadMe",
        "İstanbul",
        "ΣΊΣΥΦΟΣ",
        "σίσυφος",
        "\u{FB01}le",
        "FILE",
        "\u{01C5}",
        "\u{1FBC}",
        "\u{212A}elvin",
        "\u{212B}ngstr\u{F6}m",
        "\u{2126}",
        "\u{0345}",
        "\u{AC00}",
        "\u{13A0}\u{13F5}",
        "\u{1C90}",
        "",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    while names.len() < 400 {
        let len = 1 + rng.below(24) as usize;
        let name: String = (0..len)
            .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
            .collect();
        names.push(name);
    }

    let mut cases = Vec::new();
    for (i, name) in names.iter().enumerate() {
        for version in 0..=5 {
            cases.push(Case {
                seed: seeds[i % seeds.len()],
                version,
                name: name.as_bytes().to_vec(),
                casefold: true,
            });
        }
    }

    let mut wrong = Vec::new();
    for batch in cases.chunks(BATCH) {
        let answers = ask_debugfs(batch);
        for (case, &(major, minor)) in batch.iter().zip(&answers) {
            let version = HashVersion::from_u8(case.version).unwrap();
            let got = casefold_name_hash(&case.name, version, &case.seed);
            if (got.major, got.minor) != (remap_reserved(major), minor) {
                wrong.push(format!(
                    "v{} seed {} {:?}: got ({:#010x}, {:#010x}) debugfs ({major:#010x}, {minor:#010x})",
                    case.version,
                    seed_uuid(&case.seed),
                    String::from_utf8_lossy(&case.name),
                    got.major,
                    got.minor
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} casefolded hashes differ from debugfs (first 20):\n{}",
        wrong.len(),
        cases.len(),
        wrong[..wrong.len().min(20)].join("\n")
    );
    println!(
        "{} names, {} casefolded hashes checked against debugfs",
        names.len(),
        cases.len()
    );
}
