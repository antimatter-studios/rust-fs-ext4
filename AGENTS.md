# Working in rust-fs-ext4 (agent guide)

Pure-Rust ext2/3/4 driver (`fs-ext4`) exposing a C ABI (`fs_ext4_*`), validated
against e2fsprogs and the in-kernel ext4 driver, both inside the harness VM.
This file is the fast path for an agent picking up work here, so the workflow
does not have to be re-derived each time. It points at the existing docs rather
than duplicating them:

- **README** → `## Test contract` (suite shape), `## Building`, `### Testing`.
- **docs/TEST-DISKS.md** → the `test-disks/*.img` fixtures and their `.meta.txt`.
- **`chores.yml`** → every task named below, and what each one actually runs.

The section between the BEGIN/END markers below is **shared, byte-identical,
with every repository in this family**. Do not edit it here: change the
canonical copy and propagate it, or `scripts/agents-core-check.sh` will fail.
Everything after the END marker is specific to this repository.

<!-- BEGIN SHARED BLOCK: agent-core v2 sha256:38af4d2c5377d38ab382baa4eab4aa679841e2b4eba4f4d01dacd255ffa7d32e -->
## Claiming work

Several agents work these repositories at the same time. Before you start on
an issue, claim it, so nobody else spends a session on what you are already
doing. The lock is a **GitHub label**, because labels are shared state that
every agent can read and change without posting comments into the thread.

**Before starting.** Check, claim, then read back:

```sh
gh issue view <N> --json labels                      # holds `claimed`? pick another
gh issue edit <N> --add-label claimed --add-label claim/<session>
gh issue view <N> --json labels                      # read back and confirm
```

`<session>` is your session name — `agent-<random4>-<isodate>`, e.g.
`agent-3f7c-2026-09-22`. Create the `claim/<session>` label if it does not
exist.

**Resolving a race.** Adding a label is not compare-and-swap: two agents can
both add `claimed` and both believe they won. That is what the read-back is
for. If it shows more than one `claim/*` label, the **lexically lowest**
session keeps the issue; every other agent removes its own `claim/*` label and
picks different work. Each racer computes the same answer independently, so no
further coordination is needed.

**When you finish or stop.** Remove both labels — on merge, or the moment you
abandon the work:

```sh
gh issue edit <N> --remove-label claimed --remove-label claim/<session>
```

Delete your `claim/<session>` label from the repository at the end of your
session so they do not accumulate.

**Reclaiming a stale claim.** An agent that dies holding a claim would block an
issue forever. If `claimed` was applied more than 12 hours ago and the holder's
branch has no commits since, any agent may take it: remove the stale `claim/*`,
add your own, and say so in the issue.

**This is a convention, not a fence.** Nothing enforces it. An agent that
ignores it duplicates work; it cannot corrupt anything. Honour it anyway.

## Work in a worktree

Every working copy is a **git worktree** of an existing checkout, made with
`git worktree add`. Never `git clone` a second, unlinked copy — not for a
branch, a PR, a review, or a sibling you need at another ref:

```sh
git -C <checkout> fetch origin
git -C <checkout> worktree add <path> -b <type>/<name> origin/main   # new work
git -C <checkout> worktree add --detach <path> <tag>                 # a sibling at a pinned ref
git -C <checkout> worktree remove <path>                             # when done
```

A worktree shares the checkout's objects and remotes, and `git worktree list`
shows it to every agent on the machine, so nobody else mistakes it for
abandoned work or loses track of it. An unlinked clone copies all the history
again, is invisible to that list, and gets left behind in `/tmp` long after the
work that made it is merged. Remove your worktree when you finish.

## Skills to use

- **`dev-loop`** — the required loop for any non-trivial change: baseline the
  full suite → change → re-run (no baseline test may regress) → enhance tests →
  vet. Always run it.
- **`commit`** / **`pr`** — for grouping commits and opening pull requests.

Each repository names any further skills of its own below.

## A bug fix starts with a red

**Prove it is broken first** — a failing check or test — *then* fix it, *then*
prove that same check is green, *then* confirm the full baseline still passes.
Never write the fix before you have a red. A fix with no failing test to its
name is a claim, not a result.

## Nothing skips

A test that cannot run **fails**, naming the task that would provide what it
needed. Never add an early return for a missing fixture, tool or VM: a skipped
test reads exactly like a passing one, and a suite that quietly declines to run
is indistinguishable from a suite that passes.

Where a tier reports skips or ignored tests, that is a gate, not a note.

## Validate against something that is not us

A driver's own readers share its interpretation of the format, so they cannot
catch a misreading: the mistake is baked into the fixture *and* the parser, and
they agree with each other while disagreeing with every real filesystem. Unit
tests over self-built fixtures prove self-consistency, not correctness.

Every structure that is parsed or written gets a cross-validation test against
an **independent oracle** — the platform's own tools, a real kernel, or a third
implementation — before it is considered done. Each repository names its
oracles below.

## Output is budgeted

Test tiers run through `scripts/tier.sh`, which runs the suite **quietly**: the
whole run goes to `tmp/logs/<tier>.log`, a pass prints one verdict line naming
that log, and a failure prints the verdict, the command's status and the log's
path — `--tail N`, or `OUTPUT_BUDGET_FAIL_TAIL=N`, prints the tail for whoever
is watching. **Read the log**: a failing tier names it and does not recite it.
CI keeps the logs as an artifact, so the detail is always retrievable.

The budget caps the log, not merely what is shown, and every number in the
table was measured. A run that passes but prints more than its budget **fails**.

The reader who pays most for a noisy suite is an agent that re-reads its whole
transcript on every step, and so pays for one loud run many times over. If a
tier legitimately grows, raise its row **with the measurement that justifies
it**. Do not silence output to fit, and do not route around `tier.sh`.

## Commits and branches

- Branches are `<type>/<name>`, matching the commit type: `fix/`, `feat/`,
  `ci/`, `docs/`, `chore/`, `test/`.
- A commit is a subject plus flat one-sentence bullets. Subjects are
  declarative, not imperative: "the run-end bound is checked", not "check the
  run-end bound".
- **No AI attribution and no co-author trailers**, in commits or in pull
  request descriptions.
- `main` takes **squash merges only**.

## Project rules

- **No GPL/LGPL/AGPL dependencies.** Permissive only (MIT/BSD/Apache).
  Shelling out to a copyleft CLI as a *test oracle* is fine — linking or
  copying it is not.
- **Each of these is a standalone project.** Never mention a consuming
  application in the README, the source, or CLI help.
<!-- END SHARED BLOCK: agent-core v2 -->


## Clean room

This crate is MIT-licensed, and it can only stay that way if nothing in it
is derived from copyleft source. "No GPL dependencies" above is about
linking; this is about **reading**. It binds every contributor, human or
agent, on every task.

**Never open, and never reproduce:**

- **Linux kernel source.** Its ext4, JBD2 and Unicode code, its headers,
  its library routines (CRCs included), in any form: a checkout, a web
  viewer, a mirror, a patch on a mailing list, a quote in an issue.
- **e2fsprogs source.** libext2fs, e2fsck, debugfs, mke2fs, in any form.
  The *tools* are fine; their source is not.
- **The previous, non-clean implementations in this repository.** Code
  replaced because a provenance audit found it derived, or too close to be
  sure of, is off limits in its old form: the htree name hashes in
  `src/hash.rs` from before their clean-room rewrite, and the routines
  restated alongside them. Not in git history, not in another checkout,
  not in a published crate.
- **Your own memory of any of the above.** A model has read the kernel.
  Recalling a routine's shape or its name and writing it down is copying
  with extra steps. If you notice you are about to, stop, and derive the
  answer from the sources below instead.

**What you may use:**

- **Public specifications.** The ext4 and JBD2 format documentation as
  published on docs.kernel.org / kernel.org (the rendered documentation,
  not the source tree it is built from), the ext4 wiki, RFCs (RFC 1320
  MD4, RFC 3720 CRC32C), papers (TEA, SipHash) and the Unicode Character
  Database.
- **Permissively licensed code** (BSD, MIT, Apache), credited.
- **Black-box oracles.** Run `mke2fs`, `e2fsck`, `debugfs`, `dumpe2fs`, a
  real kernel mount and the other oracles below, and compare what they
  print and the bytes they write. Observing what a tool does is not
  copying it. Where the documentation is silent, find out this way: have
  a tool write the structure, read the bytes, and pin what you learned
  with a test.

**Write down the fact, not whose code does it.** A comment or doc states
the on-disk rule or the observed behaviour and cites where it comes from:
the documentation section, or the oracle test that shows it. It never
names a kernel or e2fsprogs function, an internal macro or a source file,
and never links into either source tree. "Mirrors the kernel's function
so-and-so in such-and-such a file" makes code read as derived whether or
not it is, and sends the next reader to open that file. Names that belong to the documented format are
fine: `struct ext4_extent_header`, `journal_header_t`, `s_inodes_count`,
`EXT4_EXTENTS_FL`.

**The check.** `scripts/check-provenance.sh` (`chore check:provenance`,
run by `chore lint` and by CI's `provenance` job) fails on kernel and
e2fsprogs source paths and links, on kernel and e2fsck function names,
kernel-internal macros and e2fsck problem codes anywhere in the tracked
tree, file names included, and on C written out in a comment or a doc. A
documented format name it flags goes into the script's allowlist with the
reason; nothing else does, and an entry nothing uses fails the check. It
is a denylist: passing it is necessary, not sufficient.


## Running tests

The tasks are the interface; CI runs exactly these (`chores.yml` has the
contract at its top):

```sh
chore siblings        # ../rust-fs-core and ../fs-linux-test-harness at their pinned refs
chore tools           # what the HOST needs: ripgrep, and the VM (Vagrant/QEMU/KVM)
chore fixtures        # build test-disks/*.img in the fs-linux-test-harness VM
chore test:unit       # no tools, no fixtures, no VM (debug; traps overflows)
chore test:images     # reads a fixture, needs no VM
chore test:oracle     # e2fsck / debugfs / mke2fs — run INSIDE the VM
chore test:kernel     # the real kernel loop-mounts our images and reads them back
chore test:lwext4     # lwext4, a third implementation, built in the VM — both ways
chore test:wasm       # the browser build (wasm32), run under Node (`chore tools:wasm` first)
chore test:vm         # the whole suite, compiled and run INSIDE the VM
chore test            # everything, as CI runs it
chore lint            # fmt + clippy -D warnings

./scripts/test.sh --test <name>   # one integration binary, e.g. repro_wants_dir_symlinks
```

**The oracle tools are never run on the host.** Not on a Mac, not on
Linux, not even where they are installed. e2fsprogs on a workstation is
whatever that machine has, and an oracle whose answer depends on the
machine is not an oracle — so they live in one Debian guest
(`scripts/vm-setup.sh` installs them) and `fs_ext4_test_support::oracle`
is the only way in. `tests/test_contract.rs` fails the suite if a test
spawns one itself, drives the VM itself, or mounts anything on the host.
The VM boots once per run (the first oracle call), every later call rides
one shared SSH connection (~0.1 s per tool invocation), and the chore
reaper stops it when the invocation ends.

**We run the Linux tests on Linux.** On a Linux host `chore test` runs
them here; on macOS it runs `chore test:vm`, which builds and runs the
same sources inside the guest (the harness mounts the repository there at
the path the host knows it by). Nothing Linux-shaped runs natively on a
Mac.

**Nothing skips.** A test that needs a fixture gets it from
`fs_ext4_test_support::fixture`, one that needs a tool from `oracle` /
`assert_e2fsck_clean`, one that needs the kernel from the guest-kernel
helpers; all of them fail, naming the task that provides it, when it is
missing. Never add an early return for a missing image, tool or VM: a
skipped test reads exactly like a passing one.
`scripts/test-targets.sh` derives the tiers from those calls; a library
test that needs a fixture or the VM goes in a `needs_host` module so
`chore test:unit` can leave it to `chore test`.

Use `scripts/test.sh` for local runs: it gives every run its own scratch
directory and cleans it afterward. **That directory is inside the
repository** (`./tmp`, gitignored) on every machine, because the guest
sees this repository and nothing else of the host — an image under `/tmp`
or `$RUNNER_TEMP` would not exist for the tool asked to read it.
`FS_EXT4_TEST_TMPDIR` names an exact directory instead, and is refused if
it is outside the repository.

Install the hooks once per clone with github-guard's installer:
`~/.claude/skills/github-guard/install.sh .` (formats with `cargo fmt` and runs
`cargo clippy --all-targets -- -D warnings` on every commit).


## What gates a merge

**One required check, and it is `ci-ok`.** Branch protection on `main`
requires that context and nothing else:

```sh
gh api repos/christhomas/rust-fs-ext4/branches/main/protection/required_status_checks
# {"strict":true,"contexts":["ci-ok"], ...}
```

`ci-ok` in `ci.yml` `needs:` every other job and fails when any of them
failed, was cancelled **or was skipped**, so a job that is renamed,
split or dropped turns the gate red in the same diff that does it. That
is the point of naming one: the list in a settings page nobody re-reads
cannot drift away from the workflow.

Do not add job names to the required list. A required context nothing
reports reads as "Expected — waiting for status to be reported" for
ever, which blocks every pull request while telling nobody why — this
repository sat that way with three stale names (#263), one of them from
a workflow that no longer existed.

Nothing in the tree can check this: protection is not a file. If a
merge is blocked with every check green, read the required contexts
first.


## Adding a test (the in-tree pattern)

Integration tests copy a fixture, drive the driver via `apply_*`, then reopen
read-only and assert. Canonical templates:
`tests/journal_writer_create_mkdir_link_symlink.rs`,
`tests/repro_wants_dir_symlinks.rs`.

```rust
let path = copy_to_tmp("ext4-csum-seed.img", "tag"); // fixture → unique scratch copy
{
    let dev = FileDevice::open_rw(&path)?;
    let fs = Filesystem::mount(Arc::new(dev))?;           // replays journal if dirty
    fs.apply_mkdir("/d", 0o755)?;
    fs.apply_symlink("../x", "/d/x")?;
    fs.apply_unlink("/d/x")?;
}                                                          // drop → unmount/flush
// reopen read-only and assert: jbd2::read_superblock().is_clean(),
// path::lookup(...), and — for checksum work — recompute & compare the
// on-disk checksum (see assert_jsb_checksum_valid in repro_wants_dir_symlinks.rs).
```

**`is_clean()` is not sufficient on its own** — it only checks `jsb.start == 0`.
A bad-but-marked-clean checksum passes it. For checksum bugs, either recompute
the specific checksum in-process (jsb example above) or cross-check with a real
ext4 (below).


## The oracles: e2fsprogs and the kernel, both in the harness VM

The driver shares this crate's spec interpretation, so its own
`verify::verify` / `fsck::audit` (structural: link counts, dirents, free-count
drift) **cannot** catch metadata_csum / journal-checksum / `itable_unused` bugs,
and no reader of ours can prove the bytes it wrote are the bytes on disk.
Independent tools can:

- **`e2fsck -fn`** — consistency. `assert_e2fsck_clean(image, tag)` in the
  test support crate. **Exit `0` is not "clean"**: `-n` exits 0 having
  answered "no" to `Free blocks count wrong`, having skipped the replay of a
  journal it then graded, and (without `-f`) having run no pass at all. Every
  report is read by `fs_ext4_test_support::verdict` into `Clean`, `Findings`
  or `NotAVerdict`, and only `Clean` passes; `oracle("e2fsck")` is reachable
  only through `.judged()` (#280). `tests/oracle_verdicts.rs` holds the reader
  to reports captured from the tools in the guest, and
  `tests/oracle_verdicts_live.rs` to the tools themselves.
- **`debugfs`** — content and metadata: `dump` + compare, `stat`, `ex`,
  `icheck`, `ncheck`, `logdump`. `tests/oracle_debugfs.rs` is the template,
  including the negative case that proves why both are needed (a flipped
  data byte passes e2fsck and fails the dump comparison).
  Both are reached through `fs_ext4_test_support::oracle(tool)`, which runs
  them in the guest and returns the tool's own `Output` — same exit status,
  same streams, no host path to fall back to. debugfs exits 0 on a request it
  could not carry out, so a `debugfs`, `dumpe2fs` or lwext4 call whose report
  is not a verdict fails where it was made.
- **The kernel itself**, through `guest_kernel_report` /
  `guest_kernel_write` in the test support crate: our image is loop-mounted
  in the guest and a script walks it, hashes every file and reads xattrs
  and ACLs back in ONE guest call. `tests/kernel_readback.rs` (Rust API),
  `tests/kernel_readback_capi.rs` (C ABI). This is what catches what
  `e2fsck` cannot — an ACL blob with the wrong version, an extent the
  kernel maps shorter than we wrote it — and it is the only place a
  filesystem is mounted at all.
- **The fixtures** are kernel-made the same way: `chore fixtures` runs
  `test-disks/guest-build-images.sh` as root in the guest (loop mounts,
  xattrs, ACLs, inline data, htree directories) through the
  [fs-linux-test-harness](https://github.com/antimatter-studios/fs-linux-test-harness)
  VM, whose tooling `scripts/vm-setup.sh` installs. For anything else in
  the guest: `chore vm:run -- <command>`, `chore vm:exec -- <command>` (the
  fast path, for a VM that is already up), `chore vm:put <file>` (lands in
  `/share`), `chore vm:down`. One VM runs at a time across every repository
  on the machine; `chore vm:slot:status` says who has it.
- `tests/lwext4_cross_validate.rs` + `tests/lwext4/report.c` — A THIRD
  IMPLEMENTATION. lwext4 (BSD-2-Clause, pure C) is built in the same guest
  by `scripts/vm-setup.sh` at a pinned commit, and the two read each
  other's images: every fixture it can mount, a tree this crate wrote, and
  a tree it wrote itself. `chore test:lwext4`.

To check a driver-mutated image by hand, ask the guest — the tools are
there, and it sees this repository at the same path the host does:

```sh
chore vm:up                                   # boot and hold it
chore vm:run -- e2fsck -fn "$PWD/tmp/x.img"
chore vm:run -- debugfs -R "'stat /path'" "$PWD/tmp/x.img"
chore vm:down
```


## Build environment

- The build cache (`target/`) is large. If the disk is tight, set
  `CARGO_TARGET_DIR` to a roomier volume — do **not** clean unrelated projects'
  `target/` dirs.
- Toolchain is pinned (`rust-toolchain.toml`); the hook enforces
  `clippy -D warnings` (e.g. `manual_div_ceil` → use `.div_ceil(n)`).


## Worked example: the metadata_csum write-path fixes

A real bug surfaced from the field (a Bookworm SD card whose journal the kernel
rejected after a write). Reproduced with `tests/repro_wants_dir_symlinks.rs` on
`ext4-csum-seed.img`, then fixed as a stack, each step proven red→green with
a real `e2fsck` and the full baseline:

1. **jbd2 superblock checksum** not recomputed in `journal_writer::write_jsb`.
2. **`bg_itable_unused`** not maintained on inode alloc (`buffer_mark_inode_used`).
3. **inode/block bitmap checksums** not recomputed on bitmap change
   (`buffer_refresh_bitmap_csum` + the four bitmap-mutating ops).
4. **inode checksum** not `i_extra_isize`-aware in `compute_inode_checksum`
   (zeroed/freed inodes need the 16-bit lo-only form).

The pattern to copy: `metadata_csum` writes must recompute **every** affected
checksum (superblock, group descriptor, bitmaps, inode, journal) — and the only
reliable proof is a real `e2fsck`, not the driver's own readers.

## The gate is a script, not a test

`chore check:ci-gate` holds both halves of the aggregate mechanically — every
job in `ci.yml` must appear in `ci-ok`'s `needs:`, and `.github-guard` must
require `ci-ok` and nothing else. The task runs `scripts/core.sh ci-gate` and
nothing else, so the script is what can be tested, reviewed and run without
`chore` at all.

It replaced `tests/ci_aggregate_gate.rs`, which was the wrong container twice
over: it parsed a YAML file and compared strings, exercising nothing this crate
ships, and as a `cargo test` it counted towards the **executed-test floor the
gate itself enforces** — so the suite could satisfy its own floor partly by
checking its own CI config.

Judging mergeability from check **conclusions** is unreliable: an in-progress
`CheckRun` reports its conclusion as an empty string, and a `StatusContext` has
no conclusion field at all. Read `mergeStateStatus` and
`statusCheckRollup.state`.

## Never grow a shared tool to solve a problem in this repository

`chore` is a general-purpose task runner this project merely consumes; the same
goes for `github-guard` and the agent-skills hooks. If something needed here
looks like it belongs inside one of them, it does not. Solve it here, or ask
first. The tell is a release: if a shared tool needs a new version cut whose
only purpose is to unblock this project, the code is in the wrong repository.

## The shared block above is checked

`scripts/agents-core-check.sh` hashes the content between the BEGIN/END markers
and compares it against the canonical digest. Editing the block here fails that
check rather than quietly giving this repository its own rules. When the block
genuinely changes it changes everywhere: edit the canonical copy, then update
the digest in the script and in the BEGIN marker of every repository.
