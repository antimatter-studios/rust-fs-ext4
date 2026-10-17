//! `fs.ext4 <target> <verb>`: an errand inside an ext4 image or device,
//! without mounting it.
//!
//! The verbs are the shared set: `ls`, `stat`, `read`, `write`, `mkdir`,
//! `get`/`info`, `set`, `resize`. Metadata is JSON (or `--text`); file content is raw bytes.
//! A verb the library cannot do yet still exists and answers `not
//! implemented` with exit status 3, so a script moved between filesystems
//! fails loudly instead of meaning something else.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use fs_core::cli::{CliError, Format, Json, Outcome, Tool};
use fs_ext4::dir::{DirEntry, DirEntryType};
use fs_ext4::features::{Compat, Incompat, RoCompat};
use fs_ext4::inode::{Inode, InodeFlags};
use fs_ext4::Filesystem;

pub const TOOL: Tool = Tool {
    name: "fs.ext4",
    verb: "fs",
    section: 1,
    usage_exit: fs_core::cli::output::EXIT_USAGE,
    about: "List, read, write and inspect an ext4 image or device without mounting it",
    command,
    run,
};

/// The canonical keys every `fs.<fs>` answers, in the shared order.
/// Filesystem specifics are nested under `ext4`.
pub const KEYS: &[&str] = &[
    "fs",
    "label",
    "total_bytes",
    "free_bytes",
    "block_size",
    "dirty",
    "ext4",
];

fn command() -> Cmd {
    Cmd::new("fs.ext4")
        .about("List, read, write and inspect an ext4 image or device without mounting it")
        .long_about(
            "Work inside an ext4 image or device directly: no mount, no kernel driver.\n\n\
             An escape hatch for an errand (get a file out, read the label, check whether \
             it is dirty), not a place to do real filesystem work: for that, mount it.\n\n\
             Metadata is JSON on stdout (--text for people); `read` writes the file's raw \
             bytes. A failure is {\"error\": \"...\", \"code\": N} on stderr, N being the \
             exit status: 1 failed, 2 wrong command line, 3 not implemented.",
        )
        .arg(
            Arg::new("target")
                .value_name("TARGET")
                .help("The image file or device")
                .value_parser(value_parser!(OsString))
                .required(true),
        )
        .arg(
            Arg::new("offset")
                .long("offset")
                .value_name("BYTES")
                .help(
                    "Where the filesystem starts in TARGET, for a partition in a whole-disk image",
                )
                .value_parser(value_parser!(u64))
                .global(true),
        )
        .args(fs_core::cli::format_args().map(|a| a.global(true)))
        .subcommand_required(true)
        .subcommand(
            Cmd::new("ls")
                .about("List a directory: name, type, size, mode, mtime (and a symlink's target)")
                .arg(
                    Arg::new("recursive")
                        .long("recursive")
                        .short('R')
                        .action(ArgAction::SetTrue)
                        .help("Walk the tree depth first, names sorted as bytes; never follow a symlink"),
                )
                .arg(
                    Arg::new("max-depth")
                        .long("max-depth")
                        .value_name("N")
                        .value_parser(value_parser!(u64))
                        .requires("recursive")
                        .help("How deep --recursive may go (default: 64); a directory's entries are depth 1"),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .value_name("N")
                        .value_parser(value_parser!(u64))
                        .requires("recursive")
                        .help("How many entries --recursive may list (default: 100000)"),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .default_value("/")
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img ls /etc\n  \
                     fs.ext4 disk.img ls / | jq -r '.[].name'\n  \
                     fs.ext4 disk.img ls --text /\n  \
                     fs.ext4 disk.img ls -R --max-depth 8 --limit 1000 /etc\n\n\
                     With --recursive each entry adds its path from the root of the volume and \
                     its depth: PATH's own entries are depth 1, and PATH itself is not listed \
                     unless it is not a directory, when it is the one entry, at depth 0. A \
                     directory whose inode was already visited is listed again with \
                     revisited: true and not entered. If one more entry would pass either \
                     bound, or a directory cannot be read, the entries so far stay on stdout \
                     and the error names the bound or the path (exit 1). A tree that exactly \
                     fits succeeds; --limit 0 allows no entry, --max-depth 0 none below PATH.",
                ),
        )
        .subcommand(
            Cmd::new("stat")
                .about("Report one path: identity, type, mode, owner, links, sizes and every time")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img stat /etc/hostname\n  \
                     fs.ext4 disk.img stat /var/log | jq .nlink\n  \
                     fs.ext4 disk.img stat --text /bin/sh\n\n\
                     A symlink is reported, not followed. Times are seconds since the epoch \
                     plus nanoseconds; a field the inode has no room for (the nanoseconds and \
                     the creation time of a 128-byte inode) is null.",
                ),
        )
        .subcommand(
            Cmd::new("read")
                .about("Write a file's bytes to stdout, or to a file with -o")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .value_name("FILE")
                        .value_parser(value_parser!(OsString))
                        .help("Write here instead of stdout"),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img read /etc/hostname\n  \
                     fs.ext4 disk.img read /var/log/syslog | grep -i error\n  \
                     fs.ext4 disk.img read /backup.tar -o backup.tar",
                ),
        )
        .subcommand(
            Cmd::new("write")
                .about("Create or replace a file with the bytes on stdin")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img write /etc/hostname < hostname\n  \
                     tar cf - ./dir | fs.ext4 disk.img write /backup.tar\n  \
                     fs.ext4 src.img read /f | fs.ext4 dst.img write /f\n\n\
                     The parent directory must exist. An existing file is replaced.",
                ),
        )
        .subcommand(
            Cmd::new("mkdir")
                .about("Create a directory (its parent must exist)")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img mkdir /backup\n  \
                     fs.ext4 disk.img mkdir /backup/2026",
                ),
        )
        .subcommand(key_command(
            "get",
            "Report the filesystem's properties, or one of them",
        ))
        .subcommand(key_command(
            "info",
            "The same as get: every property, or one of them",
        ))
        .subcommand(
            Cmd::new("set")
                .about("Change a property: the label is the one that can be set")
                .arg(Arg::new("key").value_name("KEY").required(true))
                .arg(Arg::new("value").value_name("VALUE").required(true))
                .after_help(
                    "Examples:\n  fs.ext4 disk.img set label BACKUP\n  \
                     fs.ext4 disk.img set label ''\n\n\
                     The label is at most 16 bytes; an empty one clears it. It is written \
                     to the primary superblock and to every backup.",
                ),
        )
        .subcommand(
            Cmd::new("resize")
                .about("Grow or shrink the filesystem (not implemented)")
                .arg(Arg::new("size").value_name("SIZE").required(true))
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("Do it without asking"),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img resize 20G --force\n\n\
                     Answers `not implemented` (exit 3): the library has no resize.",
                ),
        )
        .after_help(
            "Examples:\n  fs.ext4 disk.img ls /\n  \
             fs.ext4 disk.img read /etc/fstab > fstab\n  \
             fs.ext4 disk.img write /etc/fstab < fstab\n  \
             fs.ext4 disk.img get label --text\n  \
             fs.ext4 --offset 1048576 whole-disk.img info",
        )
}

fn key_command(name: &'static str, about: &'static str) -> Cmd {
    Cmd::new(name)
        .about(about)
        .arg(
            Arg::new("key")
                .value_name("KEY")
                .help(format!("One of: {} (or ext4.<field>)", KEYS.join(", "))),
        )
        .after_help(format!(
            "Examples:\n  fs.ext4 disk.img {name}\n  \
             fs.ext4 disk.img {name} label --text\n  \
             fs.ext4 disk.img {name} ext4.uuid"
        ))
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let target = matches
        .get_one::<OsString>("target")
        .expect("clap requires the target");
    let (verb, sub) = matches.subcommand().expect("clap requires a verb");
    let offset = sub
        .get_one::<u64>("offset")
        .or_else(|| matches.get_one::<u64>("offset"))
        .copied()
        .unwrap_or(0);
    match verb {
        "ls" if sub.get_flag("recursive") => {
            let format = Format::of(matches, &std::env::args_os().collect::<Vec<_>>());
            ls_recursive(&open(target, offset)?, path_arg(sub), sub, format)
        }
        "ls" => ls(&open(target, offset)?, path_arg(sub)),
        "stat" => stat(&open(target, offset)?, path_arg(sub)),
        "read" => read(&open(target, offset)?, path_arg(sub), sub.get_one("output")),
        "write" => write(target, offset, path_arg(sub)),
        "mkdir" => mkdir(target, offset, path_arg(sub)),
        "get" | "info" => get(
            &open(target, offset)?,
            sub.get_one::<String>("key").map(String::as_str),
        ),
        "set" => set(target, offset, sub),
        "resize" => Err(CliError::not_implemented(
            "resize: this library cannot resize an ext4 filesystem",
        )),
        other => unreachable!("clap knows no verb {other}"),
    }
}

fn path_arg(sub: &ArgMatches) -> &[u8] {
    let path = sub
        .get_one::<OsString>("path")
        .expect("clap requires or defaults the path");
    os_bytes(path)
}

#[cfg(unix)]
fn os_bytes(s: &OsString) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(s: &OsString) -> &[u8] {
    s.to_str().map(str::as_bytes).unwrap_or_default()
}

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Mount `target` read-only, `offset` bytes in.
fn open(target: &OsString, offset: u64) -> Result<Filesystem, CliError> {
    super::device::mount(target, offset, false)
}

fn driver_error(what: &[u8], e: fs_ext4::Error) -> CliError {
    CliError::failed(format!("{}: {e}", show(what)))
}

fn type_name(t: DirEntryType) -> &'static str {
    match t {
        DirEntryType::RegFile => "file",
        DirEntryType::Directory => "dir",
        DirEntryType::Symlink => "symlink",
        DirEntryType::CharDev => "char",
        DirEntryType::BlockDev => "block",
        DirEntryType::Fifo => "fifo",
        DirEntryType::Socket => "socket",
        DirEntryType::Unknown => "unknown",
    }
}

fn type_char(t: DirEntryType) -> char {
    match t {
        DirEntryType::RegFile => '-',
        DirEntryType::Directory => 'd',
        DirEntryType::Symlink => 'l',
        DirEntryType::CharDev => 'c',
        DirEntryType::BlockDev => 'b',
        DirEntryType::Fifo => 'p',
        DirEntryType::Socket => 's',
        DirEntryType::Unknown => '?',
    }
}

/// One `ls` entry: the fields every `fs.<fs>` reports, typed the same way
/// everywhere — name (string), type (string), size (number), mode (octal
/// string), mtime (seconds since the epoch, number), and target (string)
/// for a symlink. A name that is not UTF-8 is shown lossily, with its
/// exact bytes in `name_hex`.
fn entry(fs: &Filesystem, name: &[u8], ino: u32, inode: &Inode) -> Json {
    let kind = DirEntryType::from_mode(inode.mode);
    let mut fields = vec![("name", Json::from(show(name)))];
    if std::str::from_utf8(name).is_err() {
        fields.push((
            "name_hex",
            Json::from(name.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.extend([
        ("type", Json::from(type_name(kind))),
        ("size", Json::from(inode.size)),
        ("mode", Json::from(format!("{:04o}", inode.mode & 0o7777))),
        ("mtime", Json::from(inode.mtime)),
        ("inode", Json::from(ino)),
    ]);
    if kind == DirEntryType::Symlink {
        fields.push((
            "target",
            Json::from(fs.read_link_ino(ino).map(|t| show(&t)).ok()),
        ));
    }
    Json::object(fields)
}

fn entry_text(e: &Json) -> String {
    let field = |k: &str| e.get(k).map(Json::to_text).unwrap_or_default();
    let kind = match e.get("type").map(Json::to_text).as_deref() {
        Some("file") => DirEntryType::RegFile,
        Some("dir") => DirEntryType::Directory,
        Some("symlink") => DirEntryType::Symlink,
        Some("char") => DirEntryType::CharDev,
        Some("block") => DirEntryType::BlockDev,
        Some("fifo") => DirEntryType::Fifo,
        Some("socket") => DirEntryType::Socket,
        _ => DirEntryType::Unknown,
    };
    let mut line = format!(
        "{}{} {:>12} {}",
        type_char(kind),
        field("mode"),
        field("size"),
        e.get("path")
            .map(Json::to_text)
            .unwrap_or_else(|| field("name"))
    );
    if let Some(target) = e.get("target") {
        line.push_str(&format!(" -> {}", target.to_text()));
    }
    if e.get("revisited") == Some(&Json::Bool(true)) {
        line.push_str(" [revisited]");
    }
    line
}

fn ls(fs: &Filesystem, path: &[u8]) -> Result<Outcome, CliError> {
    let ino = fs
        .lookup_path_bytes(path)
        .map_err(|e| driver_error(path, e))?;
    let inode = fs.stat_ino(ino).map_err(|e| driver_error(path, e))?;
    let entries = if inode.is_dir() {
        let mut listed = Vec::new();
        for d in fs.read_dir_ino(ino).map_err(|e| driver_error(path, e))? {
            if d.name == b"." || d.name == b".." {
                continue;
            }
            let child = fs.stat_ino(d.inode).map_err(|e| {
                let mut full = path.to_vec();
                if !full.ends_with(b"/") {
                    full.push(b'/');
                }
                full.extend_from_slice(&d.name);
                driver_error(&full, e)
            })?;
            listed.push(entry(fs, &d.name, d.inode, &child));
        }
        listed.sort_by(|a, b| {
            a.get("name")
                .map(Json::to_text)
                .cmp(&b.get("name").map(Json::to_text))
        });
        listed
    } else {
        let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
        vec![entry(fs, name, ino, &inode)]
    };
    let text = entries
        .iter()
        .map(entry_text)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Outcome::report(Json::Arr(entries)).with_text(text))
}

/// How deep `ls --recursive` goes, and how many entries it lists, when the
/// command line does not say.
const MAX_DEPTH: u64 = 64;
const LIMIT: u64 = 100_000;

/// One `ls --recursive` entry: the `ls` entry, then its path from the root
/// of the volume (with its exact bytes in `path_hex` when it is not UTF-8),
/// its depth, and `revisited` for a directory already listed.
fn recursive_entry(
    fs: &Filesystem,
    path: &[u8],
    depth: u64,
    ino: u32,
    inode: &Inode,
    revisited: bool,
) -> Json {
    let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
    let Json::Obj(mut fields) = entry(fs, name, ino, inode) else {
        unreachable!("an ls entry is an object")
    };
    fields.push(("path".into(), Json::from(show(path))));
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex".into(),
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.push(("depth".into(), Json::from(depth)));
    if revisited {
        fields.push(("revisited".into(), Json::from(true)));
    }
    Json::Obj(fields)
}

/// A directory's entries but `.` and `..`, sorted as bytes, so two walks
/// of one volume list the same thing in the same order.
fn sorted_children(
    fs: &Filesystem,
    ino: u32,
    path: &[u8],
) -> Result<std::vec::IntoIter<DirEntry>, CliError> {
    let mut children: Vec<DirEntry> = fs
        .read_dir_ino(ino)
        .map_err(|e| driver_error(path, e))?
        .into_iter()
        .filter(|d| d.name != b"." && d.name != b"..")
        .collect();
    children.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(children.into_iter())
}

/// Walk the tree under the inode `ino`, found at `start`, into `entries`.
/// The stack holds one directory's remaining entries per level rather
/// than a call frame, so no depth bound, however large, can overflow the
/// process's stack. A symlink is listed and never followed; a directory
/// whose inode number was already visited is listed as revisited and not
/// entered, so a damaged volume whose directories form a cycle still ends.
fn walk(
    fs: &Filesystem,
    ino: u32,
    inode: &Inode,
    start: Vec<u8>,
    (max_depth, limit): (u64, u64),
    entries: &mut Vec<Json>,
) -> Result<(), CliError> {
    let reached = |bound: &str, value: u64, at: &[u8]| {
        CliError::failed(format!("{bound} {value} reached at {}", show(at)))
    };
    if !inode.is_dir() {
        if limit == 0 {
            return Err(reached("--limit", limit, &start));
        }
        entries.push(recursive_entry(fs, &start, 0, ino, inode, false));
        return Ok(());
    }
    let mut visited = HashSet::from([ino]);
    let first = sorted_children(fs, ino, &start)?;
    let mut stack = vec![(start, 0u64, first)];
    while let Some((parent, parent_depth, children)) = stack.last_mut() {
        let Some(child) = children.next() else {
            stack.pop();
            continue;
        };
        let mut path = parent.clone();
        if !path.ends_with(b"/") {
            path.push(b'/');
        }
        path.extend_from_slice(&child.name);
        let depth = *parent_depth + 1;
        if entries.len() as u64 >= limit {
            return Err(reached("--limit", limit, &path));
        }
        if depth > max_depth {
            return Err(reached("--max-depth", max_depth, &path));
        }
        let inode = fs
            .stat_ino(child.inode)
            .map_err(|e| driver_error(&path, e))?;
        let revisited = inode.is_dir() && !visited.insert(child.inode);
        entries.push(recursive_entry(
            fs,
            &path,
            depth,
            child.inode,
            &inode,
            revisited,
        ));
        if inode.is_dir() && !revisited {
            let next = sorted_children(fs, child.inode, &path)?;
            stack.push((path, depth, next));
        }
    }
    Ok(())
}

/// `ls --recursive PATH`: the tree under PATH, within `--max-depth` and
/// `--limit`. A bound reached or a directory that cannot be read is a
/// failure, but not a silent one: the entries collected so far are printed
/// in the format asked for before the error is returned, so stdout keeps
/// a whole report of what was walked and stderr says where it stopped.
/// Nothing is printed when PATH itself cannot be found.
fn ls_recursive(
    fs: &Filesystem,
    path: &[u8],
    sub: &ArgMatches,
    format: Format,
) -> Result<Outcome, CliError> {
    let ino = fs
        .lookup_path_bytes(path)
        .map_err(|e| driver_error(path, e))?;
    let inode = fs.stat_ino(ino).map_err(|e| driver_error(path, e))?;
    let bounds = (
        sub.get_one::<u64>("max-depth")
            .copied()
            .unwrap_or(MAX_DEPTH),
        sub.get_one::<u64>("limit").copied().unwrap_or(LIMIT),
    );
    let mut start = Vec::new();
    for component in path.split(|b| *b == b'/').filter(|c| !c.is_empty()) {
        start.push(b'/');
        start.extend_from_slice(component);
    }
    if start.is_empty() {
        start.push(b'/');
    }
    let mut entries = Vec::new();
    let walked = walk(fs, ino, &inode, start, bounds, &mut entries);
    let text = entries
        .iter()
        .map(entry_text)
        .collect::<Vec<_>>()
        .join("\n");
    let report = Outcome::report(Json::Arr(entries)).with_text(text);
    match walked {
        Ok(()) => Ok(report),
        Err(error) => {
            // An outcome is a report or an error, never both: the partial
            // report goes out through the shared renderer here, and the
            // caller renders the error.
            let partial = fs_core::cli::output::render(TOOL.name, format, Ok(report));
            let mut out = std::io::stdout().lock();
            out.write_all(partial.stdout.as_bytes())
                .and_then(|()| out.flush())
                .map_err(|e| CliError::failed(format!("write: {e}")))?;
            Err(error)
        }
    }
}

/// How many bytes past the 128-byte base inode `i_extra_isize` says are
/// valid: zero for a 128-byte inode, which has none.
fn extra_isize(raw: &[u8]) -> u16 {
    if raw.len() < 0x82 {
        return 0;
    }
    u16::from_le_bytes([raw[0x80], raw[0x81]])
}

/// A device node's major and minor numbers, from the first two words of
/// `i_block`. The first word holds a pair that fits the old 8-bit
/// encoding; when it is zero, the pair is in the second word's wider one.
/// tests/cli_stat_kernel.rs pins both against nodes Linux made.
fn device_numbers(block: &[u8; 60]) -> (u32, u32) {
    let old = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
    if old != 0 {
        return ((old >> 8) & 0xff, old & 0xff);
    }
    let new = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    ((new >> 8) & 0xfff, (new & 0xff) | ((new >> 12) & 0xf_ff00))
}

/// `stat PATH`: one path's identity, type, mode, owner, links, sizes and
/// every time, the shared keys first and ext4's own under `ext4`. A
/// symlink is reported, not followed. Times are signed seconds plus
/// nanoseconds; a field the inode has no room for -- the nanoseconds and
/// the creation time, as `i_extra_isize` says -- is null, not zero.
fn stat(fs: &Filesystem, path: &[u8]) -> Result<Outcome, CliError> {
    let ino = fs
        .lookup_path_bytes(path)
        .map_err(|e| driver_error(path, e))?;
    let (inode, raw) = fs
        .read_inode_verified(ino)
        .map_err(|e| driver_error(path, e))?;
    let kind = DirEntryType::from_mode(inode.mode);
    let extra = extra_isize(&raw);
    // The extended area's fields, each present once i_extra_isize reaches
    // past it (the ext4 documentation, "Inode Table").
    let nsec = |covered: u16, value: u32| (extra >= covered).then_some(u64::from(value));
    let huge = fs.sb.feature_ro_compat & RoCompat::HUGE_FILE.bits() != 0
        && inode.flags & InodeFlags::HUGE_FILE.bits() != 0;
    let unit = if huge {
        u64::from(fs.sb.block_size())
    } else {
        512
    };

    let mut fields = vec![("path", Json::from(show(path)))];
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex",
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.extend([
        ("inode", Json::from(ino)),
        ("type", Json::from(type_name(kind))),
        ("mode", Json::from(format!("{:04o}", inode.mode & 0o7777))),
        ("uid", Json::from(inode.uid)),
        ("gid", Json::from(inode.gid)),
        ("nlink", Json::from(inode.links_count)),
        ("size", Json::from(inode.size)),
        (
            "allocated_bytes",
            Json::from(inode.blocks.saturating_mul(unit)),
        ),
        ("atime", Json::from(inode.atime)),
        ("atime_nsec", Json::from(nsec(16, inode.atime_nsec))),
        ("mtime", Json::from(inode.mtime)),
        ("mtime_nsec", Json::from(nsec(12, inode.mtime_nsec))),
        ("ctime", Json::from(inode.ctime)),
        ("ctime_nsec", Json::from(nsec(8, inode.ctime_nsec))),
        ("btime", Json::from((extra >= 20).then_some(inode.crtime))),
        ("btime_nsec", Json::from(nsec(24, inode.crtime_nsec))),
    ]);
    match kind {
        DirEntryType::Symlink => fields.push((
            "target",
            Json::from(
                fs.read_link_ino(ino)
                    .map(|t| show(&t))
                    .map_err(|e| driver_error(path, e))?,
            ),
        )),
        DirEntryType::CharDev | DirEntryType::BlockDev => {
            let (major, minor) = device_numbers(&inode.block);
            fields.push(("major", Json::from(major)));
            fields.push(("minor", Json::from(minor)));
        }
        _ => {}
    }
    fields.push((
        "ext4",
        Json::object([
            ("flags", flag_names::<InodeFlags>(inode.flags)),
            ("generation", Json::from(inode.generation)),
            ("file_acl", Json::from(inode.file_acl)),
            ("blocks", Json::from(inode.blocks)),
            ("extra_isize", Json::from(extra)),
        ]),
    ));
    Ok(Outcome::report(Json::object(fields)))
}

/// Stream a regular file's bytes. Each chunk is read before it is
/// written, so a file whose blocks turn out unreadable part-way stops with
/// status 1 and what came before stays on stdout; everything that can be
/// refused up front (no such path, a directory, a symlink) is refused
/// before a byte is written. `-o FILE` writes `FILE.partial` and renames
/// it, so FILE is never left half written.
fn read(fs: &Filesystem, path: &[u8], output: Option<&OsString>) -> Result<Outcome, CliError> {
    let ino = fs
        .lookup_path_bytes(path)
        .map_err(|e| driver_error(path, e))?;
    let inode = fs.stat_ino(ino).map_err(|e| driver_error(path, e))?;
    if inode.is_dir() {
        return Err(driver_error(path, fs_ext4::Error::IsADirectory));
    }
    if inode.is_symlink() {
        let target = fs.read_link_ino(ino).map(|t| show(&t)).unwrap_or_default();
        return Err(CliError::failed(format!(
            "{}: is a symlink to {target}; read the target instead",
            show(path)
        )));
    }
    if !inode.is_file() {
        return Err(CliError::failed(format!(
            "{}: not a regular file",
            show(path)
        )));
    }
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK];
    let mut copy = |sink: &mut dyn Write| -> Result<(), CliError> {
        let mut offset = 0u64;
        while offset < inode.size {
            let want = CHUNK.min((inode.size - offset) as usize);
            let got = fs
                .read_ino(ino, offset, &mut buf[..want])
                .map_err(|e| driver_error(path, e))?;
            if got == 0 {
                return Err(CliError::failed(format!(
                    "{}: short read at byte {offset} of {}",
                    show(path),
                    inode.size
                )));
            }
            sink.write_all(&buf[..got])
                .map_err(|e| CliError::failed(format!("write: {e}")))?;
            offset += got as u64;
        }
        sink.flush()
            .map_err(|e| CliError::failed(format!("write: {e}")))
    };
    match output {
        None => copy(&mut std::io::stdout().lock())?,
        Some(file) => {
            let dest = std::path::Path::new(file);
            let mut partial = dest.as_os_str().to_owned();
            partial.push(".partial");
            let partial = std::path::PathBuf::from(partial);
            let mut f = std::fs::File::create(&partial)
                .map_err(|e| CliError::failed(format!("create {}: {e}", partial.display())))?;
            if let Err(e) = copy(&mut f) {
                drop(f);
                let _ = std::fs::remove_file(&partial);
                return Err(e);
            }
            std::fs::rename(&partial, dest)
                .map_err(|e| CliError::failed(format!("rename to {}: {e}", dest.display())))?;
        }
    }
    Ok(Outcome::done())
}

/// The UUID in its standard 8-4-4-4-12 form.
fn uuid_text(u: &[u8; 16]) -> String {
    super::mkfs::format_uuid(u)
}

fn flag_names<F: bitflags::Flags<Bits = u32>>(bits: u32) -> Json {
    let known = F::from_bits_truncate(bits);
    let mut names: Vec<Json> = known
        .iter_names()
        .map(|(name, _)| Json::from(name.to_ascii_lowercase()))
        .collect();
    let unknown = bits & !known.bits();
    if unknown != 0 {
        names.push(Json::from(format!("0x{unknown:08x}")));
    }
    Json::Arr(names)
}

/// Whether the volume needs attention before it can be trusted: not
/// cleanly unmounted (`s_state` lacks VALID_FS, or carries ERROR_FS), or a
/// journal waiting to be replayed.
pub fn is_dirty(fs: &Filesystem) -> bool {
    const ERROR_FS: u16 = 0x0002;
    !fs.sb.is_clean()
        || fs.sb.state & ERROR_FS != 0
        || fs.sb.feature_incompat & Incompat::RECOVER.bits() != 0
}

/// The envelope: the shared keys first, ext4's own under `ext4`.
pub fn envelope(fs: &Filesystem) -> Json {
    let sb = &fs.sb;
    let block_size = u64::from(sb.block_size());
    Json::object([
        ("fs", Json::from("ext4")),
        (
            "label",
            if sb.volume_name.is_empty() {
                Json::Null
            } else {
                Json::from(sb.volume_name.as_str())
            },
        ),
        ("total_bytes", Json::from(sb.blocks_count * block_size)),
        ("free_bytes", Json::from(sb.free_blocks_count * block_size)),
        ("block_size", Json::from(block_size)),
        ("dirty", Json::from(is_dirty(fs))),
        (
            "ext4",
            Json::object([
                ("uuid", Json::from(uuid_text(&sb.uuid))),
                ("total_blocks", Json::from(sb.blocks_count)),
                ("free_blocks", Json::from(sb.free_blocks_count)),
                ("reserved_blocks", Json::from(sb.r_blocks_count)),
                ("total_inodes", Json::from(sb.inodes_count)),
                ("free_inodes", Json::from(sb.free_inodes_count)),
                ("inode_size", Json::from(sb.inode_size)),
                ("blocks_per_group", Json::from(sb.blocks_per_group)),
                ("inodes_per_group", Json::from(sb.inodes_per_group)),
                ("state", Json::from(sb.state)),
                ("last_mounted", Json::from(sb.last_mounted.as_str())),
                (
                    "features",
                    Json::object([
                        ("compat", flag_names::<Compat>(sb.feature_compat)),
                        ("incompat", flag_names::<Incompat>(sb.feature_incompat)),
                        ("ro_compat", flag_names::<RoCompat>(sb.feature_ro_compat)),
                    ]),
                ),
            ]),
        ),
    ])
}

fn get(fs: &Filesystem, key: Option<&str>) -> Result<Outcome, CliError> {
    let all = envelope(fs);
    let Some(key) = key else {
        return Ok(Outcome::report(all));
    };
    let mut value = Some(&all);
    for part in key.split('.') {
        value = value.and_then(|v| v.get(part));
    }
    let Some(value) = value else {
        return Err(CliError::usage(format!(
            "no key {key:?}; the keys are {} (and ext4.<field>)",
            KEYS.join(", ")
        )));
    };
    let text = value.to_text();
    Ok(Outcome::report(Json::object([(key, value.clone())])).with_text(text))
}

fn set(target: &OsString, offset: u64, sub: &ArgMatches) -> Result<Outcome, CliError> {
    let key = sub.get_one::<String>("key").expect("clap requires the key");
    match key.as_str() {
        "label" => {
            let value = sub
                .get_one::<String>("value")
                .expect("clap requires the value");
            let mut fs = super::device::mount(target, offset, true)?;
            fs.set_volume_label(value.as_bytes())
                .map_err(|e| CliError::failed(format!("set label: {e}")))?;
            fs.finish()
                .map_err(|e| CliError::failed(format!("set label: finishing the write: {e}")))?;
            let report = Json::object([("label", Json::from(value.as_str()))]);
            Ok(Outcome::report(report).with_text(format!("label set to {value:?}")))
        }
        k if KEYS.contains(&k) || k.starts_with("ext4.") => {
            Err(CliError::refused(format!("{k} is read-only")))
        }
        other => Err(CliError::usage(format!(
            "no key {other:?}; the settable key is label"
        ))),
    }
}

/// A path the write verbs can hand the library, which takes them as text.
fn utf8_path(path: &[u8]) -> Result<&str, CliError> {
    std::str::from_utf8(path).map_err(|_| {
        CliError::failed(format!(
            "{}: the write verbs take UTF-8 paths only",
            show(path)
        ))
    })
}

/// Run `edit` on a writable mount of `target`, then release it: flushed,
/// and the volume marked clean again. A release that fails is a failure,
/// even when the edit itself succeeded. When the edit fails the mount is
/// dropped instead, which puts back the state it found.
fn edit<T>(
    target: &OsString,
    offset: u64,
    what: &[u8],
    edit: impl FnOnce(&Filesystem) -> fs_ext4::Result<T>,
) -> Result<T, CliError> {
    let fs = super::device::mount(target, offset, true)?;
    let done = edit(&fs).map_err(|e| driver_error(what, e))?;
    fs.finish()
        .map_err(|e| CliError::failed(format!("{}: finishing the write: {e}", show(what))))?;
    Ok(done)
}

/// Create or replace a regular file with everything on stdin. The whole
/// input is read before the image is opened, so a failing producer
/// (`false | fs.ext4 img write /f`) leaves the image as it was, and a new
/// file whose content cannot be written (it does not fit) is removed again.
fn write(target: &OsString, offset: u64, path: &[u8]) -> Result<Outcome, CliError> {
    let name = utf8_path(path)?;
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut data)
        .map_err(|e| CliError::failed(format!("read stdin: {e}")))?;
    let (created, size) = edit(target, offset, path, |fs| {
        let created = match fs.lookup_path_bytes(path) {
            Ok(ino) => {
                let inode = fs.stat_ino(ino)?;
                if inode.is_dir() {
                    return Err(fs_ext4::Error::IsADirectory);
                }
                if !inode.is_file() {
                    return Err(fs_ext4::Error::InvalidArgument("not a regular file"));
                }
                false
            }
            Err(fs_ext4::Error::NotFound) => {
                fs.apply_create(name, 0o644)?;
                true
            }
            Err(e) => return Err(e),
        };
        match fs.apply_replace_file_content(name, &data) {
            Ok(size) => Ok((created, size)),
            Err(e) => {
                // A file this call made is taken away again, so a write
                // that does not fit leaves no empty file behind it. The
                // content error is the one reported either way.
                if created {
                    let _ = fs.apply_unlink(name);
                }
                Err(e)
            }
        }
    })?;
    let report = Json::object([
        ("path", Json::from(name)),
        ("bytes", Json::from(size)),
        ("created", Json::from(created)),
    ]);
    let text = format!(
        "{} {name} ({size} bytes)",
        if created { "created" } else { "replaced" }
    );
    Ok(Outcome::report(report).with_text(text))
}

/// Create one directory, mode 0755. Its parent must exist, and so must
/// nothing at the path.
fn mkdir(target: &OsString, offset: u64, path: &[u8]) -> Result<Outcome, CliError> {
    let name = utf8_path(path)?;
    let ino = edit(target, offset, path, |fs| fs.apply_mkdir(name, 0o755))?;
    let report = Json::object([("path", Json::from(name)), ("inode", Json::from(ino))]);
    Ok(Outcome::report(report).with_text(format!("created {name}")))
}
