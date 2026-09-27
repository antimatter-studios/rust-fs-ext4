//! READING AN ORACLE'S REPORT AS A VERDICT, OR REFUSING TO (#280).
//!
//! Every outside opinion this suite holds itself to arrives as text and
//! an exit status, and the exit status is not the verdict. Measured
//! against e2fsprogs 1.47.0 in the harness guest, each of these exits 0:
//!
//! - `e2fsck -n` on a volume marked clean: `clean, 12/4096 files` and no
//!   pass was run. Without `-f` it only glanced at the superblock flag.
//! - `e2fsck -fn` on a volume whose journal needs recovery: `Warning:
//!   skipping journal recovery because doing a read-only filesystem
//!   check.` It then graded the structures the journal was about to
//!   replace.
//! - `e2fsck -fn` on a volume with a wrong free-blocks count: `Free blocks
//!   count wrong (1, counted=2805). Fix? no` — a finding, reported, and
//!   exit 0 anyway.
//! - `e2fsck -fn` on a volume whose group descriptor checksum is wrong:
//!   `One or more block group descriptor checksums are invalid. Fix? no`.
//! - `debugfs -R "cat /nope"` on any image, and `debugfs` on an image that
//!   is not ext4 at all: debugfs reports a failed request on stderr and
//!   exits 0 whatever happened.
//!
//! and `lwext4-report` says `ext4_mount (read-only): 5 (Input/output
//! error)` for an image that does not exist, which reads exactly like
//! lwext4 refusing a volume for a feature it lacks.
//!
//! So a report is read into one of three outcomes, and only the first is
//! a pass:
//!
//! - [`Verdict::Clean`]: the tool examined the volume and has nothing
//!   against it;
//! - [`Verdict::Findings`]: it examined the volume and says something is
//!   wrong — the failure an oracle test exists to catch, and the outcome
//!   a test that damages a volume on purpose requires;
//! - [`Verdict::NotAVerdict`]: it did not examine what it was asked to —
//!   it could not open the image, it skipped the check, it declined to
//!   replay, it was asked the wrong question. Neither its silence nor its
//!   complaints say anything about this driver, so it is never read as
//!   clean, and never as a finding either.
//!
//! [`crate::oracle`] applies this to every call of a tool named here:
//! `e2fsck` is only reachable through [`crate::Oracle::judged`], and a
//! `debugfs`, `dumpe2fs` or lwext4 call that comes back without a verdict
//! fails where it was made.

use std::process::Output;

/// What an oracle's report says about the volume it was asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The tool examined the volume and has nothing against it.
    Clean,
    /// The tool examined the volume and reports something wrong with it.
    /// The text is the part of the report that says so.
    Findings(String),
    /// The tool did not examine what it was asked about, so its report is
    /// not an answer. The text says why.
    NotAVerdict(String),
}

/// The tools whose reports this module knows how to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judge {
    /// `e2fsck` (and `fsck.ext2`/`3`/`4`, the same program): a checker.
    E2fsck,
    /// `debugfs`: a reader, and a writer with `-w`.
    Debugfs,
    /// `dumpe2fs`: a reader of the superblock and group descriptors.
    Dumpe2fs,
    /// `lwext4-report` (`tests/lwext4/report.c`): a third implementation's
    /// account of a volume.
    Lwext4,
}

impl Judge {
    /// The reader for `tool`, or `None` for a tool that makes a volume
    /// (`mkfs.ext4`, `tune2fs`) rather than reporting on one.
    pub fn of(tool: &str) -> Option<Judge> {
        let name = tool.rsplit('/').next().unwrap_or(tool);
        match name {
            "e2fsck" | "fsck.ext2" | "fsck.ext3" | "fsck.ext4" => Some(Judge::E2fsck),
            "debugfs" => Some(Judge::Debugfs),
            "dumpe2fs" => Some(Judge::Dumpe2fs),
            "lwext4-report" => Some(Judge::Lwext4),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Judge::E2fsck => "e2fsck",
            Judge::Debugfs => "debugfs",
            Judge::Dumpe2fs => "dumpe2fs",
            Judge::Lwext4 => "lwext4-report",
        }
    }

    /// Read one report: the tool's exit status (`None` when a signal
    /// ended it) and its two streams.
    pub fn read(self, code: Option<i32>, stdout: &str, stderr: &str) -> Verdict {
        let Some(code) = code else {
            return Verdict::NotAVerdict(format!(
                "{} was ended by a signal before it answered",
                self.name()
            ));
        };
        match self {
            Judge::E2fsck => e2fsck(code, stdout, stderr),
            Judge::Debugfs => debugfs(code, stderr),
            Judge::Dumpe2fs => dumpe2fs(code, stdout, stderr),
            Judge::Lwext4 => lwext4(code, stdout, stderr),
        }
    }

    /// [`Judge::read`] over a finished process.
    pub fn read_output(self, out: &Output) -> Verdict {
        self.read(
            out.status.code(),
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        )
    }
}

/// The last pass e2fsck runs. A report without it is a report of a check
/// that did not happen: e2fsck without `-f` on a volume marked clean
/// prints one status line and exits 0.
const E2FSCK_LAST_PASS: &str = "Pass 5: Checking group summary information";

/// What e2fsck says when it checks a volume without replaying its
/// journal, which `-n` never does.
const E2FSCK_SKIPPED_REPLAY: &str = "skipping journal recovery";

/// The ways e2fsck says it found something, beside its exit status —
/// which `-n` leaves at 0 for a count it was told not to fix.
const E2FSCK_FINDINGS: [&str; 4] = [
    "IGNORED",
    "still has errors",
    "FILE SYSTEM WAS MODIFIED",
    "HTREE",
];

fn e2fsck(code: i32, stdout: &str, stderr: &str) -> Verdict {
    let report = format!("{stdout}{stderr}");
    // fsck's exit status is a bit set: 1 corrected, 2 corrected and
    // needs a reboot, 4 left uncorrected, 8 operational error, 16 usage,
    // 32 cancelled, 128 shared library error.
    if !(0..=15).contains(&code) || code & 8 != 0 {
        if code & 8 != 0 && code < 16 && checksum_refusal(stderr) {
            // Refused to open the volume because a checksum is wrong:
            // e2fsck looked, and that is what it found.
            return Verdict::Findings(report);
        }
        return Verdict::NotAVerdict(format!(
            "e2fsck exited {code} ({}), so it never finished checking the volume:\n{report}",
            fsck_exit_meaning(code)
        ));
    }
    if report.contains(E2FSCK_SKIPPED_REPLAY) {
        return Verdict::NotAVerdict(format!(
            "e2fsck checked a volume whose journal it did not replay, so it graded \
             structures the journal was about to replace. Replay first (mount it, or \
             `e2fsck -fy` a copy), then check:\n{report}"
        ));
    }
    if !report.contains(E2FSCK_LAST_PASS) {
        return Verdict::NotAVerdict(format!(
            "e2fsck did not run its passes (no `{E2FSCK_LAST_PASS}`). Without `-f` a \
             volume marked clean is only glanced at:\n{report}"
        ));
    }
    // Every question e2fsck asks is about something it found, except
    // `Optimize?`: `extent tree (at level 1) could be narrower. Optimize?
    // no` is a suggestion about a valid tree, which e2fsck itself does not
    // count toward its exit status.
    let answered = report.lines().any(|line| {
        let line = line.trim_end();
        (line.ends_with("? no") || line.ends_with("? yes"))
            && !line.ends_with("Optimize? no")
            && !line.ends_with("Optimize? yes")
    });
    if code & 7 != 0 || answered || E2FSCK_FINDINGS.iter().any(|f| report.contains(f)) {
        return Verdict::Findings(report);
    }
    Verdict::Clean
}

fn fsck_exit_meaning(code: i32) -> &'static str {
    if code & 128 != 0 {
        "shared library error"
    } else if code & 32 != 0 {
        "cancelled"
    } else if code & 16 != 0 {
        "usage or syntax error"
    } else if code & 8 != 0 {
        "operational error"
    } else {
        "not an fsck exit status"
    }
}

/// A tool that would not open the volume because a checksum over it is
/// wrong: `Superblock checksum does not match superblock while trying to
/// open`.
fn checksum_refusal(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|l| l.contains("checksum does not match") || l.contains("Checksum errors"))
}

/// The lines of `stderr` that are not the version banner every e2fsprogs
/// tool prints first (`debugfs 1.47.0 (5-Feb-2023)`).
fn complaints<'a>(tool: &str, stderr: &'a str) -> Vec<&'a str> {
    stderr
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            !(l.starts_with(tool)
                && l.split_whitespace()
                    .nth(1)
                    .is_some_and(|version| version.starts_with(|c: char| c.is_ascii_digit())))
        })
        .collect()
}

fn debugfs(code: i32, stderr: &str) -> Verdict {
    let said = complaints("debugfs", stderr);
    if code != 0 {
        return Verdict::NotAVerdict(format!("debugfs exited {code}:\n{stderr}"));
    }
    if said.is_empty() {
        return Verdict::Clean;
    }
    // debugfs exits 0 whatever became of the request, and says what went
    // wrong on stderr: `File not found by ext2_lookup`, `Ext2 inode is not
    // a directory`, `Bad magic number in super-block while trying to
    // open`, `Filesystem not open`. Only a checksum complaint is a
    // statement about the volume; every other line is a request that was
    // not carried out.
    if said
        .iter()
        .all(|l| l.contains("checksum does not match") || l.contains("Checksum errors"))
    {
        return Verdict::Findings(said.join("\n"));
    }
    Verdict::NotAVerdict(format!(
        "debugfs did not carry out the request (it exits 0 regardless):\n{}",
        said.join("\n")
    ))
}

fn dumpe2fs(code: i32, stdout: &str, stderr: &str) -> Verdict {
    let said = complaints("dumpe2fs", stderr);
    if checksum_refusal(stderr) {
        return Verdict::Findings(said.join("\n"));
    }
    if code != 0 || !said.is_empty() {
        return Verdict::NotAVerdict(format!(
            "dumpe2fs exited {code} and did not describe the volume:\n{stdout}{stderr}"
        ));
    }
    // A group descriptor whose checksum is wrong is printed with the
    // value it should have had: `csum 0x1234 (EXPECTED 0xd2c4)`.
    let wrong: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("(EXPECTED "))
        .collect();
    if !wrong.is_empty() {
        return Verdict::Findings(wrong.join("\n"));
    }
    Verdict::Clean
}

/// The kinds of line `tests/lwext4/report.c` prints.
const LWEXT4_KINDS: [&str; 5] = ["type", "mode", "size", "sha256", "target"];

fn lwext4(code: i32, stdout: &str, stderr: &str) -> Verdict {
    match code {
        0 => {
            if !stderr.trim().is_empty() {
                return Verdict::NotAVerdict(format!(
                    "lwext4-report exited 0 but complained:\n{stderr}"
                ));
            }
            let malformed: Vec<&str> = stdout
                .lines()
                .filter(|line| {
                    let fields: Vec<&str> = line.splitn(3, '\t').collect();
                    fields.len() != 3 || !LWEXT4_KINDS.contains(&fields[0]) || fields[1].is_empty()
                })
                .collect();
            if !malformed.is_empty() {
                return Verdict::NotAVerdict(format!(
                    "lwext4-report printed lines that are not `kind<TAB>path<TAB>value`:\n{}",
                    malformed.join("\n")
                ));
            }
            Verdict::Clean
        }
        // `die()`: an lwext4 call returned an error. At mount, or on a
        // path inside the volume, that is lwext4's statement about the
        // volume. Anything else — usage, the block device, unmount — is
        // the reporter failing to ask.
        2 => {
            let said = stderr.trim();
            let about_the_volume = said.lines().count() == 1
                && (said.starts_with("lwext4-report: ext4_mount ")
                    || said.starts_with("lwext4-report: /mp/"));
            if about_the_volume {
                Verdict::Findings(said.to_string())
            } else {
                Verdict::NotAVerdict(format!(
                    "lwext4-report failed before it said anything about the volume:\n{said}"
                ))
            }
        }
        _ => Verdict::NotAVerdict(format!(
            "lwext4-report exited {code}, which it never does on its own:\n{stdout}{stderr}"
        )),
    }
}

/// A report, and what it was read as. What [`crate::Oracle::judged`]
/// returns.
#[must_use]
pub struct Judged {
    /// The call, as the `[oracle vm]` line prints it.
    pub call: String,
    pub verdict: Verdict,
    pub output: Output,
}

impl Judged {
    /// Everything the tool printed, stdout then stderr.
    pub fn report(&self) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&self.output.stdout),
            String::from_utf8_lossy(&self.output.stderr)
        )
    }

    /// The tool examined the volume and found nothing wrong, or the test
    /// fails with its report. The output, for a test that reads it.
    #[track_caller]
    pub fn clean(self, what: &str) -> Output {
        match &self.verdict {
            Verdict::Clean => self.output,
            Verdict::Findings(said) => panic!(
                "[{what}] {} found something wrong with the volume:\n{said}\n\
                 (exit {:?})",
                self.call,
                self.output.status.code()
            ),
            Verdict::NotAVerdict(why) => panic!(
                "[{what}] {} is not a verdict on the volume, so it is not read as \
                 clean: {why}",
                self.call
            ),
        }
    }

    /// The tool examined the volume and said something is wrong with it,
    /// or the test fails. For the tests that damage a volume on purpose:
    /// a tool that could not look has not caught anything.
    #[track_caller]
    pub fn findings(self, what: &str) -> String {
        match self.verdict {
            Verdict::Findings(said) => said,
            Verdict::Clean => panic!(
                "[{what}] {} was expected to find something wrong, and called the \
                 volume clean:\n{}",
                self.call,
                self.report()
            ),
            Verdict::NotAVerdict(why) => panic!(
                "[{what}] {} was expected to find something wrong, and did not examine \
                 the volume at all: {why}",
                self.call
            ),
        }
    }

    /// The tool examined the volume — clean, or with findings it may have
    /// repaired — and did not decline. For `e2fsck -fy` run to repair or
    /// replay a volume, where exit 1 is the tool doing its job.
    #[track_caller]
    pub fn examined(self, what: &str) -> Output {
        if let Verdict::NotAVerdict(why) = &self.verdict {
            panic!("[{what}] {} did not examine the volume: {why}", self.call);
        }
        self.output
    }

    /// `e2fsck -fy` repaired whatever it found and left nothing standing
    /// (exit 0 or 1), or the test fails. Everything it printed, for a test
    /// that reads the transcript.
    #[track_caller]
    pub fn repaired(self, what: &str) -> String {
        let code = self.output.status.code();
        let report = self.report();
        let call = self.call.clone();
        self.examined(what);
        assert!(
            matches!(code, Some(0 | 1)),
            "[{what}] {call} left errors it could not repair (exit {code:?}):\n{report}"
        );
        report
    }
}
