//! mkfs.ext4 — standalone CLI for creating fresh ext4 filesystems.
//!
//! Linux-CLI-compatible: same flag names and the same positional
//! `device` argument as the conventional Linux ext4 formatter, so
//! existing scripts / Makefiles / CI pipelines work against this binary
//! unchanged. Independent implementation, written from the on-disk spec
//! (kernel.org ext4 wiki + Carrier's *File System Forensic Analysis*);
//! no derivation from any GPL prior-art codebase.
//!
//! Cross-platform: pure Rust, no OS-specific syscalls beyond `open` /
//! `seek` / `write` (all via std::fs). Builds and runs identically on
//! Linux, macOS, Windows. The same `format_filesystem()` entry point is
//! exposed via the C ABI as `fs_ext4_mkfs`, so any FFI host (a GUI
//! formatter, a packaging script, an FSKit extension's `startFormat`,
//! etc.) exercises the exact same code path as this CLI.
//!
//! Convention follows the standard CLI: the device/file MUST already exist at
//! the target size. Use `truncate -s 64M out.img` (Linux/macOS) or
//! `fsutil file createnew out.img 67108864` (Windows) to pre-create an
//! image, then `mkfs.ext4 out.img` formats it.
//!
//! Exit codes: 0 success, 1 any failure (matches the standard CLI convention).

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::mkfs::{format_filesystem, is_valid_block_size, MAX_BLOCK_SIZE, MIN_BLOCK_SIZE};
use std::process::ExitCode;

/// Standard-CLI flags this formatter accepts and ignores, each of which takes
/// exactly one argument that we discard.
///
/// The split between this list and [`IGNORED_BOOLEAN_FLAGS`] is load-bearing,
/// not cosmetic. A flag listed here consumes the token after it, so putting a
/// boolean flag in this list makes it eat whatever came next — which, for a
/// tool whose last argument is the device, is the device.
const IGNORED_FLAGS_WITH_ARG: &[&str] = &["-m", "-N", "-i", "-E", "-O", "-T"];

/// Standard-CLI flags this formatter accepts and ignores that take NO
/// argument.
///
/// `-c` (check the device for bad blocks) is the reason this list exists. It
/// used to sit in [`IGNORED_FLAGS_WITH_ARG`], so `mkfs.ext4 -c disk.img` read
/// the image path as `-c`'s value, warned about a `-c disk.img` option nobody
/// had written, and then failed with "missing positional <device> argument"
/// about the path the caller had just given it.
const IGNORED_BOOLEAN_FLAGS: &[&str] = &["-c"];

const USAGE: &str = "\
Usage: mkfs.ext4 [options] device

Options:
  -L <label>        Volume label (max 16 bytes UTF-8).
  -b <size>         Block size in bytes. Power of 2, 1024..=65536. Default: {DEFAULT_BLOCK_SIZE}.
  -U <uuid>         Volume UUID (32 hex chars, dashes optional). Default: random.
  -F                Force; format even if device looks in use. (Accepted; we do
                    not currently inspect for active mounts.)
  -n                Dry-run: parse args + open device but do not write.
  -q                Quiet (suppress non-error output).
  --create-size <SIZE>
                    Non-standard extension (not in the conventional CLI): if device
                    doesn't exist, create it as a regular file of the given size first.
                    SIZE accepts K/M/G/T suffixes (1024-based). Refuses to apply
                    to existing block devices — only valid for image files. Use
                    when scripting test pipelines so you don't have to chain
                    truncate + mkfs.ext4. Without this flag the tool follows
                    the standard CLI convention exactly (file must pre-exist).
  -V, --version     Print version and exit.
  -h, --help        Print this help and exit.

Positional:
  device            Path to a block device or pre-sized regular file. The
                    file/device MUST already exist at the target size unless
                    --create-size is given. Pre-create with
                      truncate -s 64M out.img    (Linux/macOS)
                      fsutil file createnew out.img 67108864    (Windows)

Unsupported flags from the standard CLI are accepted with a warning, and rejected
as errors otherwise. Two groups, because they parse differently: -m, -N, -i, -E,
-O and -T each consume the argument that follows them, while -c takes none. The
full feature set will land incrementally as the underlying crate grows.
";

/// [`USAGE`] with the default block size filled in from
/// [`fs_ext4::mkfs::DEFAULT_BLOCK_SIZE`], so the help cannot state a default
/// the formatter no longer uses (#180).
fn usage() -> String {
    USAGE.replace(
        "{DEFAULT_BLOCK_SIZE}",
        &fs_ext4::mkfs::DEFAULT_BLOCK_SIZE.to_string(),
    )
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("mkfs.ext4: {msg}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Default, Debug)]
struct Opts {
    label: Option<String>,
    block_size: Option<u32>,
    uuid: Option<[u8; 16]>,
    force: bool,
    dry_run: bool,
    quiet: bool,
    /// Bytes from `--create-size <SIZE>`. When `Some(n)` and the
    /// device path doesn't exist yet, we create it as a regular file
    /// of `n` bytes before formatting. Does NOT apply to block
    /// devices — see the safety guard in `run()`.
    create_size: Option<u64>,
    device: Option<String>,
    /// Warnings collected during the parse and printed once it is over.
    ///
    /// They are not printed as they are discovered because `-q` is itself a
    /// flag: printing during the loop means `-q` only silences the flags that
    /// happen to come after it, so `-m 1 -q` warned and `-q -m 1` did not.
    /// Collecting first and acting second makes the parse order-independent.
    warnings: Vec<String>,
}

fn run() -> Result<(), String> {
    let opts = parse_args()?;

    // Everything the parse wanted to say, now that the whole command line —
    // including any `-q` at the end of it — has been read.
    if !opts.quiet {
        for warning in &opts.warnings {
            eprintln!("mkfs.ext4: warning: {warning}");
        }
    }

    let device = opts
        .device
        .as_deref()
        .ok_or_else(|| format!("missing positional <device> argument\n\n{}", usage()))?;

    let block_size = opts.block_size.unwrap_or(fs_ext4::mkfs::DEFAULT_BLOCK_SIZE);

    // --create-size handling. Three cases per the doc'd contract:
    //   (a) device path already exists as a regular file: leave it
    //       alone; treat the flag as a no-op so re-running the same
    //       command is idempotent. (Caller can `rm` first if they
    //       want a fresh image; we don't second-guess.)
    //   (b) device path is a block / character device: refuse loudly.
    //       --create-size means "make me a file" and applying it to a
    //       real device would mask a typo (`/dev/diskN` vs `/dev/disk5`).
    //   (c) device path doesn't exist: create a regular file of the
    //       requested size and proceed.
    if let Some(n) = opts.create_size {
        match std::fs::metadata(device) {
            Ok(meta) => {
                let ft = meta.file_type();
                // Block/char-device check is Unix-only — Windows
                // doesn't expose /dev/diskN-style raw devices through
                // std::fs (block-level access uses different APIs).
                // On Windows the safety guard is just "must be a
                // regular file"; on Unix we additionally refuse real
                // block/char devices so a typo'd `--create-size 32M
                // /dev/disk5` can't sail through.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    if ft.is_block_device() || ft.is_char_device() {
                        return Err(format!(
                            "--create-size refuses to apply to {device}: looks like a real block/char device, \
                             not a regular file. Did you mean to leave --create-size off?"
                        ));
                    }
                }
                if !ft.is_file() {
                    return Err(format!(
                        "--create-size: {device} exists but is not a regular file"
                    ));
                }
                // Regular file already there — leave it alone (idempotent).
                if !opts.quiet {
                    eprintln!(
                        "mkfs.ext4: --create-size: {device} already exists ({} bytes); leaving as-is",
                        meta.len()
                    );
                }
            }
            Err(_) => {
                // Path doesn't exist (the typical case). Create + size it.
                let f = std::fs::File::create(device)
                    .map_err(|e| format!("--create-size: create {device}: {e}"))?;
                f.set_len(n)
                    .map_err(|e| format!("--create-size: set_len({n}) on {device}: {e}"))?;
                drop(f);
                if !opts.quiet {
                    eprintln!("mkfs.ext4: --create-size: created {device} ({n} bytes)");
                }
            }
        }
    }

    // Open RW first so we both fail fast on permission and learn the device
    // size without a separate stat call (FileDevice caches it).
    let dev =
        FileDevice::open_rw(device).map_err(|e| format!("open {device} read-write: {e:?}"))?;
    let size = dev.size_bytes();
    if size == 0 {
        return Err(format!(
            "device {device} reports size 0 — pre-create with truncate / fsutil first"
        ));
    }

    if !opts.quiet {
        eprintln!(
            "mkfs.ext4: formatting {device} ({size} bytes, block_size={block_size}{})",
            if opts.dry_run { ", dry-run" } else { "" }
        );
    }

    if opts.dry_run {
        if !opts.quiet {
            eprintln!("mkfs.ext4: dry-run — no writes performed");
        }
        let _ = opts.force; // suppress unused warning when neither path uses it
        return Ok(());
    }

    format_filesystem(&dev, opts.label.as_deref(), opts.uuid, size, block_size)
        .map_err(|e| format!("format failed: {e:?}"))?;

    // Flush so the file's bytes hit the underlying storage before exit —
    // without this a fast caller (`mkfs && mount`) can race the kernel
    // page cache.
    dev.flush().map_err(|e| format!("flush failed: {e:?}"))?;

    if !opts.quiet {
        eprintln!("mkfs.ext4: {device} formatted successfully");
    }
    Ok(())
}

/// Parse CLI args. Hand-rolled to keep the dep tree at zero — pulling in
/// clap just to handle ten flags would more than double the binary size.
fn parse_args() -> Result<Opts, String> {
    parse_args_from(std::env::args().skip(1))
}

/// The parse itself, over any argument sequence.
///
/// Split from [`parse_args`] so the flag table can be tested without spawning
/// a process: every argument-order and argument-consumption question this
/// tool has got wrong is answerable here, in a unit test, against the same
/// code the real command line goes through.
fn parse_args_from(mut args: impl Iterator<Item = String>) -> Result<Opts, String> {
    let mut opts = Opts::default();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", usage());
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!(
                    "mkfs.ext4 ({}) {}",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            "-L" => {
                let v = args
                    .next()
                    .ok_or_else(|| "-L requires a label argument".to_string())?;
                if v.len() > 16 {
                    return Err(format!(
                        "label too long ({} bytes); ext4 max is 16 bytes UTF-8",
                        v.len()
                    ));
                }
                opts.label = Some(v);
            }
            "-b" => {
                let v = args
                    .next()
                    .ok_or_else(|| "-b requires a block size argument".to_string())?;
                let n: u32 = v
                    .parse()
                    .map_err(|_| format!("-b: not a valid number: {v}"))?;
                // Reject here, not in the formatter. The formatter's check is
                // the last line of defence and it fires after `run()` has
                // opened the device read-write and announced that it is
                // formatting it — so an unusable `-b` looked like a failure
                // partway through a format rather than a rejected argument.
                if !is_valid_block_size(n) {
                    return Err(format!(
                        "-b: block size must be a power of two in \
                         {MIN_BLOCK_SIZE}..={MAX_BLOCK_SIZE}, got {n}"
                    ));
                }
                opts.block_size = Some(n);
            }
            "-U" => {
                let v = args
                    .next()
                    .ok_or_else(|| "-U requires a UUID argument".to_string())?;
                opts.uuid = Some(parse_uuid(&v)?);
            }
            "-F" => opts.force = true,
            "-n" => opts.dry_run = true,
            "-q" => opts.quiet = true,
            "--create-size" => {
                let v = args.next().ok_or_else(|| {
                    "--create-size requires a SIZE argument (e.g. 64M)".to_string()
                })?;
                opts.create_size = Some(parse_size(&v)?);
            }
            // Accepted-but-ignored standard-CLI flags. Warn so users don't
            // think the value was honored, but don't fail — keeps existing
            // scripts portable.
            other if IGNORED_FLAGS_WITH_ARG.contains(&other) => {
                let v = args
                    .next()
                    .ok_or_else(|| format!("{other} requires an argument"))?;
                opts.warnings
                    .push(format!("{other} {v} not yet honored, ignoring"));
            }
            other if IGNORED_BOOLEAN_FLAGS.contains(&other) => {
                opts.warnings
                    .push(format!("{other} not yet honored, ignoring"));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag: {other}\n\n{}", usage()));
            }
            // First non-flag positional is the device path. Reject duplicates
            // because mkfs.ext4 only formats one target per invocation.
            _ => {
                if opts.device.is_some() {
                    return Err(format!(
                        "extra positional argument: {arg} (only one device may be given)"
                    ));
                }
                opts.device = Some(arg);
            }
        }
    }

    Ok(opts)
}

/// Parse a size like "64M" / "1G" / "1024K" / "33554432" into bytes.
/// 1024-based multipliers (K/M/G/T), case-insensitive, optional 'B'
/// suffix tolerated. Bare numbers are bytes. Same convention as
/// `truncate -s` and most disk-image tools.
fn parse_size(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("--create-size: empty size argument".to_string());
    }
    // Strip optional trailing 'B' (e.g. "64MB" → "64M") so users who
    // type either form work.
    let s = trimmed.strip_suffix(['B', 'b']).unwrap_or(trimmed);
    let (num, mult): (&str, u64) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1024),
        Some('M' | 'm') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some('T' | 't') => (&s[..s.len() - 1], 1024 * 1024 * 1024 * 1024),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => return Err(format!("--create-size: unrecognised size suffix in {s:?}")),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("--create-size: not a valid number: {num:?}"))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("--create-size: {s} overflows u64"))
}

/// Parse a UUID from its standard text form. Accepts both with-dashes
/// (8-4-4-4-12) and bare-32-hex variants — matches the standard CLI.
fn parse_uuid(s: &str) -> Result<[u8; 16], String> {
    let cleaned: String = s.chars().filter(|c| *c != '-').collect();
    if cleaned.len() != 32 {
        return Err(format!(
            "UUID must be 32 hex chars (with optional dashes), got {} chars",
            cleaned.len()
        ));
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("UUID has non-hex character near position {}", i * 2))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    /// The help states the default the formatter uses, from the one
    /// constant, and leaves no placeholder behind (#180).
    #[test]
    fn the_help_states_the_default_block_size_from_the_constant() {
        let help = super::usage();
        assert!(
            help.contains(&format!("Default: {}.", fs_ext4::mkfs::DEFAULT_BLOCK_SIZE)),
            "{help}"
        );
        assert!(!help.contains("{DEFAULT_BLOCK_SIZE}"), "{help}");
    }

    use super::*;

    fn parse(argv: &[&str]) -> Result<Opts, String> {
        parse_args_from(argv.iter().map(|s| (*s).to_string()))
    }

    /// `-c` is "check for bad blocks" in the standard CLI and takes nothing.
    /// Listed as argument-taking, it consumed the device path — so the tool
    /// warned about an option the caller never wrote and then reported the
    /// device missing. The parse must see a device here.
    #[test]
    fn dash_c_is_boolean_and_leaves_the_device_alone() {
        let opts = parse(&["-c", "/tmp/disk.img"]).expect("parse");
        assert_eq!(opts.device.as_deref(), Some("/tmp/disk.img"));
        assert_eq!(opts.warnings.len(), 1, "one ignored-flag warning");
        assert!(opts.warnings[0].starts_with("-c "), "{:?}", opts.warnings);
    }

    /// The other half of the same rule: flags that really do take an argument
    /// must still swallow it, or their value becomes the device path.
    #[test]
    fn argument_taking_ignored_flags_consume_their_value() {
        for &flag in IGNORED_FLAGS_WITH_ARG {
            let opts = parse(&[flag, "1", "/tmp/disk.img"]).expect("parse");
            assert_eq!(
                opts.device.as_deref(),
                Some("/tmp/disk.img"),
                "{flag} should have eaten its own value, not the device"
            );
            assert_eq!(
                opts.warnings,
                vec![format!("{flag} 1 not yet honored, ignoring")]
            );
        }
    }

    /// A flag that takes an argument and is given none is still an error —
    /// silently treating the end of the command line as an empty value would
    /// hide a typo.
    #[test]
    fn argument_taking_ignored_flag_without_a_value_is_an_error() {
        let err = parse(&["-m"]).expect_err("should reject");
        assert!(err.contains("-m requires an argument"), "{err}");
    }

    /// The block size has to be rejected by the parser, because the caller
    /// after it opens the device read-write and prints "formatting" before
    /// the formatter's own check ever runs.
    #[test]
    fn out_of_range_block_size_is_rejected_at_parse_time() {
        for bad in ["3000", "512", "131072", "0"] {
            let err = parse(&["-b", bad, "/tmp/disk.img"])
                .expect_err("out-of-range block size should be rejected");
            assert!(
                err.starts_with("-b: block size must be a power of two"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn in_range_block_sizes_parse() {
        for good in ["1024", "4096", "65536"] {
            let opts = parse(&["-b", good, "/tmp/disk.img"]).expect("parse");
            assert_eq!(opts.block_size, Some(good.parse().unwrap()));
        }
    }

    /// `-q` used to be read while the loop that sets it was still running, so
    /// it silenced only what came after it. The parse now just records; the
    /// same options must come out whichever end of the line `-q` sits at.
    #[test]
    fn quiet_is_independent_of_flag_order() {
        let early = parse(&["-q", "-m", "1", "/tmp/disk.img"]).expect("parse");
        let late = parse(&["-m", "1", "-q", "/tmp/disk.img"]).expect("parse");
        assert!(early.quiet && late.quiet);
        assert_eq!(early.warnings, late.warnings);
        assert_eq!(early.warnings.len(), 1);
    }

    #[test]
    fn unknown_flags_are_still_rejected() {
        let err = parse(&["-Z", "/tmp/disk.img"]).expect_err("should reject");
        assert!(err.starts_with("unknown flag: -Z"), "{err}");
    }
}
