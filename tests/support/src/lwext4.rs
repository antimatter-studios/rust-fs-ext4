//! LWEXT4: A THIRD IMPLEMENTATION OF EXT4, ASKED IN THE GUEST.
//!
//! The suite already has two outside opinions. e2fsprogs
//! ([`crate::oracle`]) is a second reader of the format, written by the
//! project that wrote the kernel's, from the same specification — so an
//! ambiguity both of us read the same way is one neither of us can see.
//! The kernel ([`crate::kernel`]) is the thing the images are actually
//! for, which makes it the authority and not an independent check.
//!
//! lwext4 (BSD-2-Clause, github.com/gkostka/lwext4) is neither: a
//! pure-C ext2/3/4 implementation with no code lineage in common with
//! the kernel or with this crate. Where it and this crate disagree
//! about the same bytes, one of the two has read the format wrong. That
//! is the whole reason it is here (#99).
//!
//! WHERE IT RUNS, AND WHY THERE. lwext4 is a portable C library, so
//! cross-validating against it needs a machine, not another operating
//! system — and this repository already has that machine: the
//! fs-linux-test-harness guest, where e2fsprogs and the kernel oracles
//! live. `scripts/vm-setup.sh` builds lwext4 there at [`PIN`], and this
//! module compiles `tests/lwext4/report.c` against it in the same guest
//! and runs it through [`crate::oracle`]. Two FreeBSD scaffolds used to
//! stand in for this and neither ever ran (#269); they are gone.
//!
//! NOTHING SKIPS. A guest without lwext4, a guest built from a different
//! revision than [`PIN`], a reporter that will not compile: each fails
//! the test that needed it, naming `chore vm:provision`, which applies
//! `scripts/vm-setup.sh` again.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::oracle::{guest_quote, guest_shell, repo, session};
use crate::verdict::{Judged, Verdict};

/// The lwext4 revision the guest builds, and the only one this module
/// will talk to.
///
/// IT IS ALSO IN `scripts/vm-setup.sh`, which does the building, and
/// `tests/lwext4_cross_validate.rs` fails if the two strings differ. A
/// pin recorded in one place and acted on in another is a pin that
/// drifts; this way a bump has to be made twice or not at all.
pub const PIN: &str = "58bcf89a121b72d4fb66334f1693d3b30e4cb9c5";

/// Where the guest keeps what `scripts/vm-setup.sh` built.
const PREFIX: &str = "/usr/local";

/// What lwext4 reported about one path, keyed the way the kernel oracle
/// keys its own report: `(kind, path) -> value`.
///
/// `kind` is one of `type`, `mode`, `size`, `sha256`, `target`; `path`
/// is relative to the root of the volume, with no leading slash.
pub type Report = BTreeMap<(String, String), String>;

/// The reporter, compiled in the guest against the pinned lwext4, once
/// per test process. Returns its path, which is the same on both sides.
///
/// Compiled rather than provisioned: `tests/lwext4/report.c` is ours and
/// changes with the tests, while the harness re-runs the setup script
/// only when the SETUP SCRIPT changes. Building the library there (slow,
/// pinned, stable) and the reporter here (a second, and always the
/// source in this checkout) is what keeps the two in step.
fn reporter() -> &'static str {
    static REPORTER: OnceLock<String> = OnceLock::new();
    REPORTER.get_or_init(|| {
        session();
        let repo = repo().to_string_lossy().into_owned();
        let out = format!("{repo}/tmp/lwext4-report");
        let script = format!(
            "set -eu\n\
             pin=\"$(cat {prefix}/lib/lwext4.pin 2>/dev/null || true)\"\n\
             if [ \"$pin\" != {pin} ]; then\n\
                 echo \"the guest has lwext4 '${{pin:-none}}'\" >&2\n\
                 exit 3\n\
             fi\n\
             cd {repo}\n\
             mkdir -p tmp\n\
             cc -std=gnu99 -O2 -Wall -Wextra -Werror -o tmp/.lwext4-report.$$ \\\n\
                 -I{prefix}/include/lwext4 tests/lwext4/report.c \\\n\
                 -L{prefix}/lib -llwext4 -lblockdev\n\
             mv tmp/.lwext4-report.$$ {out}\n",
            prefix = PREFIX,
            pin = guest_quote(PIN),
            repo = guest_quote(&repo),
            out = guest_quote(&out),
        );
        let result = guest_shell(&script)
            .unwrap_or_else(|error| panic!("cannot reach the harness guest: {error}"));
        assert!(
            result.status.success(),
            "the lwext4 cross-validation reporter could not be built in the harness VM. \
             lwext4 is built there by scripts/vm-setup.sh at {PIN}; `chore vm:provision` \
             applies that script again, and `chore vm:destroy` then `chore vm:up` \
             rebuilds the guest from scratch. Tests never skip on a missing oracle.\n{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        out
    })
}

/// `lwext4-report <mode> <image>`, as the guest ran it, and read (#280).
///
/// THE IMAGE HAS TO EXIST FIRST. lwext4 opens its file lazily, so a path
/// that names nothing comes back as `ext4_mount (read-only): 5
/// (Input/output error)` — word for word what a refusal of a real volume
/// looks like. Checked here, where it can be said plainly.
#[track_caller]
fn run(mode: &str, image: &str) -> Judged {
    assert!(
        std::fs::metadata(image).is_ok_and(|m| m.is_file() && m.len() > 0),
        "{image} does not exist or is empty, so lwext4 has no volume to report on. \
         Whatever it said about it would be about nothing."
    );
    crate::oracle(reporter()).args([mode, image]).judged()
}

/// `kind<TAB>path<TAB>value` lines, as the reporter prints them.
fn parse(text: &str) -> Report {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.splitn(3, '\t');
            let kind = fields.next().unwrap_or_default().to_string();
            let path = fields.next().unwrap_or_default().to_string();
            let value = fields.next().unwrap_or_default().to_string();
            ((kind, path), value)
        })
        .collect()
}

/// EVERYTHING lwext4 SEES IN `image`: for every path below the root, its
/// type, its permission bits, and — for a regular file — its size and
/// the SHA-256 of its contents, or — for a symlink — its target.
///
/// One guest call walks the whole volume. A failure to mount or to read
/// fails the test with what lwext4 said; [`refusal`] is the way to ask
/// about an image lwext4 is expected to turn down.
#[track_caller]
pub fn lwext4_report(image: &str, what: &str) -> Report {
    let out = run("read", image).clean(what);
    parse(&String::from_utf8_lossy(&out.stdout))
}

/// lwext4 REFUSED `image`, and this is what it said.
///
/// Not a skip and not a tolerated failure: the images lwext4 cannot read
/// are the ones using a feature it does not implement, and a test names
/// them and requires the refusal. An image that starts mounting — because
/// the pin moved, or because the fixture changed — fails here, which is
/// the signal to move it into the compared set rather than to notice
/// months later that it was never compared at all.
#[track_caller]
pub fn lwext4_refusal(image: &str, what: &str) -> String {
    let judged = run("read", image);
    match judged.verdict {
        Verdict::Findings(said) => said,
        Verdict::Clean => panic!(
            "[{what}] lwext4 was expected to refuse {image}, and it read it. \
             Move it into the compared set.\n{}",
            String::from_utf8_lossy(&judged.output.stdout)
        ),
        Verdict::NotAVerdict(why) => panic!(
            "[{what}] lwext4 was expected to refuse {image}, and failed without \
             examining it, which is not a refusal: {why}"
        ),
    }
}

/// THE OTHER DIRECTION: lwext4 writes a known tree into `image`, and
/// reports what it wrote.
///
/// The returned map is lwext4's own account of the tree — the same
/// `(kind, path) -> value` shape [`lwext4_report`] returns — so a test
/// mounts the image with this crate and compares against the writer
/// rather than against a copy of the expectation kept somewhere else.
#[track_caller]
pub fn lwext4_write(image: &str, what: &str) -> Report {
    let out = run("write", image).clean(what);
    parse(&String::from_utf8_lossy(&out.stdout))
}
