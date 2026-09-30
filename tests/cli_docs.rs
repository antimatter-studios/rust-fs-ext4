//! The man pages and shell completions the binary writes for packaging
//! (`rust-fs-ext4 generate man|completions SHARE`): one page per name in
//! its section, each tool's page naming every one of its subcommands, and
//! a zsh, bash and fish completion per name.

mod cli_support;

use cli_support::*;
use std::path::PathBuf;

fn generated(tag: &str) -> PathBuf {
    let share = PathBuf::from(fs_ext4_test_support::temp_path!(
        "cli-docs-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&share);
    let share_arg = share.to_string_lossy().into_owned();
    ok(entry().args(["generate", "man", &share_arg]));
    ok(entry().args(["generate", "completions", &share_arg]));
    share
}

fn section(name: &str) -> u8 {
    if name.starts_with("mkfs.") || name.starts_with("fsck.") {
        8
    } else {
        1
    }
}

#[test]
fn every_name_has_a_man_page_in_its_section_naming_its_subcommands() {
    let share = generated("man");
    let mut names = dotted_names();
    names.push("rust-fs-ext4".to_string());
    for name in &names {
        let s = section(name);
        let page = share.join(format!("man/man{s}/{name}.{s}"));
        let text =
            std::fs::read_to_string(&page).unwrap_or_else(|e| panic!("{}: {e}", page.display()));
        assert!(text.contains(".TH "), "{name}: not a man page");
        assert!(text.contains("Examples:"), "{name}: no example");
        // Every subcommand --help lists is named on the page.
        let help = stdout(&ok(tool(name).arg("--help")));
        let verbs: Vec<&str> = help
            .lines()
            .skip_while(|l| !l.starts_with("Commands:"))
            .skip(1)
            .take_while(|l| l.starts_with("  "))
            .filter_map(|l| l.split_whitespace().next())
            .filter(|v| *v != "help")
            .collect();
        for verb in verbs {
            assert!(text.contains(verb), "{name}'s page does not mention {verb}");
        }
    }
    // fs.ext4's subcommands have pages of their own, which its list names.
    for verb in [
        "ls", "read", "write", "mkdir", "get", "info", "set", "resize",
    ] {
        assert!(
            share.join(format!("man/man1/fs.ext4-{verb}.1")).is_file(),
            "no fs.ext4-{verb}.1"
        );
    }
}

#[test]
fn every_name_has_a_zsh_bash_and_fish_completion() {
    let share = generated("completions");
    let mut names = dotted_names();
    names.push("rust-fs-ext4".to_string());
    for name in &names {
        let zsh = std::fs::read_to_string(share.join(format!("zsh/site-functions/_{name}")))
            .unwrap_or_else(|e| panic!("zsh {name}: {e}"));
        assert!(
            zsh.starts_with(&format!("#compdef {name}")),
            "zsh {name}: {}",
            &zsh[..40.min(zsh.len())]
        );
        let bash =
            std::fs::read_to_string(share.join(format!("bash-completion/completions/{name}")))
                .unwrap_or_else(|e| panic!("bash {name}: {e}"));
        assert!(bash.contains("complete -F"), "bash {name}");
        let fish =
            std::fs::read_to_string(share.join(format!("fish/vendor_completions.d/{name}.fish")))
                .unwrap_or_else(|e| panic!("fish {name}: {e}"));
        assert!(fish.contains(&format!("complete -c {name}")), "fish {name}");
    }
}

/// A share directory is a path, and a path need not be UTF-8: clap must
/// hand it over as bytes rather than refuse the command line. Where the
/// filesystem itself refuses such a name (APFS does), the failure is the
/// filesystem's, not a usage error.
#[test]
fn a_share_directory_that_is_not_utf8_is_not_a_usage_error() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let base = PathBuf::from(fs_ext4_test_support::temp_path!(
        "cli-docs-{}-nonutf8",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let share = base.join(OsStr::from_bytes(b"share-\xff"));
    for what in ["man", "completions"] {
        let out = entry()
            .args([OsStr::new("generate"), OsStr::new(what), share.as_os_str()])
            .output()
            .expect("spawn");
        assert_ne!(
            out.status.code(),
            Some(2),
            "generate {what}: a non-UTF-8 SHARE was refused as a usage error: {}",
            stderr(&out)
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}
