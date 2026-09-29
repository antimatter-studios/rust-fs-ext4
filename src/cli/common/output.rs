//! What a tool prints: a JSON result by default, text on request, and a
//! structured error on stderr whose code is the exit status.
//!
//! JSON is written here rather than through serde: the values are a
//! handful of scalars, arrays and objects, and a writer this size keeps
//! the tools' dependencies to their argument parser.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Write as _;
use std::process::ExitCode;

use clap::ArgMatches;

/// Exit status: the operation failed.
pub const EXIT_FAILED: u8 = 1;
/// Exit status: the command line was wrong (clap's own choice as well).
pub const EXIT_USAGE: u8 = 2;
/// Exit status: the verb exists, and this driver cannot do it — not
/// implemented yet, or the format is read-only.
pub const EXIT_UNSUPPORTED: u8 = 3;

/// A JSON value, with object keys kept in the order they were added so
/// the output reads the way the code builds it.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// An object from `(key, value)` pairs.
    pub fn object<K: Into<String>>(pairs: impl IntoIterator<Item = (K, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// The value under `key`, for an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Indented JSON, two spaces a level, and no trailing newline.
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write_pretty(&self, out: &mut String, depth: usize) {
        let pad = |out: &mut String, depth: usize| out.push_str(&"  ".repeat(depth));
        match self {
            Json::Arr(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    pad(out, depth + 1);
                    item.write_pretty(out, depth + 1);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push(']');
            }
            Json::Obj(pairs) if !pairs.is_empty() => {
                out.push_str("{\n");
                for (i, (key, value)) in pairs.iter().enumerate() {
                    pad(out, depth + 1);
                    write_string(out, key);
                    out.push_str(": ");
                    value.write_pretty(out, depth + 1);
                    out.push_str(if i + 1 < pairs.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push('}');
            }
            other => other.write_compact(out),
        }
    }

    /// JSON on one line.
    pub fn to_compact(&self) -> String {
        let mut out = String::new();
        self.write_compact(&mut out);
        out
    }

    fn write_compact(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Json::UInt(n) => {
                let _ = write!(out, "{n}");
            }
            Json::Str(s) => write_string(out, s),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    item.write_compact(out);
                }
                out.push(']');
            }
            Json::Obj(pairs) => {
                out.push('{');
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_string(out, key);
                    out.push_str(": ");
                    value.write_compact(out);
                }
                out.push('}');
            }
        }
    }

    /// The same value for a person: a scalar is itself (a string without
    /// quotes, `null` as nothing), an object is `key: value` lines with
    /// nested keys dotted (`ext4.uuid: ...`), and an array is one line per
    /// element.
    pub fn to_text(&self) -> String {
        let mut lines = Vec::new();
        self.text_lines("", &mut lines);
        lines.join("\n")
    }

    fn text_lines(&self, prefix: &str, lines: &mut Vec<String>) {
        match self {
            Json::Obj(pairs) => {
                for (key, value) in pairs {
                    let dotted = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    match value {
                        Json::Obj(_) => value.text_lines(&dotted, lines),
                        _ => lines.push(format!("{dotted}: {}", value.scalar_text())),
                    }
                }
            }
            Json::Arr(items) => {
                for item in items {
                    lines.push(item.scalar_text());
                }
            }
            scalar => lines.push(scalar.scalar_text()),
        }
    }

    fn scalar_text(&self) -> String {
        match self {
            Json::Null => String::new(),
            Json::Bool(b) => b.to_string(),
            Json::Int(n) => n.to_string(),
            Json::UInt(n) => n.to_string(),
            Json::Str(s) => s.clone(),
            Json::Arr(items) => items
                .iter()
                .map(Json::scalar_text)
                .collect::<Vec<_>>()
                .join(","),
            Json::Obj(pairs) => pairs
                .iter()
                .map(|(k, v)| format!("{k}={}", v.scalar_text()))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}
impl From<u64> for Json {
    fn from(n: u64) -> Json {
        Json::UInt(n)
    }
}
impl From<u32> for Json {
    fn from(n: u32) -> Json {
        Json::UInt(n.into())
    }
}
impl From<u16> for Json {
    fn from(n: u16) -> Json {
        Json::UInt(n.into())
    }
}
impl From<i64> for Json {
    fn from(n: i64) -> Json {
        Json::Int(n)
    }
}
impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::Str(s.to_string())
    }
}
impl From<String> for Json {
    fn from(s: String) -> Json {
        Json::Str(s)
    }
}
impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(v: Option<T>) -> Json {
        v.map_or(Json::Null, Into::into)
    }
}
impl<T: Into<Json>> From<Vec<T>> for Json {
    fn from(v: Vec<T>) -> Json {
        Json::Arr(v.into_iter().map(Into::into).collect())
    }
}

/// JSON or text, from `--json`/`--text` (see [`super::format_args`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Json,
    Text,
}

impl Format {
    /// The format asked for anywhere on the command line: a subcommand's
    /// `--text` counts as much as the tool's, and the deepest one given
    /// decides.
    pub fn of(matches: &ArgMatches) -> Format {
        let mut format = Format::Json;
        let mut level = Some(matches);
        while let Some(m) = level {
            if flag(m, "text") {
                format = Format::Text;
            } else if flag(m, "json") {
                format = Format::Json;
            }
            level = m.subcommand().map(|(_, sub)| sub);
        }
        format
    }
}

fn flag(matches: &ArgMatches, id: &str) -> bool {
    matches!(matches.try_get_one::<bool>(id), Ok(Some(true)))
        && matches.value_source(id) == Some(clap::parser::ValueSource::CommandLine)
}

/// Whether a raw command line asks for text, for the one case with no
/// parse to read it from: the parse failed.
pub fn text_requested(argv: &[OsString]) -> bool {
    argv.iter()
        .rev()
        .find(|a| *a == "--text" || *a == "--json")
        .is_some_and(|a| a == "--text")
}

/// A successful run's result.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Printed on stdout: pretty JSON, or its text form.
    pub report: Option<Json>,
    /// The text form, when the tool has a better one than
    /// [`Json::to_text`].
    pub text: Option<String>,
    /// The exit status, for a tool whose success has more than one answer
    /// (fsck's "corrected"). Zero otherwise.
    pub code: u8,
}

impl Outcome {
    /// A result to print.
    pub fn report(report: Json) -> Outcome {
        Outcome {
            report: Some(report),
            ..Outcome::default()
        }
    }

    /// Nothing to print: the tool wrote its output itself (raw bytes).
    pub fn done() -> Outcome {
        Outcome::default()
    }

    /// Use `text` for `--text` instead of the generic rendering.
    pub fn with_text(mut self, text: impl Into<String>) -> Outcome {
        self.text = Some(text.into());
        self
    }

    /// Exit with `code` although the run succeeded.
    pub fn with_code(mut self, code: u8) -> Outcome {
        self.code = code;
        self
    }
}

/// A failure: printed as `{"error": message, "code": code}` on stderr (or
/// `<tool>: <message>` with `--text`), and `code` is the exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub message: String,
    pub code: u8,
}

impl CliError {
    /// The operation failed (exit 1).
    pub fn failed(message: impl Into<String>) -> CliError {
        CliError {
            message: message.into(),
            code: EXIT_FAILED,
        }
    }

    /// The command line was wrong (exit 2).
    pub fn usage(message: impl Into<String>) -> CliError {
        CliError {
            message: message.into(),
            code: EXIT_USAGE,
        }
    }

    /// The verb exists and this driver cannot do it yet (exit 3). The
    /// message starts `not implemented`, which scripts may match on.
    pub fn not_implemented(what: impl Into<String>) -> CliError {
        CliError {
            message: format!("not implemented: {}", what.into()),
            code: EXIT_UNSUPPORTED,
        }
    }

    /// The verb exists and the format cannot do it (exit 3).
    pub fn refused(message: impl Into<String>) -> CliError {
        CliError {
            message: message.into(),
            code: EXIT_UNSUPPORTED,
        }
    }

    /// A failure with a tool-specific status (fsck's 8).
    pub fn with_code(mut self, code: u8) -> CliError {
        self.code = code;
        self
    }

    /// The JSON form.
    pub fn to_json(&self) -> Json {
        Json::object([
            ("error", Json::from(self.message.as_str())),
            ("code", Json::from(u64::from(self.code))),
        ])
    }
}

/// Print a run's result or failure and turn it into the exit status.
pub fn finish(program: &str, format: Format, result: Result<Outcome, CliError>) -> ExitCode {
    match result {
        Ok(outcome) => {
            let printed = match (&outcome.report, format) {
                (None, _) => None,
                (Some(report), Format::Json) => Some(report.to_pretty()),
                (Some(report), Format::Text) => {
                    Some(outcome.text.clone().unwrap_or_else(|| report.to_text()))
                }
            };
            // An empty text form prints nothing at all, not an empty line:
            // `mkfs.ext4 --text` stays as silent on stdout as it always was.
            if let Some(printed) = printed.filter(|p| !p.is_empty()) {
                let mut stdout = std::io::stdout().lock();
                // A closed pipe (`| head`) is the reader's choice, not a failure.
                let _ = writeln!(stdout, "{printed}");
                let _ = stdout.flush();
            }
            ExitCode::from(outcome.code)
        }
        Err(error) => {
            let mut stderr = std::io::stderr().lock();
            let _ = match format {
                Format::Json => writeln!(stderr, "{}", error.to_json().to_compact()),
                Format::Text => writeln!(stderr, "{program}: {}", error.message),
            };
            ExitCode::from(error.code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_escaped_and_control_characters_survive() {
        let v = Json::from("a\"b\\c\nd\u{1}é");
        assert_eq!(v.to_compact(), r#""a\"b\\c\nd\u0001é""#);
    }

    #[test]
    fn objects_keep_their_order_and_nest_in_text_with_dots() {
        let v = Json::object([
            ("fs", Json::from("ext4")),
            ("label", Json::Null),
            ("ext4", Json::object([("uuid", Json::from("x"))])),
        ]);
        assert_eq!(
            v.to_compact(),
            r#"{"fs": "ext4", "label": null, "ext4": {"uuid": "x"}}"#
        );
        assert_eq!(v.to_text(), "fs: ext4\nlabel: \next4.uuid: x");
        assert_eq!(
            v.to_pretty(),
            "{\n  \"fs\": \"ext4\",\n  \"label\": null,\n  \"ext4\": {\n    \"uuid\": \"x\"\n  }\n}"
        );
    }

    #[test]
    fn the_last_format_flag_on_a_raw_command_line_wins() {
        let argv = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(text_requested(&argv(&["x", "--json", "--text"])));
        assert!(!text_requested(&argv(&["x", "--text", "--json"])));
        assert!(!text_requested(&argv(&["x"])));
    }
}
