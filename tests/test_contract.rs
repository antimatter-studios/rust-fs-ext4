//! The test contract, checked: THE ORACLE TOOLS RUN IN THE HARNESS VM
//! AND NOWHERE ELSE, the kernel is only ever asked in the guest, and no
//! test announces a skip.
//!
//! Why the host is forbidden rather than merely second choice: e2fsprogs
//! on a workstation is whatever that machine has — a Homebrew keg on a
//! Mac, a distribution build on Linux, a different version per developer
//! — and on a Mac it is not the platform the images are for at all. One
//! version, in one guest, answers the same way for everyone. So a test
//! that spawns `e2fsck` itself is refused here even when it would work
//! on the machine that wrote it.
//!
//! `chore test:unit`, `chore test:oracle` and `chore test:kernel` are
//! chosen by `scripts/test-targets.sh` from what each test file calls:
//! `fs_ext4_test_support::oracle` / `assert_e2fsck_clean` for a tool,
//! the kernel helpers for a mount, `fixture` or a path under the fixture
//! directory for an image. That is only sound while those are the only
//! ways in: a test that spawned a tool by name would be classified as a
//! unit test, run on the `unit` CI job with no VM, and — worse — be free
//! to return early when the tool is absent, which is the silent pass the
//! contract exists to end. So this file reads every test source and
//! refuses every other shape.
//!
//! It names the patterns it looks for without spelling them out, so that
//! this file itself stays in the unit tier.

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.rs` file under `dir`, recursively, except the support crate
/// (which is where the sanctioned helpers live) and this file (whose
/// self-test spells out the shapes it refuses).
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "support") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") && !path.ends_with(file!()) {
            out.push(path);
        }
    }
}

fn all_test_sources() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    rust_sources(&manifest_dir().join("tests"), &mut files);
    rust_sources(&manifest_dir().join("src"), &mut files);
    assert!(
        files.len() > 100,
        "found only {} sources; the scan is looking in the wrong place",
        files.len()
    );
    files
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
            (p, text)
        })
        .collect()
}

/// The e2fsprogs programs the oracle tests use.
const TOOLS: [&str; 8] = [
    "mkfs.ext4",
    "mke2fs",
    "e2fsck",
    "fsck.ext4",
    "debugfs",
    "dumpe2fs",
    "tune2fs",
    "resize2fs",
];

/// Places in `text` where a process is spawned from a string literal that
/// names an oracle tool, or from a hard-coded sbin path.
fn direct_tool_spawns(text: &str) -> Vec<String> {
    let spawn = ["Command", "::", "new", "("].concat();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices(&spawn) {
        let rest = text[at + spawn.len()..].trim_start();
        let Some(literal) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = literal.find('"') else {
            continue;
        };
        let program = &literal[..end];
        let named = TOOLS.contains(&program)
            || program.contains("sbin/")
            || TOOLS.iter().any(|t| program.ends_with(&format!("/{t}")));
        if named {
            hits.push(program.to_string());
        }
    }
    // A probe of a fixed install path is how the old "is e2fsck here?"
    // skips found their tool.
    for line in text.lines() {
        let probe = ["\"/usr/", "sbin/"].concat();
        let probe_root = ["\"/", "sbin/"].concat();
        if !line.contains(&spawn)
            && (line.contains(&probe) || line.contains(&probe_root))
            && TOOLS.iter().any(|t| line.contains(t))
        {
            hits.push(line.trim().to_string());
        }
    }
    hits
}

/// Places in `text` that spawn a program NAMED BY A VARIABLE.
///
/// The scans above read the literal a process is spawned with, so a test
/// that puts the tool's name in a variable first would walk past them —
/// and that is not a hypothetical: every oracle test used to do exactly
/// that, with a `run(program, args)` helper. The only programs a test
/// spawns by computed name are this crate's own binaries, which come
/// from `CARGO_BIN_EXE_*`, so the rule is: a non-literal program must be
/// one of those, in the same file.
fn indirect_spawns(text: &str) -> Vec<String> {
    let spawn = ["Command", "::", "new", "("].concat();
    let own_binary = ["CARGO_BIN", "_EXE"].concat();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices(&spawn) {
        let rest = text[at + spawn.len()..].trim_start();
        if rest.starts_with('"') {
            continue;
        }
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            hits.push(rest.lines().next().unwrap_or_default().trim().to_string());
            continue;
        }
        // The binding it came from, wherever it is in the file.
        let bound_to_own_binary = text.lines().any(|line| {
            (line.contains(&format!("let {name} ="))
                || line.contains(&format!("let {name}:"))
                || line.contains(&format!("const {name}:")))
                && line.contains(&own_binary)
        });
        if !bound_to_own_binary {
            hits.push(name);
        }
    }
    hits
}

/// Programs that reach the VM or mount a filesystem. A test drives
/// neither itself: the harness is spoken to in one place (the support
/// crate), so there is one answer to "is the VM up", one place that
/// boots it, and no test that mounts anything on the machine running it.
const HARNESS: [&str; 6] = ["vagrant", "ssh", "mount", "umount", "losetup", "vm.sh"];

/// Places in `text` that spawn one of those.
fn harness_spawns(text: &str) -> Vec<String> {
    let spawn = ["Command", "::", "new", "("].concat();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices(&spawn) {
        let rest = text[at + spawn.len()..].trim_start();
        let Some(literal) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = literal.find('"') else {
            continue;
        };
        let program = &literal[..end];
        let last = program.rsplit('/').next().unwrap_or(program);
        if HARNESS.contains(&last) {
            hits.push(program.to_string());
        }
    }
    hits
}

/// Programs that run root. Nothing in this suite needs it — the mounts and
/// the tools happen in the guest — so asking for it is refused outright,
/// whatever it is then asked to do.
const ESCALATORS: [&str; 5] = ["sudo", "doas", "su", "pkexec", "run0"];

/// Programs whose job is to run ANOTHER program named in their arguments.
/// Every scan above reads only the program a process is spawned with, so
/// one of these in front of `e2fsck` or `mount` hides it from all of them
/// (#287). Refused outright: the arguments are not something a text scan
/// can be trusted to read.
const RUNNERS: [&str; 10] = [
    "env", "script", "xargs", "nohup", "timeout", "nice", "setsid", "stdbuf", "chroot", "unshare",
];

/// Shells. A shell's script is a string built at run time, so a `-c`
/// argument can carry anything; refused, except in the files named in
/// [`HOST_SHELL_ALLOWED`].
const SHELLS: [&str; 7] = ["sh", "bash", "zsh", "dash", "ksh", "fish", "busybox"];

/// The files that may spawn a shell on the host, and why. Each one's
/// shell spawns are still read, and refused if they name an oracle tool,
/// a harness program, an escalator or a runner. A shell a test needs for
/// anything else goes through `fs_ext4_test_support`, which runs it in
/// the guest.
const HOST_SHELL_ALLOWED: [(&str, &str); 1] = [(
    "tests/oracle_encoding.rs",
    "a POSIX shell is the independent decoder guest_quote is checked against; \
     its scripts are printf, base64 and od",
)];

/// The program a spawn at `at` names, if it is a string literal, and the
/// basename of it.
fn spawned_literal(text: &str, at: usize, spawn: &str) -> Option<(String, String)> {
    let rest = text[at + spawn.len()..].trim_start();
    let literal = rest.strip_prefix('"')?;
    let end = literal.find('"')?;
    let program = &literal[..end];
    let last = program.rsplit('/').next().unwrap_or(program);
    Some((program.to_string(), last.to_string()))
}

/// Places in `text` that spawn an escalator, a runner or a shell.
fn trampoline_spawns(text: &str) -> Vec<String> {
    let spawn = ["Command", "::", "new", "("].concat();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices(&spawn) {
        let Some((program, last)) = spawned_literal(text, at, &spawn) else {
            continue;
        };
        let last = last.as_str();
        if ESCALATORS.contains(&last) || RUNNERS.contains(&last) || SHELLS.contains(&last) {
            hits.push(program);
        }
    }
    hits
}

/// A string literal anywhere in `text` that is an escalator's name or
/// path. Belt to [`trampoline_spawns`]'s braces: it also sees one handed
/// to a spawn through a variable, an alias of `Command`, or an argument.
fn escalator_literals(text: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for e in ESCALATORS {
        for form in [format!("\"{e}\""), format!("/{e}\"")] {
            if text.contains(&form) {
                hits.push(e.to_string());
                break;
            }
        }
    }
    hits
}

/// Whether `word` appears in `text` as a whole word.
fn names_word(text: &str, word: &str) -> bool {
    let part_of_word = |c: char| c.is_alphanumeric() || c == '_' || c == '.' || c == '-';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(part_of_word) && !after.is_some_and(part_of_word)
    })
}

/// For each shell spawn in `text`, the text from the spawn to the call
/// that runs it — the program, its arguments and its script — and any
/// oracle tool, harness program, escalator or runner that text names.
fn shell_script_reaches(text: &str) -> Vec<String> {
    let spawn = ["Command", "::", "new", "("].concat();
    let forbidden: Vec<&str> = TOOLS
        .iter()
        .chain(HARNESS.iter())
        .chain(ESCALATORS.iter())
        .chain(RUNNERS.iter())
        .copied()
        .collect();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices(&spawn) {
        let Some((_, last)) = spawned_literal(text, at, &spawn) else {
            continue;
        };
        if !SHELLS.contains(&last.as_str()) {
            continue;
        }
        let chain = &text[at..];
        let end = [".output(", ".status(", ".spawn("]
            .iter()
            .filter_map(|run| chain.find(run))
            .min()
            .unwrap_or(chain.len());
        let chain = &chain[..end];
        for word in &forbidden {
            if names_word(chain, word) {
                hits.push(format!("{last} -c ... {word}"));
            }
        }
    }
    hits
}

/// Lines that print a skip notice: the signature of a test that returns
/// early and passes having checked nothing.
fn announced_skips(text: &str) -> Vec<String> {
    let print = ["eprint", "ln!("].concat();
    let lines: Vec<&str> = text.lines().collect();
    let mut hits = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains(&print) {
            continue;
        }
        // The message may sit on the next line or two after rustfmt.
        let window = lines[i..lines.len().min(i + 3)].join(" ").to_lowercase();
        if window.contains("skip") {
            hits.push(format!("line {}: {}", i + 1, line.trim()));
        }
    }
    hits
}

#[test]
fn no_test_runs_an_oracle_tool_on_the_host() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        for hit in direct_tool_spawns(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these run an oracle tool on the HOST. The tools live in the harness VM and \
         nowhere else: use fs_ext4_test_support::oracle, which runs them there and \
         fails, naming the task that fixes it, when it cannot:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_test_announces_a_skip() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        for hit in announced_skips(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these print a skip notice. A test never skips on a missing tool or \
         fixture; fail instead (fixture / oracle_tool do):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_test_drives_the_vm_or_mounts_a_filesystem_itself() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        for hit in harness_spawns(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these drive the VM or mount a filesystem themselves. The guest is reached \
         through fs_ext4_test_support (the oracle and kernel helpers), which boots it once \
         per process and keeps one connection; a mount happens only inside the \
         guest, never on the host:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_test_spawns_a_program_it_named_in_a_variable() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        for hit in indirect_spawns(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these spawn a program whose name is in a variable, which the checks above \
         cannot read. Only this crate's own binaries (CARGO_BIN_EXE_*) are spawned \
         that way; an oracle tool goes through fs_ext4_test_support::oracle:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_test_hands_its_work_to_root_a_shell_or_another_runner() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        let relative = path.strip_prefix(manifest_dir()).unwrap_or(&path);
        let shell_allowed = HOST_SHELL_ALLOWED
            .iter()
            .any(|(file, _)| relative == Path::new(file));
        for hit in trampoline_spawns(&text) {
            let last = hit.rsplit('/').next().unwrap_or(&hit);
            if shell_allowed && SHELLS.contains(&last) {
                continue;
            }
            offenders.push(format!("{}: {hit}", path.display()));
        }
        for hit in escalator_literals(&text) {
            offenders.push(format!("{}: \"{hit}\"", path.display()));
        }
        if shell_allowed {
            for hit in shell_script_reaches(&text) {
                offenders.push(format!("{}: {hit}", path.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these spawn root, a shell or a program that runs another one, which every \
         check above reads straight past: what it carries is an argument. Nothing here \
         needs root; a shell or a tool runs in the guest, through \
         fs_ext4_test_support:\n{}",
        offenders.join("\n")
    );
}

/// An allowance that no longer covers anything is a hole left open for
/// the next file to be given that name.
#[test]
fn every_host_shell_allowance_is_still_used() {
    for (file, why) in HOST_SHELL_ALLOWED {
        let text = std::fs::read_to_string(manifest_dir().join(file))
            .unwrap_or_else(|e| panic!("{file} is allowed a host shell ({why}) but: {e}"));
        let spawns_a_shell = trampoline_spawns(&text).iter().any(|hit| {
            let last = hit.rsplit('/').next().unwrap_or(hit);
            SHELLS.contains(&last)
        });
        assert!(
            spawns_a_shell,
            "{file} is allowed a host shell ({why}) and no longer spawns one; \
             remove it from HOST_SHELL_ALLOWED"
        );
    }
}

/// The scans find what they are for, so the two tests above cannot pass
/// by looking at nothing.
#[test]
fn the_scans_recognise_the_shapes_they_refuse() {
    let spawn = [
        "let out = Command",
        "::new(\"e2fsck\").arg(img).output();\n",
        "let dbg = Command",
        "::new(\"/usr/sbin/debugfs\");\n",
        "let ok = Command",
        "::new(tool).arg(img);\n",
        "let found = [\"/usr/",
        "sbin/e2fsck\", \"/",
        "sbin/e2fsck\"].into_iter().find(|p| exists(p));\n",
    ]
    .concat();
    let hits = direct_tool_spawns(&spawn);
    assert_eq!(hits.len(), 3, "{hits:?}");
    assert_eq!(
        hits[..2],
        ["e2fsck".to_string(), "/usr/sbin/debugfs".to_string()]
    );

    let indirect = [
        "const MKFS: &str = env!(\"CARGO_BIN",
        "_EXE_mkfs_ext4\");\n",
        "let out = Command",
        "::new(MKFS).output();\n",
        "let out = Command",
        "::new(tool).args(args).output();\n",
        "Command",
        "::new(\"sh\").arg(\"-c\");\n",
    ]
    .concat();
    assert_eq!(indirect_spawns(&indirect), ["tool".to_string()]);

    let harness = [
        "let vm = Command",
        "::new(\"../fs-linux-test-harness/scripts/vm.sh\");\n",
        "Command",
        "::new(\"mount\").args([\"-o\", \"loop\"]);\n",
        "Command",
        "::new(\"cargo\").arg(\"test\");\n",
    ]
    .concat();
    assert_eq!(
        harness_spawns(&harness),
        [
            "../fs-linux-test-harness/scripts/vm.sh".to_string(),
            "mount".to_string()
        ]
    );

    let skip = [
        "if missing {\n    eprint",
        "ln!(\n        \"SKIP: no image\"\n    );\n    return;\n}\n",
        "eprint",
        "ln!(\"note: took {ms} ms\");\n",
    ]
    .concat();
    assert_eq!(announced_skips(&skip).len(), 1);

    // #287: the oracle tools and a mount, run as root on the host, handed
    // to a privileged shell as one argument. Every scan above reads only
    // the program a process is spawned with, which here is the escalator.
    let escape = [
        "let out = Command",
        "::new(\"sudo\")\n",
        "    .args([\"-n\", \"bash\", \"-c\", ",
        "\"mkfs.ext4 -F x.img && mount -o loop x.img /mnt && e2fsck -fn x.img\"])\n",
        "    .output();\n",
    ]
    .concat();
    let caught = [
        direct_tool_spawns(&escape),
        indirect_spawns(&escape),
        harness_spawns(&escape),
        trampoline_spawns(&escape),
        escalator_literals(&escape),
    ]
    .concat();
    assert!(
        !caught.is_empty(),
        "a privileged shell carrying the oracle tools walks past every scan"
    );
    assert_eq!(trampoline_spawns(&escape), ["sudo".to_string()]);
    assert_eq!(escalator_literals(&escape), ["sudo".to_string()]);

    // The same, without root: a shell, a runner, an escalator by path,
    // and an escalator reached through an alias of `Command`.
    let trampolines = [
        "Command",
        "::new(\"bash\").args([\"-c\", \"e2fsck -fn x.img\"]);\n",
        "Command",
        "::new(\"/usr/bin/env\").arg(\"debugfs\");\n",
        "Command",
        "::new(\"/usr/bin/doas\").arg(\"true\");\n",
        "Command",
        "::new(\"cargo\").arg(\"test\");\n",
        "Cmd::new(\"pkexec\");\n",
    ]
    .concat();
    assert_eq!(
        trampoline_spawns(&trampolines),
        [
            "bash".to_string(),
            "/usr/bin/env".to_string(),
            "/usr/bin/doas".to_string()
        ]
    );
    assert_eq!(
        escalator_literals(&trampolines),
        ["doas".to_string(), "pkexec".to_string()]
    );

    // An allowed shell is still read: one that reaches a tool or a mount
    // is refused, one that prints is not.
    let shells = [
        "Command",
        "::new(\"sh\").arg(\"-c\").arg(format!(\"printf %s {}\", q)).output();\n",
        "Command",
        "::new(\"sh\").arg(\"-c\").arg(\"mount -o loop x.img /mnt\").output();\n",
        "Command",
        "::new(\"bash\")\n    .arg(\"-c\")\n    .arg(\"sudo e2fsck -fn x.img\")\n    .status();\n",
    ]
    .concat();
    assert_eq!(
        shell_script_reaches(&shells),
        [
            "sh -c ... mount".to_string(),
            "bash -c ... e2fsck".to_string(),
            "bash -c ... sudo".to_string()
        ]
    );
    // A word that only contains a name is not that name.
    assert!(!names_word("remount unmounted e2fsck.log", "mount"));
    assert!(!names_word("e2fsck.log", "e2fsck"));
}
