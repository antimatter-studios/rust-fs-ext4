//! The oracle tools, run where the filesystem they belong to lives: IN
//! THE HARNESS VM, never on the host.
//!
//! WHY NOT ON THE HOST. e2fsprogs on a workstation is whatever that
//! machine happens to have: a Homebrew keg on a Mac, a distribution
//! build on Linux, three different versions across three developers —
//! and on a Mac it is not even the same code path the filesystem it
//! judges runs under. An oracle whose answer depends on which laptop
//! asked is not an oracle. So there is ONE place the tools exist: the
//! Debian guest the harness boots, provisioned by `scripts/vm-setup.sh`,
//! the same guest that builds the kernel-made fixtures. A Mac needs no
//! e2fsprogs at all, and every machine gets the same answers.
//!
//! WHY IT IS NOT SLOW. The VM is booted once for a suite (`chore
//! test:oracle` brings it up and holds it) and every call rides one
//! multiplexed SSH connection — about 30 ms of overhead per tool
//! invocation, against 700 ms for a fresh connection. Nothing is copied:
//! the harness mounts this repository in the guest AT THE SAME ABSOLUTE
//! PATH the host uses, so an image at `<repo>/tmp/x.img` is that same
//! path in the guest, and the arguments cross unchanged.
//!
//! THAT IS ALSO THE ONE RULE A TEST MUST KEEP: everything a tool touches
//! lives inside this repository ([`crate::temp_dir`] and `test-disks/`
//! both do). A path outside it fails here, naming the rule, rather than
//! producing a puzzling "No such file or directory" from the guest.
//!
//! WHEN THE SUITE ITSELF RUNS IN THE GUEST (`chore test:vm`, which is
//! how a Mac runs the Linux suite at all), there is no VM to ask: this
//! IS it. The harness says so with `FLTH_GUEST=1` in every command it
//! runs there, and the tool is then spawned directly. That is the same
//! rule, not a second one — the tools run in the harness's Debian guest
//! and nowhere else; only the distance to it changes.
//!
//! NOTHING SKIPS. A missing harness, a VM that will not boot, a tool the
//! guest does not have: each fails the test that needed it, naming the
//! task that fixes it.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use crate::verdict::{Judge, Judged, Verdict};

/// This repository, which is also where the guest sees it.
pub(crate) fn repo() -> &'static Path {
    static REPO: OnceLock<PathBuf> = OnceLock::new();
    REPO.get_or_init(|| {
        // <repo>/tests/support -> <repo>
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("the test support crate lives at <repo>/tests/support")
            .to_path_buf()
    })
}

/// The harness's driver script, or a panic naming `chore siblings`.
pub(crate) fn vm_script() -> &'static Path {
    static VM: OnceLock<PathBuf> = OnceLock::new();
    VM.get_or_init(|| {
        let path = repo().join("../fs-linux-test-harness/scripts/vm.sh");
        assert!(
            path.is_file(),
            "the fs-linux-test-harness sibling is not checked out at {}. \
             `chore siblings` clones it at the commit chores.yml pins. \
             The oracle tools run in its VM and nowhere else, so there is \
             nothing to fall back to and nothing to skip.",
            path.display()
        );
        path
    })
}

/// True when this process is itself running inside the harness guest.
pub(crate) fn in_guest() -> bool {
    std::env::var_os("FLTH_GUEST").is_some_and(|value| value == "1")
}

/// Run a harness command from the repository root (where the harness
/// finds `fs-linux-test-harness.toml`).
pub(crate) fn vm(command: &str, argument: &str) -> io::Result<Output> {
    Command::new(vm_script())
        .arg(command)
        .arg(argument)
        .current_dir(repo())
        .stdin(Stdio::null())
        .output()
}

/// Run a script in the guest, wherever this process is.
///
/// From the host that is `vm.sh exec` over the harness's one shared
/// connection. Inside the guest it is the shell itself.
pub(crate) fn guest_shell(script: &str) -> io::Result<Output> {
    if in_guest() {
        return Command::new("bash")
            .arg("-c")
            .arg(script)
            .current_dir(repo())
            .stdin(Stdio::null())
            .output();
    }
    vm("exec", script)
}

/// Boot the VM once per test process, and hold the result.
///
/// `vm.sh up` is idempotent and costs milliseconds when the VM is
/// already running, which is the normal case: `chore test:oracle` brings
/// it up for the whole tier. A test process that finds it down boots it
/// rather than failing — a suite run by hand with a bare `cargo test`
/// still works, and the chore reaper stops what it left behind.
pub(crate) fn session() {
    static SESSION: OnceLock<()> = OnceLock::new();
    SESSION.get_or_init(|| {
        if in_guest() {
            return;
        }
        let out = vm("up", "")
            .unwrap_or_else(|error| panic!("cannot run {}: {error}", vm_script().display()));
        assert!(
            out.status.success(),
            "the fs-linux-test-harness VM would not start, so no oracle tool can run.\n\
             `chore vm:host:check` says what this host is missing; `chore vm:destroy` \
             clears a broken machine.\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        mirror_repo_path();
    });
}

/// MAKE ONE PATH MEAN ONE THING ON BOTH SIDES.
///
/// The harness mounts this repository in the guest at `/repo`. A test
/// hands `e2fsck` the path it used on the host — `<repo>/tmp/x.img` —
/// so the guest is given that same absolute path as a symlink to the
/// mount. Every argument then crosses unchanged: no rewriting of
/// arguments, none of the paths inside a `debugfs` script, and no
/// copying of images in and out.
///
/// Idempotent, and it refuses to replace a real directory: in a guest
/// that somehow has one at that path, silently shadowing it would be
/// worse than stopping.
fn mirror_repo_path() {
    let repo = repo().to_string_lossy().into_owned();
    let script = format!(
        "set -eu\n\
         repo={0}\n\
         if [ -e \"$repo\" ] && [ ! -L \"$repo\" ]; then\n\
             echo \"$repo exists in the guest and is not the repository mount\" >&2\n\
             exit 1\n\
         fi\n\
         mkdir -p \"$(dirname \"$repo\")\"\n\
         ln -sfn /repo \"$repo\"\n\
         [ -f \"$repo/Cargo.toml\" ]",
        guest_quote(&repo)
    );
    let out = vm("exec", &script)
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", vm_script().display()));
    assert!(
        out.status.success(),
        "the guest cannot see this repository at {repo}, so no oracle tool can read \
         the images a test writes. The harness mounts the consumer repository at /repo \
         on every boot (`chore vm:destroy` then `chore vm:up` re-provisions it).\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One argument, as the guest's shell will read it. Public so that
/// `tests/oracle_encoding.rs` can check it without a VM.
#[doc(hidden)]
pub fn guest_quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', r"'\''"))
}

/// A tool invocation, built and then run in the guest.
///
/// ```ignore
/// let out = oracle("e2fsck").args(["-fn", &image]).output();
/// assert_eq!(out.status.code(), Some(0));
/// ```
///
/// [`Output`] exactly as the guest produced it: the tool's own exit
/// status, its stdout and its stderr, kept apart.
#[must_use]
pub struct Oracle {
    tool: String,
    args: Vec<Arg>,
    env: Vec<(String, String)>,
    stdin: Option<Vec<u8>>,
}

/// Start building a call to `tool` (`mkfs.ext4`, `mke2fs`, `e2fsck`,
/// `fsck.ext4`, `debugfs`, `dumpe2fs`, `tune2fs`, `resize2fs`).
///
/// THE ONLY WAY A TEST REACHES AN ORACLE TOOL: `tests/test_contract.rs`
/// fails the build of the contract if a test spawns one itself, which
/// would run it on the host — a different version, a different platform,
/// and free to be absent.
pub fn oracle(tool: &str) -> Oracle {
    Oracle {
        tool: tool.to_string(),
        args: Vec::new(),
        env: Vec::new(),
        stdin: None,
    }
}

/// One argument, as the guest will receive it.
enum Arg {
    Text(String),
    Bytes(Vec<u8>),
}

impl Arg {
    /// Text when the bytes are text, and bytes when they are not: a
    /// directory entry name is any byte sequence but `/` and NUL, and
    /// the tests that check that hand `debugfs` exactly such names.
    #[track_caller]
    fn new(argument: &OsStr) -> Self {
        match argument.to_str() {
            Some(text) => Arg::Text(text.to_string()),
            None => {
                let bytes = argument.as_bytes().to_vec();
                // Bytes are decoded in the guest inside a command
                // substitution, which eats trailing newlines, so an
                // argument that ends in one is refused rather than
                // silently trimmed.
                assert!(
                    !bytes.ends_with(b"\n"),
                    "an argument that is not text and ends in a newline cannot be \
                     passed to the guest exactly"
                );
                Arg::Bytes(bytes)
            }
        }
    }

    /// The argument as the guest's shell must read it.
    fn shell(&self) -> String {
        match self {
            Arg::Text(text) => guest_quote(text),
            // Decoded in the guest, inside quotes, so the bytes reach
            // the tool exactly as they are here.
            Arg::Bytes(bytes) => format!(
                "\"$(printf %s {} | base64 -d)\"",
                guest_quote(&guest_base64(bytes))
            ),
        }
    }

    fn display(&self) -> String {
        match self {
            Arg::Text(text) => text.clone(),
            Arg::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        }
    }

    fn as_path(&self) -> Option<&str> {
        match self {
            Arg::Text(text) => Some(text),
            Arg::Bytes(_) => None,
        }
    }
}

impl Oracle {
    /// One argument. Anything a path or a name can be: `&str`,
    /// `String`, `&Path`, `PathBuf`, `OsString`.
    #[track_caller]
    pub fn arg(mut self, argument: impl AsRef<OsStr>) -> Self {
        self.args.push(Arg::new(argument.as_ref()));
        self
    }

    #[track_caller]
    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(arguments.into_iter().map(|a| Arg::new(a.as_ref())));
        self
    }

    /// An environment variable for the tool, in the guest.
    pub fn env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.to_string(), value.to_string()));
        self
    }

    /// Bytes on the tool's standard input (a `debugfs -f -` script).
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    /// Run it, and return what the tool did.
    ///
    /// A tool that reports on a volume is read before it is returned
    /// (#280, see [`crate::verdict`]): a `debugfs`, `dumpe2fs` or lwext4
    /// call that did not examine what it was asked about fails here, and
    /// `e2fsck` — a checker, whose exit status is not its verdict — is
    /// refused outright in favour of [`Oracle::judged`].
    #[track_caller]
    pub fn output(self) -> Output {
        let judge = Judge::of(&self.tool);
        assert!(
            judge != Some(Judge::E2fsck),
            "`{}` is a checker, and its exit status is not its verdict: `e2fsck -n` \
             exits 0 on a volume it skipped, one whose journal it did not replay, and \
             one it has just reported a wrong count on. Read it with \
             `oracle(\"e2fsck\")...judged()` or `assert_e2fsck_clean`.",
            self.tool
        );
        let (call, out) = self.run();
        if let Some(judge) = judge {
            if let Verdict::NotAVerdict(why) = judge.read_output(&out) {
                panic!("{call} did not examine what it was asked about: {why}");
            }
        }
        out
    }

    /// Run a tool that reports on a volume, and read its report as a
    /// [`Verdict`]. Never fails on the verdict itself: the caller says
    /// which one it requires ([`Judged::clean`], [`Judged::findings`],
    /// [`Judged::repaired`]) or matches on it.
    #[track_caller]
    pub fn judged(self) -> Judged {
        let Some(judge) = Judge::of(&self.tool) else {
            panic!(
                "`{}` makes volumes rather than reporting on them; there is no verdict \
                 to read. Use .output().",
                self.tool
            );
        };
        let (call, output) = self.run();
        Judged {
            verdict: judge.read_output(&output),
            call,
            output,
        }
    }

    #[track_caller]
    fn run(self) -> (String, Output) {
        session();
        for argument in self.args.iter().filter_map(Arg::as_path) {
            self.check_path(argument);
        }

        let run = Run::new();
        let out = guest_shell(&self.script(&run)).unwrap_or_else(|error| {
            panic!("cannot run the oracle tool in the guest: {error}");
        });
        let Some(code) = run.code() else {
            panic!(
                "the oracle tool `{}` could not be run in the fs-linux-test-harness VM \
                 (the harness exited {:?}).\n`chore vm:status` shows the VM; \
                 `chore vm:up` boots it.\n{}{}",
                self.tool,
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let (stdout, stderr) = run.streams();
        assert!(
            code != 127,
            "the oracle tool `{}` is not installed in the harness VM. \
             It is provisioned by scripts/vm-setup.sh; `chore vm:provision` \
             applies that script again. Tests never skip on a missing tool.\n{}",
            self.tool,
            String::from_utf8_lossy(&stderr)
        );

        // The evidence a green run carries: every tool call and its
        // verdict, printed by `chore test:oracle` (--show-output).
        let call = format!(
            "{} {}",
            self.tool,
            self.args
                .iter()
                .map(Arg::display)
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!("[oracle vm] {call} -> {code}");
        (
            call,
            Output {
                status: ExitStatusExt::from_raw(code << 8),
                stdout,
                stderr,
            },
        )
    }

    /// The shell the guest runs: the tool, its arguments unchanged, with
    /// its streams and its exit status captured in files both sides see.
    fn script(&self, run: &Run) -> String {
        let mut line = String::new();
        for (name, value) in &self.env {
            line.push_str(&format!("{name}={} ", guest_quote(value)));
        }
        line.push_str(&guest_quote(&self.tool));
        for argument in &self.args {
            line.push(' ');
            line.push_str(&argument.shell());
        }
        let redirect = format!(
            "> {} 2> {}",
            guest_quote(&run.stdout.to_string_lossy()),
            guest_quote(&run.stderr.to_string_lossy())
        );
        // A tool's own exit status is data here — `e2fsck` answers 1 for
        // "errors found", and the harness answers 1 for "no VM" — so it
        // travels in a file of its own rather than as the exit status of
        // the call, where the two would be the same number.
        let command = match &self.stdin {
            Some(bytes) => format!(
                "printf %s {} | base64 -d | {line} {redirect}",
                guest_quote(&guest_base64(bytes))
            ),
            None => format!("{line} {redirect}"),
        };
        format!(
            "mkdir -p {dir} && cd {repo} && {{ {command}; }}; printf %s $? > {status}",
            dir = guest_quote(&run.dir.to_string_lossy()),
            repo = guest_quote(&repo().to_string_lossy()),
            status = guest_quote(&run.status.to_string_lossy()),
        )
    }

    /// Everything a tool touches is inside this repository, because that
    /// is the tree the guest has. Caught here, where the rule can be
    /// explained, rather than in the guest as a missing file.
    #[track_caller]
    fn check_path(&self, argument: &str) {
        if !argument.starts_with('/') || !Path::new(argument).exists() {
            return;
        }
        let repo = repo();
        assert!(
            Path::new(argument).starts_with(repo),
            "`{}` was given {argument}, which is outside {}. The oracle tools run in \
             the harness VM, which sees this repository and nothing else of the host, \
             so a path outside it does not exist there. Put scratch files under \
             fs_ext4_test_support::temp_dir() (`temp_path!`), which is inside the \
             repository for exactly this reason.",
            self.tool,
            repo.display()
        );
    }
}

/// The three files one call leaves behind, named so that two calls —
/// from two threads or two test binaries — never share one.
pub(crate) struct Run {
    pub(crate) dir: PathBuf,
    pub(crate) stdout: PathBuf,
    pub(crate) stderr: PathBuf,
    pub(crate) status: PathBuf,
}

impl Run {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = crate::temp_dir().join("oracle");
        let name = format!(
            "{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            stdout: dir.join(format!("{name}.out")),
            stderr: dir.join(format!("{name}.err")),
            status: dir.join(format!("{name}.status")),
            dir,
        }
    }

    /// The tool's exit status, or `None` when the guest never ran it.
    pub(crate) fn code(&self) -> Option<i32> {
        let text = std::fs::read_to_string(&self.status).ok()?;
        let code = text.trim().parse().ok()?;
        let _ = std::fs::remove_file(&self.status);
        Some(code)
    }

    pub(crate) fn streams(&self) -> (Vec<u8>, Vec<u8>) {
        let read = |path: &PathBuf| {
            let bytes = std::fs::read(path).unwrap_or_default();
            let _ = std::fs::remove_file(path);
            bytes
        };
        (read(&self.stdout), read(&self.stderr))
    }
}

/// The encoding the guest's `base64 -d` reads, spelled out here rather
/// than pulled in as a dependency of the test support crate.
#[doc(hidden)]
pub fn guest_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut word = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            word |= u32::from(*byte) << (16 - 8 * i);
        }
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(word >> (18 - 6 * i)) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
