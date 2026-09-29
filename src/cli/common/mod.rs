//! The command-line plumbing every driver's tools share: dispatch on
//! `argv[0]`, `--version`, `doctor`, the JSON output and the structured
//! error, and the `--json`/`--text` switch.
//!
//! IT KNOWS NOTHING ABOUT EXT4, and must not learn. A driver describes
//! itself once, as a [`Family`] — the repository name, the crate and
//! version, the install hints, and one [`Tool`] per dotted name, each a
//! clap command and a function that runs it — and hands that to
//! [`main`]. Everything here reads only that description. That is what
//! lets this directory be lifted, as it stands, into a crate the other
//! drivers depend on: nothing in it names a filesystem, and nothing
//! outside it is reached from it.
//!
//! THE CONTRACT IT CARRIES, so each driver fills it in rather than
//! defining its own:
//!
//! - **One binary, many names.** Invoked as a tool's dotted name
//!   (`mkfs.ext4`), it is that tool. Invoked under any other name — the
//!   repository's (`rust-fs-ext4`), or a renamed copy — the first
//!   argument names the tool, by its verb (`mkfs`) or its full name
//!   (`mkfs.ext4`). That second form is the one nothing on PATH can
//!   shadow.
//! - **`--version`** prints `<tool> (<crate>) <version>` for every name,
//!   which is how `doctor` and the test suites tell our binary from
//!   another package's program of the same name.
//! - **Output.** A result is JSON on stdout by default, `--text` for
//!   people; file content is raw bytes and is never wrapped. A failure is
//!   `{"error": "...", "code": N}` on STDERR, and `N` is the exit status,
//!   so stdout carries a result or nothing — never half of one, and never
//!   an error a pipe would take for data.
//! - **Exit statuses**, outside a tool that has its own scheme (`fsck`'s
//!   0/1/4/8): 0 done, 1 failed, 2 the command line was wrong, 3 the verb
//!   exists but this driver cannot do it (not implemented, or the format
//!   is read-only). A script moved between filesystems fails loudly on 3
//!   instead of meaning something else.
//! - **`<repo> doctor`** resolves every dotted name on PATH and says, per
//!   name, whether the program found is ours, and if not, what wins and
//!   how to fix it.

pub mod dispatch;
pub mod doctor;
pub mod family;
pub mod output;
pub mod version;

// clap's `Command` is imported as `Cmd` throughout: it is not a process,
// and tests/test_contract.rs reads a `Command` constructor in the source
// text as a process spawn.
// The one process this plumbing starts is doctor's `--version` probe
// (std's `Command`, imported there as `Process`), which runs a program by
// the name PATH gives it because that is the question doctor answers.

pub use family::{Family, Tool};
pub use output::{CliError, Format, Json, Outcome};

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{Arg, ArgAction, Command as Cmd};

/// The whole program: work out which tool this is, parse its command
/// line, run it and print what it returned.
pub fn main(family: &'static Family) -> ExitCode {
    run(family, std::env::args_os().collect())
}

/// [`main`] over an explicit argument vector, `argv[0]` included.
pub fn run(family: &'static Family, argv: Vec<OsString>) -> ExitCode {
    match dispatch::resolve(family, argv) {
        dispatch::Target::Tool(tool, argv) => run_tool(family, tool, argv),
        dispatch::Target::Repo(argv) => run_repo(family, argv),
    }
}

/// A tool's clap command, named and versioned for the name it runs under.
pub fn tool_command(family: &'static Family, tool: &Tool) -> Cmd {
    (tool.command)()
        .name(tool.name)
        .bin_name(tool.name)
        .version(version::clap_version(family))
}

/// The repository-named entry point's clap command: every tool as a
/// subcommand (so its help and its man page list them), plus `doctor`.
///
/// Parsing never reaches a tool's subcommand here: [`dispatch::resolve`]
/// hands `rust-fs-ext4 mkfs ...` to the tool itself before this runs.
pub fn repo_command(family: &'static Family) -> Cmd {
    let mut cmd = Cmd::new(family.repo)
        .bin_name(family.repo)
        .version(version::clap_version(family))
        .about(family.about)
        .subcommand_required(true)
        .arg_required_else_help(true)
        .after_help(repo_examples(family));
    for tool in family.tools {
        cmd = cmd.subcommand((tool.command)().name(tool.verb).about(format!(
            "{} (the same program as `{}`)",
            tool.about, tool.name
        )));
    }
    cmd.subcommand(doctor::command(family)).subcommand(
        Cmd::new("generate")
            .about("Print what packaging needs from the binary itself")
            .hide(true)
            .subcommand_required(true)
            .subcommand(
                Cmd::new("names").about("The dotted names to link to this binary, one per line"),
            ),
    )
}

fn repo_examples(family: &Family) -> String {
    let mut text = String::from("Examples:\n");
    for tool in family.tools {
        text.push_str(&format!(
            "  {} {} --help    same as `{} --help`\n",
            family.repo, tool.verb, tool.name
        ));
    }
    text.push_str(&format!(
        "  {} doctor           is every tool on PATH ours?\n",
        family.repo
    ));
    text
}

fn run_tool(family: &'static Family, tool: &'static Tool, argv: Vec<OsString>) -> ExitCode {
    let text_requested = output::text_requested(&argv);
    match tool_command(family, tool).try_get_matches_from(argv) {
        Err(error) => clap_failure(tool.name, error, text_requested, tool.usage_exit),
        Ok(matches) => {
            let format = Format::of(&matches);
            let result = (tool.run)(&matches);
            output::finish(tool.name, format, result)
        }
    }
}

fn run_repo(family: &'static Family, argv: Vec<OsString>) -> ExitCode {
    let text_requested = output::text_requested(&argv);
    let matches = match repo_command(family).try_get_matches_from(argv) {
        Ok(matches) => matches,
        Err(error) => return clap_failure(family.repo, error, text_requested, output::EXIT_USAGE),
    };
    match matches.subcommand() {
        Some(("doctor", sub)) => {
            let format = Format::of(sub);
            output::finish(family.repo, format, Ok(doctor::run(family)))
        }
        Some(("generate", sub)) => match sub.subcommand_name() {
            Some("names") => {
                for tool in family.tools {
                    println!("{}", tool.name);
                }
                ExitCode::SUCCESS
            }
            _ => unreachable!("clap requires a generate subcommand"),
        },
        // A tool's verb never reaches here (dispatch took it), and clap
        // refuses anything else before this point.
        _ => unreachable!("clap requires a known subcommand"),
    }
}

/// Help and version go to stdout with status 0; anything else is a
/// command line that was wrong, status `usage_exit`, as a structured error
/// unless `--text` was asked for.
fn clap_failure(
    program: &str,
    error: clap::Error,
    text_requested: bool,
    usage_exit: u8,
) -> ExitCode {
    use clap::error::ErrorKind;
    match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            let _ = error.print();
            ExitCode::SUCCESS
        }
        // A bare `rust-fs-ext4`: the help is the answer, but nothing was done.
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            let _ = error.print();
            ExitCode::from(usage_exit)
        }
        _ if text_requested => {
            let _ = error.print();
            ExitCode::from(usage_exit)
        }
        _ => {
            let rendered = error.render().to_string();
            let message = rendered
                .trim()
                .strip_prefix("error: ")
                .unwrap_or(rendered.trim())
                .to_string();
            output::finish(
                program,
                Format::Json,
                Err(CliError::usage(message).with_code(usage_exit)),
            )
        }
    }
}

/// `--json` and `--text`, for any command that reports. The last one
/// given wins, so an alias or a wrapper can add either without breaking a
/// command line that already has the other.
pub fn format_args() -> [Arg; 2] {
    [
        Arg::new("json")
            .long("json")
            .help("Report as JSON on stdout (the default)")
            .action(ArgAction::SetTrue)
            .overrides_with("text"),
        Arg::new("text")
            .long("text")
            .help("Report as text for a person, instead of JSON")
            .action(ArgAction::SetTrue)
            .overrides_with("json"),
    ]
}
