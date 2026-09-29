//! `fsck.ext4`: check an ext4 filesystem, and repair what the library
//! knows how to repair.
//!
//! What it checks is this crate's audit (`fsck::audit_with_repair`): the
//! directory tree walked from `/`, link counts against the entries that
//! reference each inode, `..` entries, entry types against their inodes,
//! directory-block checksums, and the free counts in every group
//! descriptor and the superblock against the bitmaps. It is a subset of
//! what e2fsck checks, and says so: a volume this calls clean is one on
//! which none of THESE invariants is broken.
//!
//! EXIT STATUS IS fsck(8)'s, because scripts and the `fsck` front-end read
//! it: 0 clean, 1 errors corrected, 4 errors left uncorrected, 8 an
//! operational error (the target could not be opened or mounted), 16 a
//! wrong command line.
//!
//! Without `-y` nothing is written, as with `-n`: a check that is not
//! told it may repair does not. The report is JSON by default.

use std::ffi::OsString;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use crate::common::{CliError, Json, Outcome, Tool};
use fs_ext4::fsck::{audit_with_repair, Anomaly, AuditReport};

/// fsck(8): no errors.
pub const CLEAN: u8 = 0;
/// fsck(8): filesystem errors corrected.
pub const CORRECTED: u8 = 1;
/// fsck(8): filesystem errors left uncorrected.
pub const UNCORRECTED: u8 = 4;
/// fsck(8): operational error.
pub const OPERATIONAL: u8 = 8;
/// fsck(8): usage or syntax error.
pub const USAGE: u8 = 16;

pub const TOOL: Tool = Tool {
    name: "fsck.ext4",
    verb: "fsck",
    section: 8,
    usage_exit: USAGE,
    about: "Check an ext4 filesystem, and repair what can be repaired",
    command,
    run,
};

fn command() -> Cmd {
    Cmd::new("fsck.ext4")
        .about("Check an ext4 filesystem, and repair what can be repaired")
        .long_about(
            "Check an ext4 image or device: the directory tree, link counts, `..` \
             entries, entry types, directory-block checksums, and the free counts in the \
             group descriptors and the superblock.\n\n\
             Nothing is written unless -y (or -p) is given. The report is JSON on stdout \
             (--text for people).\n\n\
             Exit status, as fsck(8): 0 clean, 1 errors corrected, 4 errors left \
             uncorrected, 8 operational error, 16 usage error.",
        )
        .arg(
            Arg::new("device")
                .value_name("TARGET")
                .help("The image file or device to check")
                .value_parser(value_parser!(OsString))
                .required(true),
        )
        .arg(
            Arg::new("no")
                .short('n')
                .long("no")
                .help("Check only; open read-only and change nothing (the default)")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("yes")
                .short('y')
                .long("yes")
                .help("Repair everything that can be repaired")
                .action(ArgAction::SetTrue)
                .conflicts_with("no"),
        )
        .arg(
            Arg::new("preen")
                .short('p')
                .visible_short_alias('a')
                .long("preen")
                .help("Repair automatically, as -y (every repair made here is a safe one)")
                .action(ArgAction::SetTrue)
                .conflicts_with("no"),
        )
        .arg(
            Arg::new("force")
                .short('f')
                .long("force")
                .help("Check even a volume marked clean (accepted: every check is full)")
                .action(ArgAction::Count),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .help("Accepted for compatibility; the report already lists every finding")
                .action(ArgAction::Count),
        )
        .arg(
            Arg::new("offset")
                .long("offset")
                .value_name("BYTES")
                .help(
                    "Where the filesystem starts in TARGET, for a partition in a whole-disk image",
                )
                .value_parser(value_parser!(u64)),
        )
        .args(crate::common::format_args())
        .after_help(
            "Examples:\n  fsck.ext4 disk.img                 check, change nothing\n  \
             fsck.ext4 -fn disk.img             the same, as e2fsck spells it\n  \
             fsck.ext4 -y disk.img              repair what can be repaired\n  \
             fsck.ext4 --text disk.img; echo $?",
        )
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let target = matches
        .get_one::<OsString>("device")
        .expect("clap requires the device");
    let offset = matches.get_one::<u64>("offset").copied().unwrap_or(0);
    let repair = matches.get_flag("yes") || matches.get_flag("preen");
    let name = target.to_string_lossy().into_owned();

    let fs = super::device::mount(target, offset, repair).map_err(|e| e.with_code(OPERATIONAL))?;
    let dirty = super::fs::is_dirty(&fs);

    let mut findings = Vec::new();
    let mut refused = None;
    let mut result = audit_with_repair(
        &fs,
        u32::MAX,
        u32::MAX,
        |_, _, _| {},
        |a| findings.push(finding(a)),
        repair,
    );
    // A repair the library refuses before it looks (a feature it does not
    // maintain, a device it cannot write) still gets a check: say why
    // nothing was repaired, and report what is wrong.
    if repair {
        if let Err(e @ (fs_ext4::Error::Unsupported(_) | fs_ext4::Error::ReadOnly)) = &result {
            refused = Some(format!("repair refused: {e}"));
            findings.clear();
            result = audit_with_repair(
                &fs,
                u32::MAX,
                u32::MAX,
                |_, _, _| {},
                |a| findings.push(finding(a)),
                false,
            );
        }
    }

    let mode = if repair && refused.is_none() {
        "repair"
    } else {
        "check"
    };
    let report = match result {
        Ok(report) => report,
        // An I/O failure is the device, not the filesystem: operational.
        Err(e @ fs_ext4::Error::Io(_)) => {
            return Err(CliError::failed(format!("{name}: {e}")).with_code(OPERATIONAL));
        }
        // Anything else is the walk meeting a structure it cannot read:
        // an error on the volume, and not one this can correct.
        Err(e) => {
            findings.push(Json::object([
                ("kind", Json::from("unreadable")),
                ("detail", Json::from(e.to_string())),
            ]));
            let json = Json::object([
                ("fs", Json::from("ext4")),
                ("device", Json::from(name.as_str())),
                ("mode", Json::from(mode)),
                ("clean", Json::from(false)),
                ("dirty", Json::from(dirty)),
                ("exit", Json::from(u64::from(UNCORRECTED))),
                ("error", Json::from(format!("the check stopped: {e}"))),
                ("findings", Json::Arr(findings)),
            ]);
            let text = format!("fsck.ext4: {name}: the check stopped: {e}");
            return Ok(Outcome::report(json).with_text(text).with_code(UNCORRECTED));
        }
    };

    if repair && refused.is_none() {
        fs.finish().map_err(|e| {
            CliError::failed(format!("{name}: finishing the repair: {e}")).with_code(OPERATIONAL)
        })?;
    }

    let code = exit_code(&report);
    let mut fields = vec![
        ("fs", Json::from("ext4")),
        ("device", Json::from(name.as_str())),
        ("mode", Json::from(mode)),
        ("clean", Json::from(report.anomalies_count == 0)),
        ("dirty", Json::from(dirty)),
        ("exit", Json::from(u64::from(code))),
        ("found", Json::from(report.initial_anomalies_count)),
        ("repaired", Json::from(report.repaired_count)),
        ("remaining", Json::from(report.anomalies_count)),
        (
            "scanned",
            Json::object([
                ("directories", Json::from(report.directories_scanned)),
                ("entries", Json::from(report.entries_scanned)),
                ("inodes", Json::from(report.inodes_visited)),
            ]),
        ),
        ("findings", Json::Arr(findings.clone())),
    ];
    if let Some(why) = &refused {
        fields.push(("refused", Json::from(why.as_str())));
    }
    let mut text = vec![match code {
        CLEAN => format!(
            "fsck.ext4: {name}: clean ({} directories, {} entries)",
            report.directories_scanned, report.entries_scanned
        ),
        CORRECTED => format!(
            "fsck.ext4: {name}: {} problems found, all corrected",
            report.initial_anomalies_count
        ),
        _ => format!(
            "fsck.ext4: {name}: {} problems found, {} repaired, {} remaining",
            report.initial_anomalies_count, report.repaired_count, report.anomalies_count
        ),
    }];
    if let Some(why) = refused {
        text.push(format!("  {why}"));
    }
    for f in &findings {
        text.push(format!("  {}", f.to_text().replace('\n', ", ")));
    }
    Ok(Outcome::report(Json::object(fields))
        .with_text(text.join("\n"))
        .with_code(code))
}

/// fsck(8)'s status for a finished run.
fn exit_code(report: &AuditReport) -> u8 {
    if report.anomalies_count > 0 {
        UNCORRECTED
    } else if report.repaired_count > 0 || report.initial_anomalies_count > 0 {
        CORRECTED
    } else {
        CLEAN
    }
}

/// One finding as JSON: its `kind`, and the fields that locate it.
fn finding(a: &Anomaly) -> Json {
    let pairs: Vec<(&str, Json)> = match a {
        Anomaly::LinkCountTooLow {
            ino,
            stored,
            observed,
        } => vec![
            ("kind", "link_count_too_low".into()),
            ("inode", (*ino).into()),
            ("stored", (*stored).into()),
            ("observed", (*observed).into()),
        ],
        Anomaly::LinkCountTooHigh {
            ino,
            stored,
            observed,
        } => vec![
            ("kind", "link_count_too_high".into()),
            ("inode", (*ino).into()),
            ("stored", (*stored).into()),
            ("observed", (*observed).into()),
        ],
        Anomaly::DanglingEntry {
            parent_ino,
            child_ino,
            observed,
        } => vec![
            ("kind", "dangling_entry".into()),
            ("parent", (*parent_ino).into()),
            ("inode", (*child_ino).into()),
            ("observed", (*observed).into()),
        ],
        Anomaly::WrongDotDot {
            dir_ino,
            claims,
            actual_parent,
        } => vec![
            ("kind", "wrong_dotdot".into()),
            ("inode", (*dir_ino).into()),
            ("claims", (*claims).into()),
            ("parent", (*actual_parent).into()),
        ],
        Anomaly::BogusEntry {
            parent_ino,
            child_ino,
            name,
        } => vec![
            ("kind", "bogus_entry".into()),
            ("parent", (*parent_ino).into()),
            ("inode", (*child_ino).into()),
            ("name", String::from_utf8_lossy(name).into_owned().into()),
        ],
        Anomaly::BlockGroupFreeCountDrift {
            group_index,
            stored_blocks,
            observed_blocks,
            stored_inodes,
            observed_inodes,
        } => vec![
            ("kind", "group_free_count_drift".into()),
            ("group", (*group_index).into()),
            ("stored_blocks", (*stored_blocks).into()),
            ("observed_blocks", (*observed_blocks).into()),
            ("stored_inodes", (*stored_inodes).into()),
            ("observed_inodes", (*observed_inodes).into()),
        ],
        Anomaly::SuperblockFreeCountDrift {
            stored_blocks,
            observed_blocks,
            stored_inodes,
            observed_inodes,
        } => vec![
            ("kind", "superblock_free_count_drift".into()),
            ("stored_blocks", (*stored_blocks).into()),
            ("observed_blocks", (*observed_blocks).into()),
            ("stored_inodes", (*stored_inodes).into()),
            ("observed_inodes", (*observed_inodes).into()),
        ],
        Anomaly::DuplicateDirentForDirInode { ino, dirents } => vec![
            ("kind", "duplicate_directory_entry".into()),
            ("inode", (*ino).into()),
            (
                "entries",
                Json::Arr(
                    dirents
                        .iter()
                        .map(|(parent, name)| {
                            Json::object([
                                ("parent", Json::from(*parent)),
                                ("name", Json::from(name.as_str())),
                            ])
                        })
                        .collect(),
                ),
            ),
        ],
        Anomaly::DirBlockChecksumMismatch {
            dir_ino,
            logical_block,
            htree,
        } => vec![
            ("kind", "directory_block_checksum".into()),
            ("inode", (*dir_ino).into()),
            ("block", (*logical_block).into()),
            ("htree", (*htree).into()),
        ],
    };
    Json::object(pairs)
}
