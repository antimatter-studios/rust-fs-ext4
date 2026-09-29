//! `fs.ext4 <target> <verb>`: an errand inside an ext4 image or device,
//! without mounting it.
//!
//! The verbs are the shared set: `ls`, `read`, `get`/`info`, `set`,
//! `resize`. Metadata is JSON (or `--text`); file content is raw bytes.
//! A verb the library cannot do yet still exists and answers `not
//! implemented` with exit status 3, so a script moved between filesystems
//! fails loudly instead of meaning something else.

use std::ffi::OsString;
use std::io::Write;
use std::sync::Arc;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use crate::common::{CliError, Json, Outcome, Tool};
use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::dir::DirEntryType;
use fs_ext4::features::{Compat, Incompat, RoCompat};
use fs_ext4::inode::Inode;
use fs_ext4::Filesystem;

pub const TOOL: Tool = Tool {
    name: "fs.ext4",
    verb: "fs",
    section: 1,
    about: "List, read and inspect an ext4 image or device without mounting it",
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
        .about("List, read and inspect an ext4 image or device without mounting it")
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
        .args(crate::common::format_args().map(|a| a.global(true)))
        .subcommand_required(true)
        .subcommand(
            Cmd::new("ls")
                .about("List a directory: name, type, size, mode, mtime (and a symlink's target)")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .default_value("/")
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.ext4 disk.img ls /etc\n  \
                     fs.ext4 disk.img ls / | jq -r '.[].name'\n  \
                     fs.ext4 disk.img ls --text /",
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
                .about("Change a property (label: not implemented yet)")
                .arg(Arg::new("key").value_name("KEY").required(true))
                .arg(Arg::new("value").value_name("VALUE").required(true))
                .after_help(
                    "Examples:\n  fs.ext4 disk.img set label BACKUP\n\n\
                     Answers `not implemented` (exit 3) until the library can write the \
                     label.",
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
        "ls" => ls(&open(target, offset)?, path_arg(sub)),
        "read" => read(&open(target, offset)?, path_arg(sub), sub.get_one("output")),
        "get" | "info" => get(
            &open(target, offset)?,
            sub.get_one::<String>("key").map(String::as_str),
        ),
        "set" => set(sub),
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

/// A device that starts `offset` bytes into another: a partition inside a
/// whole-disk image.
struct Offset<D> {
    inner: D,
    offset: u64,
    size: u64,
}

impl<D: BlockDevice> BlockDevice for Offset<D> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_ext4::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(fs_ext4::Error::OutOfBounds)?;
        if end > self.size {
            return Err(fs_ext4::Error::OutOfBounds);
        }
        self.inner.read_at(self.offset + offset, buf)
    }

    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_ext4::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(fs_ext4::Error::OutOfBounds)?;
        if end > self.size {
            return Err(fs_ext4::Error::OutOfBounds);
        }
        self.inner.write_at(self.offset + offset, buf)
    }

    fn flush(&self) -> fs_ext4::Result<()> {
        self.inner.flush()
    }

    fn is_writable(&self) -> bool {
        self.inner.is_writable()
    }
}

/// Mount `target` read-only, `offset` bytes in.
fn open(target: &OsString, offset: u64) -> Result<Filesystem, CliError> {
    let name = target.to_string_lossy();
    let dev = FileDevice::open(&name).map_err(|e| CliError::failed(format!("open {name}: {e}")))?;
    let size = dev.size_bytes();
    if offset >= size {
        return Err(CliError::failed(format!(
            "--offset {offset} is past the end of {name} ({size} bytes)"
        )));
    }
    let dev: Arc<dyn BlockDevice> = if offset == 0 {
        Arc::new(dev)
    } else {
        Arc::new(Offset {
            inner: dev,
            offset,
            size: size - offset,
        })
    };
    Filesystem::mount(dev)
        .map_err(|e| CliError::failed(format!("{name} is not a readable ext4 filesystem: {e}")))
}

fn ext4_error(what: &[u8], e: fs_ext4::Error) -> CliError {
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
        field("name")
    );
    if let Some(target) = e.get("target") {
        line.push_str(&format!(" -> {}", target.to_text()));
    }
    line
}

fn ls(fs: &Filesystem, path: &[u8]) -> Result<Outcome, CliError> {
    let ino = fs
        .lookup_path_bytes(path)
        .map_err(|e| ext4_error(path, e))?;
    let inode = fs.stat_ino(ino).map_err(|e| ext4_error(path, e))?;
    let entries = if inode.is_dir() {
        let mut listed = Vec::new();
        for d in fs.read_dir_ino(ino).map_err(|e| ext4_error(path, e))? {
            if d.name == b"." || d.name == b".." {
                continue;
            }
            let child = fs.stat_ino(d.inode).map_err(|e| {
                let mut full = path.to_vec();
                if !full.ends_with(b"/") {
                    full.push(b'/');
                }
                full.extend_from_slice(&d.name);
                ext4_error(&full, e)
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

/// Stream a regular file's bytes. Each chunk is read before it is
/// written, so a file whose blocks turn out unreadable part-way stops with
/// status 1 and what came before stays on stdout; everything that can be
/// refused up front (no such path, a directory, a symlink) is refused
/// before a byte is written. `-o FILE` writes `FILE.partial` and renames
/// it, so FILE is never left half written.
fn read(fs: &Filesystem, path: &[u8], output: Option<&OsString>) -> Result<Outcome, CliError> {
    let ino = fs
        .lookup_path_bytes(path)
        .map_err(|e| ext4_error(path, e))?;
    let inode = fs.stat_ino(ino).map_err(|e| ext4_error(path, e))?;
    if inode.is_dir() {
        return Err(ext4_error(path, fs_ext4::Error::IsADirectory));
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
                .map_err(|e| ext4_error(path, e))?;
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
fn is_dirty(fs: &Filesystem) -> bool {
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

fn set(sub: &ArgMatches) -> Result<Outcome, CliError> {
    let key = sub.get_one::<String>("key").expect("clap requires the key");
    match key.as_str() {
        "label" => Err(CliError::not_implemented(
            "set label: this library has no writer for the ext4 volume label yet",
        )),
        k if KEYS.contains(&k) || k.starts_with("ext4.") => {
            Err(CliError::refused(format!("{k} is read-only")))
        }
        other => Err(CliError::usage(format!(
            "no key {other:?}; the settable key is label"
        ))),
    }
}
