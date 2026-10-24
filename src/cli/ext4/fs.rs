//! `fs.ext4 <target> <verb>`: an errand inside an ext4 image or device,
//! without mounting it.
//!
//! The verbs are the shared set: `ls`, `stat`, `read`, `export`, `import`, `xattr`, `write`, `mkdir`,
//! `rm`, `rmdir`, `mv`, `ln`, `truncate`, `get`/`info`, `set`, `resize`. Metadata is JSON (or `--text`); file content is raw bytes.
//! A verb the library cannot do yet still exists and answers `not
//! implemented` with exit status 3, so a script moved between filesystems
//! fails loudly instead of meaning something else.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use fs_core::cli::{transfer, CliError, Format, Json, Outcome, Tool};
use fs_ext4::dir::{DirEntry, DirEntryType};
use fs_ext4::features::{Compat, Incompat, RoCompat};
use fs_ext4::inode::{Inode, InodeFlags};
use fs_ext4::{Filesystem, InodeRef};

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
             Metadata is JSON on stdout (--text for people); `read` and `xattr PATH NAME` \
             write raw bytes. A failure is {\"error\": \"...\", \"code\": N} on stderr, N being the \
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
            Cmd::new("export")
                .about("Copy a regular file, or with --recursive a tree, out through the shared bounded transfer")
                .args(transfer::export_args_with_times())
                .after_help(
                    "Examples:\n  fs.ext4 disk.img export /backup.tar backup.tar\n  \
                     fs.ext4 disk.img export --replace /etc/hostname hostname\n  \
                     fs.ext4 disk.img export /backup.tar - | tar tf -\n  \
                     fs.ext4 disk.img export --recursive /home/alice alice\n  \
                     fs.ext4 disk.img export --recursive --preserve=links,times /home/alice alice\n\n\
                     Files are copied in fixed chunks. A local destination is published only \
                     when complete; an existing name is refused unless --replace is given. \
                     The report includes the byte count and SHA-256; DEST - writes only raw \
                     bytes to stdout. Directories and symlinks are refused.\n\n\
                     With --recursive a directory is copied through the shared tree transfer: \
                     every item is reported, existing files are refused unless --replace is \
                     given, and symlinks and special files are skipped, never followed. \
                     --preserve=links recreates hard links once per inode and symlinks with \
                     their stored target; --preserve=times sets access and modification times \
                     from the ext4 inode. --preserve requires --recursive: a single-file export \
                     refuses it rather than publish a file without what was asked for.",
                ),
        )
        .subcommand(
            Cmd::new("import")
                .about("Import one regular file or stdin through bounded staged writes")
                .arg(Arg::new("source").value_name("SRC").required(true).value_parser(value_parser!(OsString)))
                .arg(Arg::new("path").value_name("PATH").required(true).value_parser(value_parser!(OsString)))
                .arg(Arg::new("replace").long("replace").action(ArgAction::SetTrue))
                .arg(Arg::new("verify").long("verify").action(ArgAction::SetTrue))
                .after_help("Examples:\n  fs.ext4 disk.img import --verify backup.tar /backup.tar\n  fs.ext4 disk.img import --replace hostname /etc/hostname\n  cat backup.tar | fs.ext4 disk.img import - /backup.tar\n\nFiles use fixed chunks and publish only when complete. SRC - is spooled to a private temporary host file first. --verify reopens the image and checks the staged SHA-256 before publication. Atomic publication requires a journal; otherwise exit 3. A kill may leave a .dj-import-*.partial file beside the destination, visible through ls. Trees and metadata preservation await the shared tree API."),
        )
        .subcommand(
            Cmd::new("xattr")
                .about("List a path's extended attributes, or write one value's raw bytes")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("name")
                        .value_name("NAME")
                        .help("The attribute to read, namespace prefix included (user.comment)")
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .value_name("FILE")
                        .value_parser(value_parser!(OsString))
                        .requires("name")
                        .help("Write the named attribute's value here instead of stdout"),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img xattr /usr/bin/ping\n  \
                     fs.ext4 disk.img xattr --text /home/me/notes.txt\n  \
                     fs.ext4 disk.img xattr /usr/bin/ping security.capability -o caps.bin\n\n\
                     Without NAME, list each attribute's name, namespace prefix included, and \
                     the size of its value, sorted by name bytes; --text lists size, a tab and \
                     name. With NAME, write the value's raw bytes, even with --text; an empty \
                     value succeeds and a missing attribute fails (exit 1). Values are the \
                     bytes the inode stores, so an ACL is ext4's compact form, and the tail of \
                     an inline file or directory is listed as system.data. A symlink's own \
                     attributes are read, never its target's. -o writes FILE.partial and \
                     renames it, as read does. Attributes cannot be set or removed here.",
                ),
        )
        .subcommand(
            Cmd::new("write")
                .about("Create or replace a file with the bytes on stdin, or write them into one at a position")
                .arg(
                    Arg::new("at")
                        .long("at")
                        .value_name("BYTES")
                        .value_parser(value_parser!(u64))
                        .help("Write into the existing file starting this many bytes in, truncating nothing"),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img write /etc/hostname < hostname\n  \
                     tar cf - ./dir | fs.ext4 disk.img write /backup.tar\n  \
                     fs.ext4 src.img read /f | fs.ext4 dst.img write /f\n  \
                     printf 'v2' | fs.ext4 disk.img write --at 4096 /data.bin\n\n\
                     The parent directory must exist. An existing file is replaced.\n\n\
                     With --at the bytes go into a regular file that is there, starting that \
                     many bytes in (a position inside the file; --offset stays where the \
                     filesystem starts in TARGET). Nothing else in the file changes, and a \
                     write past the end grows it, the gap reading back as zeros. Nothing is \
                     created and a symlink is never followed. A directory, a missing path, a \
                     symlink, an immutable or append-only file and a position past the largest \
                     file are refused before the image is opened for writing, and so is a \
                     block-mapped (ext2/ext3-style) or encrypted file, which this driver cannot \
                     write into (exit 3).",
                ),
        )
        .subcommand(
            Cmd::new("mkdir")
                .about("Create a directory (its parent must exist, or with -p every one on the way)")
                .arg(
                    Arg::new("parents")
                        .short('p')
                        .long("parents")
                        .action(ArgAction::SetTrue)
                        .help("Make every missing directory on the way; one already there is success"),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img mkdir /backup\n  \
                     fs.ext4 disk.img mkdir /backup/2026\n  \
                     fs.ext4 disk.img mkdir -p /backup/2026/10/17\n\n\
                     With --parents every missing directory on the way to PATH is made, mode \
                     0755, and the report lists those it made: none, and nothing written, when \
                     the whole chain is already there. A file, a symlink or a name too long \
                     anywhere on the way is refused before the image is opened for writing, so \
                     a refused command makes no part of the chain. A symlink is never \
                     followed, even one naming a directory.",
                ),
        )
        .subcommand(
            Cmd::new("rm")
                .about("Remove a name that is not a directory; the file goes with its last name")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img rm /notes.txt\n  \
                     fs.ext4 disk.img rm --text /backup/old.tar\n\n\
                     A symlink is removed, never followed. A file with other names keeps \
                     its content under them. A missing path or a directory is refused \
                     before the image is opened for writing. Nothing is removed recursively.",
                ),
        )
        .subcommand(
            Cmd::new("rmdir")
                .about("Remove an empty directory")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img rmdir /backup\n  \
                     fs.ext4 disk.img rmdir --text /backup/2026\n\n\
                     A missing path, anything that is not a directory, a directory with \
                     entries other than . and .., and the root are refused before the \
                     image is opened for writing.",
                ),
        )
        .subcommand(
            Cmd::new("mv")
                .about("Give a name a new name, in its own directory or another")
                .arg(
                    Arg::new("source")
                        .value_name("SOURCE")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("replace")
                        .long("replace")
                        .action(ArgAction::SetTrue)
                        .help("Replace a name already at PATH, as rename(2) does"),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img mv /notes.txt /archive/notes.txt\n  \
                     fs.ext4 disk.img mv /draft /final\n  \
                     fs.ext4 disk.img mv --replace /hosts.new /etc/hosts\n\n\
                     PATH is the new name itself, not a directory to move into. A name \
                     already at PATH is refused unless --replace is given, and then it is \
                     replaced as rename(2) does: a file by a file, an empty directory by a \
                     directory. A missing SOURCE or parent, a directory moved into itself, \
                     a directory with entries in the way, . and .. and the root are refused \
                     before the image is opened for writing. A symlink is moved, never \
                     followed.",
                ),
        )
        .subcommand(
            Cmd::new("ln")
                .about("Add a name for a file (a hard link), or with -s make a symbolic link")
                .arg(
                    Arg::new("symbolic")
                        .short('s')
                        .long("symbolic")
                        .action(ArgAction::SetTrue)
                        .help("Make a symbolic link whose target is TARGET, as given"),
                )
                .arg(
                    Arg::new("source")
                        .value_name("TARGET")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img ln /data/a /data/also-a\n  \
                     fs.ext4 disk.img ln -s ../etc/hosts /hosts\n  \
                     fs.ext4 disk.img ln --text -s /var/log /logs\n\n\
                     PATH is the new name itself, not a directory to link into. A hard link \
                     gives the file TARGET names one more name and one more link; a symlink \
                     named as TARGET is linked itself, never followed. With -s, TARGET is \
                     the symlink's text as given, never resolved: relative, absolute or \
                     naming nothing. A name already at PATH, a missing TARGET or parent, a \
                     hard link to a directory, a file at 65000 links, an empty target and a \
                     target that does not fit in one block (4095 bytes at most) are refused \
                     before the image is opened for writing.",
                ),
        )
        .subcommand(
            Cmd::new("truncate")
                .about("Set a regular file's length, releasing what lies past a shorter one")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("size")
                        .value_name("SIZE")
                        .required(true)
                        .value_parser(value_parser!(u64))
                        .help("The new length, a count of bytes"),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img truncate /var/log/syslog 0\n  \
                     fs.ext4 disk.img truncate /sparse.img 1073741824\n  \
                     fs.ext4 disk.img truncate --text /notes.txt 4096\n\n\
                     A shrink releases the blocks past the new end; a grow reads back as \
                     zeros and allocates nothing. Only a regular file that is there is \
                     truncated: nothing is created, and a symlink is never followed. A \
                     directory, a missing path, a symlink and an immutable or append-only \
                     file are refused before the image is opened for writing, and so are \
                     the shrink of a block-mapped (ext2/ext3-style) file and any truncate of \
                     an encrypted one, which this driver cannot do (exit 3).",
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
        "export" => {
            let request = transfer::ExportRequest::from_matches(sub)?;
            let tree = transfer::TreeOptions::from_matches(sub)?;
            // A single file has no links to keep, and the single-file
            // transfer does not set times, so --preserve there would be
            // ignored: refused before the image is opened.
            if !tree.recursive && sub.get_many::<String>("preserve").is_some() {
                return Err(CliError::usage(
                    "--preserve requires --recursive: a single-file export keeps neither links nor times",
                ));
            }
            let fs = open(target, offset)?;
            if tree.recursive {
                transfer::export_tree_with_times(
                    &request,
                    &mut TreeExport { fs: &fs },
                    tree,
                    transfer::LinkOptions::from_matches(sub)?,
                    transfer::TimeOptions::from_matches(sub)?,
                )
            } else {
                export(&fs, &request, transfer::ExportOptions::from_matches(sub)?)
            }
        }
        "import" => super::import::run(
            target,
            offset,
            path_arg(sub),
            sub.get_one::<OsString>("source")
                .expect("clap requires source"),
            sub.get_flag("replace"),
            sub.get_flag("verify"),
        ),
        "xattr" => xattr(
            &open(target, offset)?,
            path_arg(sub),
            sub.get_one::<OsString>("name").map(os_bytes),
            sub.get_one("output"),
        ),
        "write" => match sub.get_one::<u64>("at") {
            Some(&at) => write_at(target, offset, path_arg(sub), at),
            None => write(target, offset, path_arg(sub)),
        },
        "mkdir" if sub.get_flag("parents") => mkdir_parents(target, offset, path_arg(sub)),
        "mkdir" => mkdir(target, offset, path_arg(sub)),
        "rm" => remove(target, offset, path_arg(sub), Removal::Name),
        "rmdir" => remove(target, offset, path_arg(sub), Removal::Directory),
        "mv" => rename(
            target,
            offset,
            os_bytes(sub.get_one::<OsString>("source").expect("clap requires it")),
            path_arg(sub),
            sub.get_flag("replace"),
        ),
        "ln" => link(
            target,
            offset,
            os_bytes(sub.get_one::<OsString>("source").expect("clap requires it")),
            path_arg(sub),
            sub.get_flag("symbolic"),
        ),
        "truncate" => truncate(
            target,
            offset,
            path_arg(sub),
            *sub.get_one::<u64>("size").expect("clap requires it"),
        ),
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
    let copy = |sink: &mut dyn Write| -> Result<(), CliError> {
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
    deliver(output, copy)?;
    Ok(Outcome::done())
}

/// The driver's positioned reader, with the length captured before the
/// shared transfer starts. A generation-checked handle refuses inode reuse.
struct ExportSource<'a> {
    fs: &'a Filesystem,
    inode: InodeRef,
    size: u64,
    path: &'a [u8],
}

impl transfer::FileSource for ExportSource<'_> {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, CliError> {
        self.fs
            .read_ino(self.inode, offset, buf)
            .map_err(|e| driver_error(self.path, e))
    }
}

/// Resolve and validate the source through ext4, then leave copying,
/// digesting and complete-file publication to core's shared transfer.
fn export(
    fs: &Filesystem,
    request: &transfer::ExportRequest,
    options: transfer::ExportOptions,
) -> Result<Outcome, CliError> {
    let path = os_bytes(&request.path);
    let ino = fs
        .lookup_path_bytes(path)
        .map_err(|e| driver_error(path, e))?;
    let inode = fs.stat_ino(ino).map_err(|e| driver_error(path, e))?;
    if inode.is_dir() {
        return Err(driver_error(path, fs_ext4::Error::IsADirectory));
    }
    if inode.is_symlink() {
        return Err(CliError::failed(format!(
            "{}: a symlink is not followed: name the file it points at",
            show(path)
        )));
    }
    if !inode.is_file() {
        return Err(CliError::failed(format!(
            "{}: not a regular file",
            show(path)
        )));
    }
    transfer::export_with_options(
        request,
        &mut ExportSource {
            fs,
            inode: InodeRef::new(ino, inode.generation),
            size: inode.size,
            path,
        },
        options,
    )
}

/// ext4 lookup, listing, stat, symlink targets and positioned reads behind
/// core's shared tree transfer, which owns traversal, naming, publication
/// and reports. Symlinks and special files are `Other` and never followed.
struct TreeExport<'a> {
    fs: &'a Filesystem,
}

impl TreeExport<'_> {
    fn resolve(&self, path: &std::path::Path) -> Result<(Vec<u8>, u32, Inode), CliError> {
        let bytes = os_bytes(&path.as_os_str().to_owned()).to_vec();
        let ino = self
            .fs
            .lookup_path_bytes(&bytes)
            .map_err(|e| driver_error(&bytes, e))?;
        let inode = self.fs.stat_ino(ino).map_err(|e| driver_error(&bytes, e))?;
        Ok((bytes, ino, inode))
    }
}

/// `inode`'s kind, length, identity and times as the shared transfer
/// reads them. The inode number is unique within one filesystem and
/// shared by hard links.
fn tree_metadata(ino: u32, inode: &Inode) -> transfer::TreeMetadata {
    let kind = if inode.is_file() {
        transfer::TreeKind::File
    } else if inode.is_dir() {
        transfer::TreeKind::Directory
    } else {
        transfer::TreeKind::Other
    };
    transfer::TreeMetadata {
        kind,
        size: inode.size,
        inode: u64::from(ino),
        accessed: system_time(inode.atime, inode.atime_nsec),
        modified: system_time(inode.mtime, inode.mtime_nsec),
    }
}

/// Seconds and nanoseconds since the epoch, either side of it.
fn system_time(secs: i64, nsec: u32) -> Option<std::time::SystemTime> {
    use std::time::{Duration, UNIX_EPOCH};
    let whole = Duration::from_secs(secs.unsigned_abs());
    let at = if secs >= 0 {
        UNIX_EPOCH.checked_add(whole)?
    } else {
        UNIX_EPOCH.checked_sub(whole)?
    };
    at.checked_add(Duration::from_nanos(u64::from(nsec)))
}

#[cfg(unix)]
fn os_name(name: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(name.to_vec())
}

#[cfg(not(unix))]
fn os_name(name: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(name).into_owned())
}

impl transfer::TreeSource for TreeExport<'_> {
    /// The stored target of a fast (in-inode) or slow (block) symlink,
    /// read without resolving it. ext4 records no directory bit for a
    /// link, so Windows gets a file link.
    fn symbolic_link(
        &mut self,
        path: &std::path::Path,
    ) -> Result<Option<transfer::SymbolicLink>, CliError> {
        let (bytes, ino, inode) = self.resolve(path)?;
        if !inode.is_symlink() {
            return Ok(None);
        }
        let target = self
            .fs
            .read_link_ino(InodeRef::new(ino, inode.generation))
            .map_err(|e| driver_error(&bytes, e))?;
        Ok(Some(transfer::SymbolicLink {
            target: os_name(&target).into(),
            target_is_directory: false,
        }))
    }

    fn metadata(&mut self, path: &std::path::Path) -> Result<transfer::TreeMetadata, CliError> {
        let (_, ino, inode) = self.resolve(path)?;
        Ok(tree_metadata(ino, &inode))
    }

    fn read_dir(&mut self, path: &std::path::Path) -> Result<Vec<transfer::TreeEntry>, CliError> {
        let (bytes, ino, _) = self.resolve(path)?;
        let mut children = Vec::new();
        for d in self
            .fs
            .read_dir_ino(ino)
            .map_err(|e| driver_error(&bytes, e))?
        {
            if d.name == b"." || d.name == b".." {
                continue;
            }
            let child = self.fs.stat_ino(d.inode).map_err(|e| {
                let mut full = bytes.clone();
                if !full.ends_with(b"/") {
                    full.push(b'/');
                }
                full.extend_from_slice(&d.name);
                driver_error(&full, e)
            })?;
            children.push(transfer::TreeEntry {
                name: os_name(&d.name),
                metadata: tree_metadata(d.inode, &child),
            });
        }
        Ok(children)
    }

    fn read_file_at(
        &mut self,
        path: &std::path::Path,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, CliError> {
        let (bytes, ino, inode) = self.resolve(path)?;
        self.fs
            .read_ino(InodeRef::new(ino, inode.generation), offset, buf)
            .map_err(|e| driver_error(&bytes, e))
    }
}

/// Hand `copy` stdout, or with `-o FILE` a new `FILE.partial` that is
/// renamed to FILE once `copy` succeeds and removed if it fails, so FILE
/// is never left half written.
fn deliver(
    output: Option<&OsString>,
    mut copy: impl FnMut(&mut dyn Write) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let Some(file) = output else {
        return copy(&mut std::io::stdout().lock());
    };
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
        .map_err(|e| CliError::failed(format!("rename to {}: {e}", dest.display())))
}

/// `xattr PATH [NAME]`: the extended attributes PATH's own inode holds --
/// a symlink's, never its target's -- as the library reads them, values
/// in an EA inode followed. Without NAME, each name, namespace prefix
/// included, and the size of its value, sorted by name bytes, a name that
/// is not UTF-8 shown lossily with its exact bytes in `name_hex`; with
/// NAME, matched as raw bytes, that value's raw bytes, written the way
/// `read` writes a file's.
/// Everything is the inode's stored bytes: an ACL in ext4's compact form,
/// and the tail of an inline file or directory as `system.data`, which a
/// mounted kernel does not list.
fn xattr(
    fs: &Filesystem,
    path: &[u8],
    name: Option<&[u8]>,
    output: Option<&OsString>,
) -> Result<Outcome, CliError> {
    let ino = fs
        .lookup_path_bytes(path)
        .map_err(|e| driver_error(path, e))?;
    let (inode, raw) = fs
        .read_inode_verified(ino)
        .map_err(|e| driver_error(path, e))?;
    if let Some(name) = name {
        // Any bytes but NUL name an attribute, so NAME is matched as the
        // bytes it is, UTF-8 or not (#541).
        let value = fs_ext4::xattr::get_resolved_raw(fs, &inode, &raw, name)
            .map_err(|e| driver_error(path, e))?
            .ok_or_else(|| {
                CliError::failed(format!("{}: no attribute {}", show(path), show(name)))
            })?;
        deliver(output, |sink| {
            sink.write_all(&value)
                .and_then(|()| sink.flush())
                .map_err(|e| CliError::failed(format!("write: {e}")))
        })?;
        return Ok(Outcome::done());
    }
    let mut attrs =
        fs_ext4::xattr::read_all_resolved(fs, &inode, &raw).map_err(|e| driver_error(path, e))?;
    attrs.sort_by(|a, b| a.raw_name.cmp(&b.raw_name));
    let text: Vec<String> = attrs
        .iter()
        .map(|a| format!("{}\t{}", a.value.len(), a.name))
        .collect();
    let entries = attrs
        .into_iter()
        .map(|a| {
            let mut fields = vec![("name", Json::from(a.name))];
            if std::str::from_utf8(&a.raw_name).is_err() {
                fields.push((
                    "name_hex",
                    Json::from(
                        a.raw_name
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>(),
                    ),
                ));
            }
            fields.push(("size", Json::from(a.value.len() as u64)));
            Json::object(fields)
        })
        .collect();
    Ok(Outcome::report(Json::Arr(entries)).with_text(text.join("\n")))
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

/// Create every missing directory on the way to `path`, mode 0755, as
/// `mkdir -p` does, through the library's `apply_mkdir_at`, all in one
/// writable open. Every refusal is decided first, on a read-only mount, so
/// a refused command never opens the image for writing and makes no part
/// of the chain; a chain already there all the way is success without
/// opening it for writing either. Should a directory fail to be made all
/// the same (the volume is full), the ones made before it are removed
/// again. The names are handed over as bytes, so none of them need be
/// UTF-8.
fn mkdir_parents(target: &OsString, offset: u64, path: &[u8]) -> Result<Outcome, CliError> {
    let chain = chain(&open(target, offset)?, path)?;
    if !chain.is_empty() {
        edit(target, offset, path, |fs| {
            let mut made: Vec<u32> = Vec::with_capacity(chain.len());
            let parent = |step: &Step, made: &[u32]| match step.parent {
                Dir::Existing(ino) => ino,
                Dir::Made(i) => made[i],
            };
            for step in &chain {
                match fs.apply_mkdir_at(parent(step, &made), step.name, 0o755) {
                    Ok(ino) => made.push(ino),
                    Err(e) => {
                        // Last made, first removed, so each is empty when
                        // its turn comes. The error reported is the one
                        // that stopped the chain.
                        for step in chain[..made.len()].iter().rev() {
                            let _ = fs.apply_rmdir_at(parent(step, &made), step.name);
                        }
                        return Err(e);
                    }
                }
            }
            Ok(())
        })?;
    }
    let text = if chain.is_empty() {
        format!("{} exists; nothing created", show(path))
    } else {
        let lines: Vec<String> = chain
            .iter()
            .map(|step| format!("created {}", show(step.shown)))
            .collect();
        lines.join("\n")
    };
    let created: Vec<Json> = chain
        .iter()
        .map(|step| Json::from(show(step.shown)))
        .collect();
    let mut fields = path_fields(path);
    fields.push(("created", Json::from(created)));
    Ok(Outcome::report(Json::object(fields)).with_text(text))
}

/// The directory a directory of a chain is made in: one already on the
/// volume, or the one the chain makes at that index.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Existing(u32),
    Made(usize),
}

/// One directory `mkdir --parents` makes: the entry `name` in `parent`,
/// reported as `shown`, the path as given up to and including the name.
struct Step<'p> {
    parent: Dir,
    name: &'p [u8],
    shown: &'p [u8],
}

/// The directories `mkdir --parents` makes for `path`, in order, once each
/// refusal the guest's `mkdir -p` gives on the kernel's mount has been
/// ruled out, for the same reason and, where two apply, at the first name
/// on the way that fails (`tests/cli_mkdir_kernel.rs` holds the two to the
/// same answers):
///
/// - an empty path: not found;
/// - a name over 255 bytes: a name too long;
/// - a name already there that is not a directory: not a directory when
///   more of the path follows it, and already there when it is the last.
///
/// `.` stays where the walk is and `..` goes up, through a directory the
/// chain makes as well as one already there, so `a/../b` makes `a` and `b`
/// as `mkdir -p` does. Nothing is made until every name has been decided:
/// where `mkdir -p` makes the start of a chain before it stops, this makes
/// none of it. No symlink is followed, so one on the way is not a
/// directory and one at `path` is already there, even one naming a
/// directory, which `mkdir -p` would follow.
fn chain<'p>(fs: &Filesystem, path: &'p [u8]) -> Result<Vec<Step<'p>>, CliError> {
    use fs_ext4::Error;
    let refuse = |e: Error| driver_error(path, e);
    if path.is_empty() {
        return Err(CliError::failed("an empty path: not found"));
    }
    // Each name, with where it ends in `path`.
    let mut names = Vec::new();
    let mut at = 0;
    for name in path.split(|&b| b == b'/') {
        let end = at + name.len();
        if !name.is_empty() {
            names.push((name, end));
        }
        at = end + 1;
    }
    let mut steps: Vec<Step<'p>> = Vec::new();
    let mut here = Dir::Existing(fs_ext4::path::EXT4_ROOT_INODE);
    for (i, &(name, end)) in names.iter().enumerate() {
        if name == b"." {
            continue;
        }
        if name == b".." {
            here = match here {
                Dir::Existing(ino) => Dir::Existing(fs.lookup_at(ino, b"..").map_err(refuse)?),
                Dir::Made(j) => steps[j].parent,
            };
            continue;
        }
        if name.len() > 255 {
            return Err(refuse(Error::NameTooLong));
        }
        let planned = steps
            .iter()
            .position(|step| step.parent == here && step.name == name);
        let existing = match (planned, here) {
            (Some(_), _) | (None, Dir::Made(_)) => None,
            (None, Dir::Existing(dir)) => match fs.lookup_at(dir, name) {
                Ok(ino) => Some(ino),
                Err(Error::NotFound) => None,
                Err(e) => return Err(refuse(e)),
            },
        };
        here = match (planned, existing) {
            (Some(j), _) => Dir::Made(j),
            (None, Some(ino)) if fs.stat_ino(ino).map_err(refuse)?.is_dir() => Dir::Existing(ino),
            (None, Some(_)) if i + 1 == names.len() => return Err(refuse(Error::AlreadyExists)),
            (None, Some(_)) => return Err(refuse(Error::NotADirectory)),
            (None, None) => {
                steps.push(Step {
                    parent: here,
                    name,
                    shown: &path[..end],
                });
                Dir::Made(steps.len() - 1)
            }
        };
    }
    Ok(steps)
}

/// What a removal takes away: `rm` a name that is not a directory,
/// `rmdir` an empty directory.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Removal {
    Name,
    Directory,
}

/// Remove one name. Everything the removal could be refused for is
/// decided first, on a read-only mount, so a refused command never opens
/// the image for writing and leaves it byte-identical. The removal itself
/// is the library's `apply_unlink_at` or `apply_rmdir_at`, which checks
/// again; the name is handed over as bytes, so one that is not UTF-8 can
/// be removed too.
fn remove(
    target: &OsString,
    offset: u64,
    path: &[u8],
    removal: Removal,
) -> Result<Outcome, CliError> {
    let (parent, name) = removable(&open(target, offset)?, path, removal)?;
    edit(target, offset, path, |fs| match removal {
        Removal::Name => fs.apply_unlink_at(parent, name),
        Removal::Directory => fs.apply_rmdir_at(parent, name),
    })?;
    let mut fields = path_fields(path);
    fields.push(("removed", Json::from(true)));
    Ok(Outcome::report(Json::object(fields)).with_text(format!("removed {}", show(path))))
}

/// The directory holding `path`'s last name, and that name, once each
/// refusal the kernel gives `unlink(2)` and `rmdir(2)` has been ruled
/// out, for the same reason it gives (`tests/cli_rm_kernel.rs` holds the
/// two to the same answers): a path that does not resolve, a directory
/// given to `rm` or a name ending in `/` that is not one, anything but a
/// directory given to `rmdir`, a directory with entries other than `.`
/// and `..`, `.` itself, `..`, and the root.
///
/// The final name is never followed as a symlink, so a symlink is what `rm`
/// removes and what `rmdir` refuses as not a directory.
fn removable<'p>(
    fs: &Filesystem,
    path: &'p [u8],
    removal: Removal,
) -> Result<(u32, &'p [u8]), CliError> {
    use fs_ext4::Error;
    let refuse = |e: Error| driver_error(path, e);
    let (dir, name, trailing_slash) = last_name(path);
    if name.is_empty() {
        return Err(match removal {
            Removal::Name => refuse(Error::IsADirectory),
            Removal::Directory => CliError::failed(format!(
                "{}: the root directory cannot be removed",
                show(path)
            )),
        });
    }
    if removal == Removal::Directory && name == b"." {
        return Err(refuse(Error::InvalidArgument("rmdir: cannot remove .")));
    }
    if removal == Removal::Directory && name == b".." {
        return Err(refuse(Error::DirectoryNotEmpty));
    }
    let parent = fs.lookup_dir_path_bytes(dir).map_err(refuse)?;
    let ino = fs.lookup_at(parent, name).map_err(refuse)?;
    let inode = fs.stat_ino(ino).map_err(refuse)?;
    match removal {
        Removal::Name if inode.is_dir() => return Err(refuse(Error::IsADirectory)),
        Removal::Name if trailing_slash => return Err(refuse(Error::NotADirectory)),
        Removal::Name => {}
        Removal::Directory if !inode.is_dir() => return Err(refuse(Error::NotADirectory)),
        Removal::Directory => {
            if !is_empty_dir(fs, ino).map_err(refuse)? {
                return Err(refuse(Error::DirectoryNotEmpty));
            }
        }
    }
    Ok((parent, name))
}

/// A report's `path`, with its exact bytes in `path_hex` when it is not
/// UTF-8.
fn path_fields(path: &[u8]) -> Vec<(&'static str, Json)> {
    let mut fields = vec![("path", Json::from(show(path)))];
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex",
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields
}

/// `path` split for an operation on its last name: the directory part, the
/// last name with any trailing `/` taken off (empty for the root), and
/// whether there was a trailing `/`.
fn last_name(path: &[u8]) -> (&[u8], &[u8], bool) {
    let end = path.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
    let trimmed = &path[..end];
    let start = trimmed
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(0, |i| i + 1);
    let (dir, name) = trimmed.split_at(start);
    (dir, name, end < path.len())
}

/// The directory `dir` names: not found when it does not resolve, and not
/// a directory when it, or a name on the way to it, is something else. The
/// root when `dir` is empty.
fn directory(fs: &Filesystem, dir: &[u8]) -> fs_ext4::Result<u32> {
    let ino = fs.lookup_dir_path_bytes(dir)?;
    if fs.stat_ino(ino)?.is_dir() {
        Ok(ino)
    } else {
        Err(fs_ext4::Error::NotADirectory)
    }
}

/// Whether directory `ino` has no entries other than `.` and `..`.
fn is_empty_dir(fs: &Filesystem, ino: u32) -> fs_ext4::Result<bool> {
    let entries = fs.read_dir_ino(ino)?;
    Ok(entries.iter().all(|e| e.name == b"." || e.name == b".."))
}

/// Give `source` the name `path`, through the library's `apply_rename_at`,
/// replacing a name already there only when `replace` is set. Every refusal
/// is decided first, on a read-only mount, so a refused command never opens
/// the image for writing and leaves it byte-identical; a rename that would
/// change nothing (a name onto itself, or onto another name of the same
/// file, with `replace`) succeeds without opening it for writing either, as
/// rename(2) succeeds and does nothing. The names are handed over as bytes,
/// so one that is not UTF-8 can be moved too.
fn rename(
    target: &OsString,
    offset: u64,
    source: &[u8],
    path: &[u8],
    replace: bool,
) -> Result<Outcome, CliError> {
    if let Some(the_move) = movable(&open(target, offset)?, source, path, replace)? {
        edit(target, offset, source, |fs| {
            fs.apply_rename_at(
                the_move.from_dir,
                the_move.from_name,
                the_move.to_dir,
                the_move.to_name,
                replace,
            )
        })?;
    }
    let mut fields = path_fields(path);
    fields.push(("moved", Json::from(true)));
    let text = format!("moved {} to {}", show(source), show(path));
    Ok(Outcome::report(Json::object(fields)).with_text(text))
}

/// A rename every refusal has been ruled out for: the entry `from_name` in
/// directory `from_dir`, to be filed as `to_name` in `to_dir`.
struct Move<'p> {
    from_dir: u32,
    from_name: &'p [u8],
    to_dir: u32,
    to_name: &'p [u8],
}

/// The rename of `source` to `path`, or `None` when it would change
/// nothing, once each refusal the kernel gives `renameat2` has been ruled
/// out, for the same reason it gives and, where two apply, the one it
/// gives first (`tests/cli_mv_kernel.rs` holds the two to the same answers;
/// without `replace` the call compared is `RENAME_NOREPLACE`, with it plain
/// rename(2)). In that order:
///
/// - a directory part of either path that does not resolve, or is not a
///   directory: `source`'s first, then `path`'s;
/// - `.`, `..` or the root as `source`: busy;
/// - `.`, `..` or the root as `path`: it exists, and with `replace` it is
///   busy;
/// - a `source` that does not exist; then a new name too long;
/// - a name already at `path`, without `replace`;
/// - a `source` that is not a directory, named with a trailing `/` on
///   either path;
/// - a directory moved to within itself;
/// - a name at `path` that `source` cannot replace: a directory for a
///   file, a file for a directory, or a directory with entries.
///
/// The final name is never followed as a symlink, so a symlink is moved as
/// itself, and replaced as a file is.
fn movable<'p>(
    fs: &Filesystem,
    source: &'p [u8],
    path: &'p [u8],
    replace: bool,
) -> Result<Option<Move<'p>>, CliError> {
    use fs_ext4::Error;
    let at_source = |e: Error| driver_error(source, e);
    let at_path = |e: Error| driver_error(path, e);
    let busy = |what: &[u8]| {
        CliError::failed(format!(
            "{}: resource busy: ., .. and the root cannot be moved or replaced",
            show(what)
        ))
    };
    let not_a_name = |name: &[u8]| name.is_empty() || name == b"." || name == b"..";

    let (from_dir, from_name, from_slash) = last_name(source);
    let (to_dir, to_name, to_slash) = last_name(path);
    let from_dir = directory(fs, from_dir).map_err(at_source)?;
    let to_dir = directory(fs, to_dir).map_err(at_path)?;
    if not_a_name(from_name) {
        return Err(busy(source));
    }
    if not_a_name(to_name) {
        return Err(if replace {
            busy(path)
        } else {
            at_path(Error::AlreadyExists)
        });
    }
    let from = fs.lookup_at(from_dir, from_name).map_err(at_source)?;
    if to_name.len() > 255 {
        return Err(at_path(Error::NameTooLong));
    }
    let existing = match fs.lookup_at(to_dir, to_name) {
        Ok(ino) => Some(ino),
        Err(Error::NotFound) => None,
        Err(e) => return Err(at_path(e)),
    };
    if existing.is_some() && !replace {
        return Err(at_path(Error::AlreadyExists));
    }
    let from_is_dir = fs.stat_ino(from).map_err(at_source)?.is_dir();
    if !from_is_dir && from_slash {
        return Err(at_source(Error::NotADirectory));
    }
    if !from_is_dir && to_slash {
        return Err(at_path(Error::NotADirectory));
    }
    if from_is_dir && within(fs, to_dir, from).map_err(at_path)? {
        return Err(at_path(Error::InvalidArgument(
            "a directory cannot be moved into itself",
        )));
    }
    if let Some(old) = existing {
        if old == from {
            return Ok(None);
        }
        let old_is_dir = fs.stat_ino(old).map_err(at_path)?.is_dir();
        if old_is_dir && !from_is_dir {
            return Err(at_path(Error::IsADirectory));
        }
        if from_is_dir && !old_is_dir {
            return Err(at_path(Error::NotADirectory));
        }
        if old_is_dir && !is_empty_dir(fs, old).map_err(at_path)? {
            return Err(at_path(Error::DirectoryNotEmpty));
        }
    }
    Ok(Some(Move {
        from_dir,
        from_name,
        to_dir,
        to_name,
    }))
}

/// Whether directory `dir` is directory `top` or lies beneath it, by the
/// `..` entries from `dir` up to the root, whose `..` is itself.
fn within(fs: &Filesystem, dir: u32, top: u32) -> fs_ext4::Result<bool> {
    let mut at = dir;
    // A chain longer than the inode table is a loop, not a tree.
    for _ in 0..=fs.sb.inodes_count {
        if at == top {
            return Ok(true);
        }
        let parent = fs.lookup_at(at, b"..")?;
        if parent == at {
            return Ok(false);
        }
        at = parent;
    }
    Err(fs_ext4::Error::Corrupt(
        "a directory's .. chain does not reach the root",
    ))
}

/// The most links a file can have. The kernel refuses one more as too many
/// links, as `tests/cli_ln_kernel.rs` shows on a file brought to it.
const MAX_LINKS: u16 = 65000;

/// The longest symlink target the kernel takes: one of 4096 bytes or more
/// it refuses as a name too long before it looks at the volume at all.
const MAX_TARGET: usize = 4095;

/// Give `path` to the file `source` names (a hard link), or with `symbolic`
/// make `path` a symlink whose target is `source` as given, through the
/// library's `apply_link_at` or `apply_symlink_at`. Every refusal is
/// decided first, on a read-only mount, so a refused command never opens
/// the image for writing and leaves it byte-identical. The names and the
/// target are handed over as bytes, so none of them need be UTF-8.
fn link(
    target: &OsString,
    offset: u64,
    source: &[u8],
    path: &[u8],
    symbolic: bool,
) -> Result<Outcome, CliError> {
    let the_link = linkable(&open(target, offset)?, source, path, symbolic)?;
    let ino = edit(target, offset, path, |fs| match the_link.inode {
        Some(ino) => fs
            .apply_link_at(ino, the_link.dir, the_link.name)
            .map(|()| ino),
        None => fs.apply_symlink_at(the_link.dir, the_link.name, source),
    })?;
    let mut fields = path_fields(path);
    fields.push(("linked", Json::from(true)));
    fields.push(("inode", Json::from(ino)));
    let text = if symbolic {
        format!("linked {} -> {}", show(path), show(source))
    } else {
        format!("linked {} to {}", show(path), show(source))
    };
    Ok(Outcome::report(Json::object(fields)).with_text(text))
}

/// A link every refusal has been ruled out for: the entry `name` to be
/// added to directory `dir`, naming the inode `inode` (a hard link), or a
/// new symlink when that is `None`.
struct Link<'p> {
    inode: Option<u32>,
    dir: u32,
    name: &'p [u8],
}

/// The link of `path` to `source`, once each refusal the kernel gives
/// `linkat` (without flags) or `symlinkat` has been ruled out, for the same
/// reason it gives and, where two apply, the one it gives first
/// (`tests/cli_ln_kernel.rs` holds the two to the same answers). In that
/// order:
///
/// - for a symlink, an empty target (not found), or one of 4096 bytes or
///   more (a name too long);
/// - for a hard link, a `source` that does not exist, a name on the way to
///   it that is not a directory, or a trailing `/` on one that is not;
/// - a directory part of `path` that does not resolve, or is not a
///   directory;
/// - a new name over 255 bytes;
/// - a name already at `path`, `.`, `..` and the root included; then a
///   new name with a trailing `/` (not found);
/// - for a hard link, a directory as `source` (not permitted), then a file
///   at the most links it can have;
/// - for a symlink, a target as long as one block or longer.
///
/// The final name is never followed as a symlink, so a hard link to a symlink
/// names the symlink. The kernel does follow one named with a trailing
/// `/`, which this refuses as not a directory whatever it names. Both
/// names are in the one volume, whose root's `..` is the root, so a link
/// across volumes cannot be asked for.
fn linkable<'p>(
    fs: &Filesystem,
    source: &[u8],
    path: &'p [u8],
    symbolic: bool,
) -> Result<Link<'p>, CliError> {
    use fs_ext4::Error;
    let at_source = |e: Error| driver_error(source, e);
    let at_path = |e: Error| driver_error(path, e);
    let too_long = |most: usize| {
        CliError::failed(format!(
            "{}: name too long: a symlink target here is at most {most} bytes",
            show(path)
        ))
    };

    let inode = if symbolic {
        if source.is_empty() {
            return Err(CliError::failed(format!(
                "{}: not found: a symlink target cannot be empty",
                show(path)
            )));
        }
        if source.len() > MAX_TARGET {
            return Err(too_long(MAX_TARGET));
        }
        None
    } else {
        let (dir, name, slash) = last_name(source);
        let dir = directory(fs, dir).map_err(at_source)?;
        let ino = if name.is_empty() {
            dir
        } else {
            fs.lookup_at(dir, name).map_err(at_source)?
        };
        let inode = fs.stat_ino(ino).map_err(at_source)?;
        if slash && !inode.is_dir() {
            return Err(at_source(Error::NotADirectory));
        }
        Some((ino, inode))
    };

    let (dir, name, slash) = last_name(path);
    let dir = directory(fs, dir).map_err(at_path)?;
    if name.len() > 255 {
        return Err(at_path(Error::NameTooLong));
    }
    if name.is_empty() || name == b"." || name == b".." {
        return Err(at_path(Error::AlreadyExists));
    }
    match fs.lookup_at(dir, name) {
        Ok(_) => return Err(at_path(Error::AlreadyExists)),
        Err(Error::NotFound) if slash => return Err(at_path(Error::NotFound)),
        Err(Error::NotFound) => {}
        Err(e) => return Err(at_path(e)),
    }

    match inode {
        Some((ino, inode)) => {
            if inode.is_dir() {
                return Err(CliError::failed(format!(
                    "{}: operation not permitted: a directory cannot have a hard link",
                    show(source)
                )));
            }
            if inode.links_count >= MAX_LINKS {
                return Err(at_source(Error::TooManyLinks));
            }
            Ok(Link {
                inode: Some(ino),
                dir,
                name,
            })
        }
        None => {
            // The kernel refuses a target as long as a block (1023 bytes
            // fit a 1 KiB block, 1024 do not), after every reason above.
            let most = fs.sb.block_size() as usize - 1;
            if source.len() > most {
                return Err(too_long(most));
            }
            Ok(Link {
                inode: None,
                dir,
                name,
            })
        }
    }
}

/// Set the length of the regular file `path` names to `size` bytes,
/// through the library's `apply_truncate_ino`: a shrink releases the
/// blocks past the new end, and a grow reads back as zeros and allocates
/// nothing. Every refusal is decided first, on a read-only mount, so a
/// refused command never opens the image for writing and leaves it
/// byte-identical. The names are looked up as bytes, so none of them need
/// be UTF-8.
fn truncate(target: &OsString, offset: u64, path: &[u8], size: u64) -> Result<Outcome, CliError> {
    let fs = open(target, offset)?;
    let ino = truncatable(&fs, path, size)?;
    if size
        > fs.max_file_size_ino(ino)
            .map_err(|e| driver_error(path, e))?
    {
        return Err(driver_error(path, fs_ext4::Error::FileTooLarge));
    }
    edit(target, offset, path, |fs| fs.apply_truncate_ino(ino, size))?;
    let mut fields = path_fields(path);
    fields.push(("size", Json::from(size)));
    let text = format!("{} is {size} bytes", show(path));
    Ok(Outcome::report(Json::object(fields)).with_text(text))
}

/// The inode of the regular file `path` names, once each refusal of
/// [`existing_file`] has been ruled out and then the shrink of a
/// block-mapped file, which the library cannot plan, with exit 3.
fn truncatable(fs: &Filesystem, path: &[u8], size: u64) -> Result<u32, CliError> {
    let (ino, inode) = existing_file(fs, path)?;
    if size < inode.size && !inode.has_extents() && !inode.has_inline_data() {
        return Err(CliError::not_implemented(format!(
            "{}: this driver cannot shrink a block-mapped (ext2/ext3-style) file, only grow it",
            show(path)
        )));
    }
    Ok(ino)
}

/// The inode of the regular file `path` names and its attributes, once
/// each refusal the kernel gives opening it for writing has been ruled
/// out, for the same reason and, where two apply, at the first name on the
/// way that fails (`tests/cli_truncate_kernel.rs` holds this to the
/// guest's `truncate`, `tests/cli_pwrite_kernel.rs` to its `dd
/// conv=notrunc`):
///
/// - an empty path: not found;
/// - on the way, a name after one that is not a directory: not a
///   directory; then a name over 255 bytes: a name too long; then a name
///   that is not there: not found;
/// - a directory, the root included, or a trailing `/` after anything
///   else: is a directory, as the guest's `truncate` and `dd` open to
///   create and the kernel will not create a name that ends in `/`;
/// - an immutable or append-only file: not permitted.
///
/// No symlink is followed, so one on the way is not a directory and one at
/// `path` is refused, where the guest's tools would write to what it
/// names. A missing file is not created, as they would create it. After
/// those, an encrypted file is refused with exit 3: this driver holds no
/// key for its blocks.
fn existing_file(fs: &Filesystem, path: &[u8]) -> Result<(u32, Inode), CliError> {
    use fs_ext4::Error;
    let refuse = |e: Error| driver_error(path, e);
    if path.is_empty() {
        return Err(CliError::failed("an empty path: not found"));
    }
    let mut ino = fs_ext4::path::EXT4_ROOT_INODE;
    let mut inode = fs.stat_ino(ino).map_err(refuse)?;
    for name in path.split(|&b| b == b'/').filter(|name| !name.is_empty()) {
        if !inode.is_dir() {
            return Err(refuse(Error::NotADirectory));
        }
        if name.len() > 255 {
            return Err(refuse(Error::NameTooLong));
        }
        ino = fs.lookup_at(ino, name).map_err(refuse)?;
        inode = fs.stat_ino(ino).map_err(refuse)?;
    }
    if inode.is_dir() || path.ends_with(b"/") {
        return Err(refuse(Error::IsADirectory));
    }
    if inode.is_symlink() {
        return Err(CliError::failed(format!(
            "{}: a symlink is not followed: name the file it points at",
            show(path)
        )));
    }
    if !inode.is_file() {
        return Err(refuse(Error::InvalidArgument("not a regular file")));
    }
    if inode.flags & (InodeFlags::IMMUTABLE.bits() | InodeFlags::APPEND.bits()) != 0 {
        return Err(CliError::failed(format!(
            "{}: operation not permitted: the file is immutable or append-only",
            show(path)
        )));
    }
    if inode.flags & InodeFlags::ENCRYPT.bits() != 0 {
        return Err(CliError::not_implemented(format!(
            "{}: the file is encrypted and this driver holds no key",
            show(path)
        )));
    }
    Ok((ino, inode))
}

/// Write everything on stdin into the regular file `path` names, starting
/// `at` bytes in, through the library's `apply_pwrite_ino`: the bytes
/// there are overwritten and no others, and a write past the end grows the
/// file, the gap reading back as zeros and allocating nothing. The whole
/// input is read before the image is opened, and every refusal is decided
/// on a read-only mount, so a refused command never opens the image for
/// writing and leaves it byte-identical; nothing on stdin writes nothing
/// and does not open it for writing either. The names are looked up as
/// bytes, so none of them need be UTF-8.
fn write_at(target: &OsString, offset: u64, path: &[u8], at: u64) -> Result<Outcome, CliError> {
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut data)
        .map_err(|e| CliError::failed(format!("read stdin: {e}")))?;
    let (ino, inode) = writable_at(&open(target, offset)?, path, at, data.len() as u64)?;
    let size = if data.is_empty() {
        inode.size
    } else {
        edit(target, offset, path, |fs| {
            fs.apply_pwrite_ino(ino, at, &data)
        })?
    };
    let mut fields = path_fields(path);
    fields.push(("at", Json::from(at)));
    fields.push(("bytes", Json::from(data.len() as u64)));
    fields.push(("size", Json::from(size)));
    let text = format!(
        "wrote {} bytes at {at} into {} ({size} bytes)",
        data.len(),
        show(path)
    );
    Ok(Outcome::report(Json::object(fields)).with_text(text))
}

/// The inode of the regular file `path` names and its attributes, once
/// each refusal of [`existing_file`] has been ruled out, then a write of
/// `len` bytes at `at` ending past the last block an extent can address
/// (`ee_block` is 32 bits): a file too large. After those, a block-mapped
/// file, which the library cannot write into, is refused with exit 3.
fn writable_at(fs: &Filesystem, path: &[u8], at: u64, len: u64) -> Result<(u32, Inode), CliError> {
    let (ino, inode) = existing_file(fs, path)?;
    let largest = (u64::from(u32::MAX) + 1) * u64::from(fs.sb.block_size());
    if at.checked_add(len).is_none_or(|end| end > largest) {
        return Err(CliError::failed(format!(
            "{}: file too large: a write at {at} ends past the {largest} bytes a file can hold",
            show(path)
        )));
    }
    if !inode.has_extents() && !inode.has_inline_data() {
        return Err(CliError::not_implemented(format!(
            "{}: this driver cannot write into a block-mapped (ext2/ext3-style) file",
            show(path)
        )));
    }
    Ok((ino, inode))
}
