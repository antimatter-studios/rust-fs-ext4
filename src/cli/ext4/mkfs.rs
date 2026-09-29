//! `mkfs.ext4`: create a fresh ext4 filesystem.
//!
//! The formatter that shipped as the `mkfs_ext4` target, moved into the
//! multi-call binary. Every flag it accepted is still accepted, the
//! standard formatter's flags it accepts and ignores included, because
//! scripts pass them. `--size` is the shared spelling of what was
//! `--create-size`, which stays as an alias.
//!
//! What is new is the result: a JSON report on stdout, read back from the
//! superblock that was written (`--text` prints nothing there, as the tool
//! did before). The progress lines on stderr are unchanged.
//!
//! Convention follows the standard formatter: the device or file must
//! already exist at the target size, unless `--size` is given for an
//! image file that does not exist yet.

use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, Command as Cmd};

use crate::common::{CliError, Json, Outcome, Tool};
use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::mkfs::{
    format_filesystem, is_valid_block_size, DEFAULT_BLOCK_SIZE, MAX_BLOCK_SIZE, MIN_BLOCK_SIZE,
};

pub const TOOL: Tool = Tool {
    name: "mkfs.ext4",
    verb: "mkfs",
    section: 8,
    about: "Create an ext4 filesystem on a device or an image file",
    command,
    run,
};

/// The standard formatter's flags accepted and ignored that take one
/// argument, which is discarded. A flag listed here consumes the token
/// after it, so a boolean flag listed here would eat the device.
pub const IGNORED_FLAGS_WITH_ARG: &[char] = &['m', 'N', 'i', 'E', 'O', 'T'];

/// The standard formatter's flags accepted and ignored that take nothing.
/// `-c` (check for bad blocks) is why this list exists: listed as
/// argument-taking it consumed the image path.
pub const IGNORED_BOOLEAN_FLAGS: &[char] = &['c'];

/// ext4's volume label field: 16 bytes.
const MAX_LABEL_BYTES: usize = 16;

fn command() -> Cmd {
    let mut cmd = Cmd::new("mkfs.ext4")
        .about("Create an ext4 filesystem on a device or an image file")
        .long_about(
            "Create an ext4 filesystem on a device or a pre-sized image file.\n\n\
             The device or file must already exist at the target size (`truncate -s 64M \
             out.img`), unless --size is given for an image file that does not exist yet.\n\n\
             A JSON report of what was written, read back from the new superblock, goes \
             to stdout; progress goes to stderr.",
        )
        .arg(
            Arg::new("device")
                .value_name("TARGET")
                .help("Block device or image file to format")
                .required(true),
        )
        .arg(
            Arg::new("label")
                .short('L')
                .long("label")
                .value_name("LABEL")
                .help("Volume label, at most 16 bytes of UTF-8")
                .value_parser(parse_label),
        )
        .arg(
            Arg::new("block-size")
                .short('b')
                .long("block-size")
                .value_name("BYTES")
                .help(format!(
                    "Block size: a power of two, {MIN_BLOCK_SIZE}..={MAX_BLOCK_SIZE}. \
                     Default: {DEFAULT_BLOCK_SIZE}."
                ))
                .value_parser(parse_block_size),
        )
        .arg(
            Arg::new("uuid")
                .short('U')
                .long("uuid")
                .value_name("UUID")
                .help("Volume UUID, 32 hex digits, dashes optional. Default: random.")
                .value_parser(parse_uuid),
        )
        .arg(
            Arg::new("force")
                .short('F')
                .long("force")
                .help("Format even if the device looks in use (accepted; nothing is inspected yet)")
                .action(ArgAction::Count),
        )
        .arg(
            Arg::new("dry-run")
                .short('n')
                .long("dry-run")
                .help("Open the device and report, but write nothing")
                .action(ArgAction::Count),
        )
        .arg(
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .help("No progress or warnings on stderr")
                .action(ArgAction::Count),
        )
        .arg(
            Arg::new("size")
                .long("size")
                .visible_alias("create-size")
                .value_name("SIZE")
                .help(
                    "Create TARGET as an image file of SIZE bytes first, if it does not exist \
                     (K/M/G/T suffixes, 1024-based). Refused for block and character devices.",
                )
                .value_parser(parse_size),
        )
        .args(crate::common::format_args())
        .after_help(
            "Examples:\n  \
             mkfs.ext4 --size 64M --label BACKUP disk.img\n  \
             truncate -s 1G disk.img && mkfs.ext4 -b 4096 disk.img\n  \
             mkfs.ext4 -n disk.img                  open and report, write nothing\n  \
             mkfs.ext4 --text -q disk.img           print nothing on success\n\n\
             Accepted and ignored, with a warning, because scripts pass them: \
             -m -N -i -E -O -T (each takes a value), and -c.",
        );
    for &flag in IGNORED_FLAGS_WITH_ARG {
        cmd = cmd.arg(
            Arg::new(ignored_id(flag))
                .short(flag)
                .value_name("VALUE")
                .hide(true)
                .action(ArgAction::Append)
                .allow_hyphen_values(true),
        );
    }
    for &flag in IGNORED_BOOLEAN_FLAGS {
        cmd = cmd.arg(
            Arg::new(ignored_id(flag))
                .short(flag)
                .hide(true)
                .action(ArgAction::Count),
        );
    }
    cmd
}

/// The clap id of an ignored flag. clap ids are static strings, and there
/// are seven of these, so a table beats building one per run.
fn ignored_id(flag: char) -> &'static str {
    match flag {
        'm' => "ignored-m",
        'N' => "ignored-N",
        'i' => "ignored-i",
        'E' => "ignored-E",
        'O' => "ignored-O",
        'T' => "ignored-T",
        'c' => "ignored-c",
        other => unreachable!("-{other} is not an ignored flag"),
    }
}

/// Everything the command line said, once it has all been read.
#[derive(Debug)]
struct Opts {
    device: String,
    label: Option<String>,
    block_size: Option<u32>,
    uuid: Option<[u8; 16]>,
    dry_run: bool,
    quiet: bool,
    create_size: Option<u64>,
    /// Warnings about ignored flags, in command-line order. Printed after
    /// the parse, so `-q` anywhere silences all of them.
    warnings: Vec<String>,
}

fn opts(matches: &ArgMatches) -> Opts {
    let mut warnings: Vec<(usize, String)> = Vec::new();
    for &flag in IGNORED_FLAGS_WITH_ARG {
        let id = ignored_id(flag);
        if let (Some(values), Some(indices)) =
            (matches.get_many::<String>(id), matches.indices_of(id))
        {
            for (value, index) in values.zip(indices) {
                warnings.push((index, format!("-{flag} {value} not yet honored, ignoring")));
            }
        }
    }
    for &flag in IGNORED_BOOLEAN_FLAGS {
        let id = ignored_id(flag);
        // A count flag always has a value (0), and so an index: only one
        // the command line actually gave is a warning.
        if matches.value_source(id) != Some(clap::parser::ValueSource::CommandLine) {
            continue;
        }
        if let Some(indices) = matches.indices_of(id) {
            for index in indices {
                warnings.push((index, format!("-{flag} not yet honored, ignoring")));
            }
        }
    }
    warnings.sort_by_key(|(index, _)| *index);
    Opts {
        device: matches
            .get_one::<String>("device")
            .cloned()
            .expect("clap requires the device"),
        label: matches.get_one::<String>("label").cloned(),
        block_size: matches.get_one::<u32>("block-size").copied(),
        uuid: matches.get_one::<[u8; 16]>("uuid").copied(),
        dry_run: matches.get_count("dry-run") > 0,
        quiet: matches.get_count("quiet") > 0,
        create_size: matches.get_one::<u64>("size").copied(),
        warnings: warnings.into_iter().map(|(_, w)| w).collect(),
    }
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let opts = opts(matches);
    let say = |line: String| {
        if !opts.quiet {
            eprintln!("mkfs.ext4: {line}");
        }
    };
    for warning in &opts.warnings {
        say(format!("warning: {warning}"));
    }
    let device = opts.device.as_str();
    let block_size = opts.block_size.unwrap_or(DEFAULT_BLOCK_SIZE);

    // --size: (a) an existing regular file is left alone, so the same
    // command run twice is idempotent; (b) a block or character device is
    // refused, because "make me a file" applied to /dev/diskN hides a
    // typo; (c) a path that does not exist is created at that size -- except
    // under --dry-run, which writes nothing: the file is not created and the
    // report describes the size it would have had.
    let mut would_create: Option<u64> = None;
    if let Some(n) = opts.create_size {
        match std::fs::metadata(device) {
            Ok(meta) => {
                let ft = meta.file_type();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    if ft.is_block_device() || ft.is_char_device() {
                        return Err(CliError::failed(format!(
                            "--create-size refuses to apply to {device}: looks like a real \
                             block/char device, not a regular file. Did you mean to leave \
                             --create-size off?"
                        )));
                    }
                }
                if !ft.is_file() {
                    return Err(CliError::failed(format!(
                        "--create-size: {device} exists but is not a regular file"
                    )));
                }
                say(format!(
                    "--create-size: {device} already exists ({} bytes); leaving as-is",
                    meta.len()
                ));
            }
            Err(_) if opts.dry_run => {
                say(format!("--create-size: would create {device} ({n} bytes)"));
                would_create = Some(n);
            }
            Err(_) => {
                let f = std::fs::File::create(device).map_err(|e| {
                    CliError::failed(format!("--create-size: create {device}: {e}"))
                })?;
                f.set_len(n).map_err(|e| {
                    CliError::failed(format!("--create-size: set_len({n}) on {device}: {e}"))
                })?;
                drop(f);
                say(format!("--create-size: created {device} ({n} bytes)"));
            }
        }
    }

    // Read-write first: it fails fast on permission, and it learns the size.
    // A dry run over a target --size would create has nothing to open.
    let (dev, size) = match would_create {
        Some(n) => (None, n),
        None => {
            let dev = FileDevice::open_rw(device)
                .map_err(|e| CliError::failed(format!("open {device} read-write: {e:?}")))?;
            let size = dev.size_bytes();
            (Some(dev), size)
        }
    };
    if size == 0 {
        return Err(CliError::failed(format!(
            "device {device} reports size 0 — pre-create with truncate / fsutil first"
        )));
    }

    say(format!(
        "formatting {device} ({size} bytes, block_size={block_size}{})",
        if opts.dry_run { ", dry-run" } else { "" }
    ));

    let base = [
        ("fs", Json::from("ext4")),
        ("device", Json::from(device)),
        ("device_bytes", Json::from(size)),
        ("dry_run", Json::from(opts.dry_run)),
    ];
    if opts.dry_run {
        say("dry-run — no writes performed".to_string());
        let mut report: Vec<(&str, Json)> = base.to_vec();
        report.push(("formatted", Json::from(false)));
        report.push(("block_size", Json::from(block_size)));
        report.push(("label", Json::from(opts.label.clone())));
        return Ok(Outcome::report(Json::object(report)).with_text(String::new()));
    }

    let Some(dev) = dev else {
        unreachable!("only a dry run leaves the device unopened");
    };
    format_filesystem(&dev, opts.label.as_deref(), opts.uuid, size, block_size)
        .map_err(|e| CliError::failed(format!("format failed: {e:?}")))?;
    // Flush so the bytes reach storage before exit: `mkfs && mount` must
    // not race the page cache.
    dev.flush()
        .map_err(|e| CliError::failed(format!("flush failed: {e:?}")))?;
    say(format!("{device} formatted successfully"));

    // The report is what the superblock now SAYS, read back, not what was
    // asked for: a label or UUID that did not reach the disk shows here.
    let fs = fs_ext4::Filesystem::mount(Arc::new(dev))
        .map_err(|e| CliError::failed(format!("read back {device} after formatting: {e:?}")))?;
    let sb = &fs.sb;
    let mut report: Vec<(&str, Json)> = base.to_vec();
    report.push(("formatted", Json::from(true)));
    report.push((
        "label",
        if sb.volume_name.is_empty() {
            Json::Null
        } else {
            Json::from(sb.volume_name.as_str())
        },
    ));
    report.push(("uuid", Json::from(format_uuid(&sb.uuid))));
    report.push(("block_size", Json::from(sb.block_size())));
    report.push((
        "total_bytes",
        Json::from(sb.blocks_count * u64::from(sb.block_size())),
    ));
    report.push(("total_blocks", Json::from(sb.blocks_count)));
    report.push(("free_blocks", Json::from(sb.free_blocks_count)));
    report.push(("total_inodes", Json::from(sb.inodes_count)));
    report.push(("free_inodes", Json::from(sb.free_inodes_count)));
    // `--text` prints nothing on success, exactly as the tool always has:
    // its progress is on stderr, and a script checks the status.
    Ok(Outcome::report(Json::object(report)).with_text(String::new()))
}

/// The UUID in its standard 8-4-4-4-12 form.
pub fn format_uuid(u: &[u8; 16]) -> String {
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn parse_label(v: &str) -> Result<String, String> {
    if v.len() > MAX_LABEL_BYTES {
        return Err(format!(
            "label too long ({} bytes); ext4 max is {MAX_LABEL_BYTES} bytes UTF-8",
            v.len()
        ));
    }
    Ok(v.to_string())
}

/// Refused here, at parse time: the formatter's own check fires only
/// after the device is open read-write and "formatting" has been printed,
/// so a bad `-b` read as a format that failed halfway (#180's neighbour).
fn parse_block_size(v: &str) -> Result<u32, String> {
    let n: u32 = v.parse().map_err(|_| format!("not a valid number: {v}"))?;
    if !is_valid_block_size(n) {
        return Err(format!(
            "block size must be a power of two in {MIN_BLOCK_SIZE}..={MAX_BLOCK_SIZE}, got {n}"
        ));
    }
    Ok(n)
}

/// A size like `64M`, `1G`, `1024K` or `33554432`: 1024-based K/M/G/T,
/// either case, an optional trailing `B`. A bare number is bytes, as for
/// `truncate -s`.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("empty size argument".to_string());
    }
    let s = trimmed.strip_suffix(['B', 'b']).unwrap_or(trimmed);
    let (num, mult): (&str, u64) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        Some('T' | 't') => (&s[..s.len() - 1], 1 << 40),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => return Err(format!("unrecognised size suffix in {s:?}")),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("not a valid number: {num:?}"))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("{s} overflows u64"))
}

/// A UUID in its text form, with or without dashes.
fn parse_uuid(s: &str) -> Result<[u8; 16], String> {
    let cleaned: String = s.chars().filter(|c| *c != '-').collect();
    if cleaned.len() != 32 {
        return Err(format!(
            "UUID must be 32 hex chars (with optional dashes), got {} chars",
            cleaned.len()
        ));
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("UUID has non-hex character near position {}", i * 2))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Opts, clap::Error> {
        let mut full = vec!["mkfs.ext4"];
        full.extend_from_slice(argv);
        command().try_get_matches_from(full).map(|m| opts(&m))
    }

    /// The help states the default the formatter uses, from the constant.
    #[test]
    fn the_help_states_the_default_block_size_from_the_constant() {
        let help = command().render_long_help().to_string();
        assert!(
            help.contains(&format!("Default: {DEFAULT_BLOCK_SIZE}.")),
            "{help}"
        );
    }

    /// `-c` is boolean in the standard formatter. Listed as argument-taking,
    /// it consumed the device path.
    #[test]
    fn dash_c_is_boolean_and_leaves_the_device_alone() {
        let opts = parse(&["-c", "/x/disk.img"]).expect("parse");
        assert_eq!(opts.device, "/x/disk.img");
        assert_eq!(opts.warnings, vec!["-c not yet honored, ignoring"]);
    }

    /// Flags that take an argument still swallow it, or their value would
    /// become the device path.
    #[test]
    fn argument_taking_ignored_flags_consume_their_value() {
        for &flag in IGNORED_FLAGS_WITH_ARG {
            let dash = format!("-{flag}");
            let opts = parse(&[&dash, "1", "/x/disk.img"]).expect("parse");
            assert_eq!(opts.device, "/x/disk.img", "{dash} ate the device");
            assert_eq!(
                opts.warnings,
                vec![format!("{dash} 1 not yet honored, ignoring")]
            );
        }
    }

    #[test]
    fn argument_taking_ignored_flag_without_a_value_is_an_error() {
        assert!(parse(&["-m"]).is_err());
    }

    #[test]
    fn out_of_range_block_sizes_are_rejected_at_parse_time() {
        for bad in ["3000", "512", "131072", "0"] {
            let err = parse(&["-b", bad, "/x/disk.img"])
                .expect_err(bad)
                .to_string();
            assert!(
                err.contains("block size must be a power of two"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn in_range_block_sizes_parse() {
        for good in ["1024", "4096", "65536"] {
            let opts = parse(&["-b", good, "/x/disk.img"]).expect("parse");
            assert_eq!(opts.block_size, Some(good.parse().unwrap()));
        }
    }

    /// `-q` anywhere silences every warning: the warnings are collected
    /// in command-line order and the same whichever end `-q` is at.
    #[test]
    fn quiet_is_independent_of_flag_order() {
        let early = parse(&["-q", "-m", "1", "/x/disk.img"]).expect("parse");
        let late = parse(&["-m", "1", "-q", "/x/disk.img"]).expect("parse");
        assert!(early.quiet && late.quiet);
        assert_eq!(early.warnings, late.warnings);
    }

    #[test]
    fn warnings_keep_command_line_order_across_flags() {
        let opts = parse(&["-T", "small", "-c", "-m", "0", "/x/disk.img"]).expect("parse");
        assert_eq!(
            opts.warnings,
            vec![
                "-T small not yet honored, ignoring",
                "-c not yet honored, ignoring",
                "-m 0 not yet honored, ignoring",
            ]
        );
    }

    #[test]
    fn unknown_flags_are_still_rejected() {
        assert!(parse(&["-Z", "/x/disk.img"]).is_err());
    }

    #[test]
    fn size_is_the_shared_spelling_and_create_size_still_works() {
        for flag in ["--size", "--create-size"] {
            let opts = parse(&[flag, "64M", "/x/disk.img"]).expect("parse");
            assert_eq!(opts.create_size, Some(64 << 20), "{flag}");
        }
    }

    #[test]
    fn a_second_device_is_refused() {
        assert!(parse(&["/x/a.img", "/x/b.img"]).is_err());
    }

    #[test]
    fn long_labels_are_refused() {
        assert!(parse(&["-L", "seventeen-bytes-x", "/x/disk.img"]).is_err());
        assert!(parse(&["-L", "sixteen-bytes-xx", "/x/disk.img"]).is_ok());
    }

    #[test]
    fn repeated_flags_are_accepted_as_the_standard_formatter_accepts_them() {
        assert!(parse(&["-F", "-F", "-q", "-q", "/x/disk.img"]).is_ok());
    }
}
