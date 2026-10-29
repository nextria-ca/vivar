//! Man pages straight from the clap command tree: `vivar man <dir>` writes
//! `vivar.1` and one `vivar-<sub>.1` per subcommand (nested ones as
//! `vivar-python-install.1`), in plain roff with no extra dependency.

use anyhow::{Context, Result};
use clap::Command;
use std::fs;
use std::path::Path;

/// Writes every page into `dir` and returns their names.
pub fn write_all(root: &Command, dir: &Path) -> Result<Vec<String>> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut names = Vec::new();
    let mut stack: Vec<(Vec<String>, &Command)> = vec![(vec!["vivar".to_owned()], root)];
    while let Some((path, cmd)) = stack.pop() {
        let name = path.join("-");
        fs::write(dir.join(format!("{name}.1")), page(&path, cmd))?;
        names.push(format!("{name}.1"));
        for sub in cmd.get_subcommands() {
            if sub.is_hide_set() || sub.get_name() == "help" {
                continue;
            }
            let mut p = path.clone();
            p.push(sub.get_name().to_owned());
            stack.push((p, sub));
        }
    }
    names.sort();
    Ok(names)
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('-', "\\-")
}

fn page(path: &[String], cmd: &Command) -> String {
    let name = path.join("-");
    let title = path.join(" ");
    let about = cmd.get_about().map(|a| a.to_string()).unwrap_or_default();
    let long = cmd
        .get_long_about()
        .map(|a| a.to_string())
        .unwrap_or_else(|| about.clone());
    let mut out = String::new();
    out.push_str(&format!(
        ".TH \"{}\" \"1\" \"\" \"vivar {}\" \"vivar manual\"\n",
        esc(&name).to_uppercase(),
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(&format!(".SH NAME\n{} \\- {}\n", esc(&name), esc(&about)));
    // Synopsis: options, then positionals, then a subcommand slot.
    let mut synopsis = format!("\\fB{}\\fR", esc(&title));
    let generated = |a: &&clap::Arg| {
        matches!(
            a.get_action(),
            clap::ArgAction::Help
                | clap::ArgAction::HelpShort
                | clap::ArgAction::HelpLong
                | clap::ArgAction::Version
        )
    };
    let (opts, positionals): (Vec<_>, Vec<_>) = cmd
        .get_arguments()
        .filter(|a| !generated(a))
        .partition(|a| !a.is_positional());
    if !opts.is_empty() {
        synopsis.push_str(" [\\fIOPTIONS\\fR]");
    }
    for a in &positionals {
        let n = a
            .get_value_names()
            .and_then(|v| v.first().map(|s| s.to_string()))
            .unwrap_or_else(|| a.get_id().to_string().to_uppercase());
        let many = if a.get_num_args().is_some_and(|r| r.max_values() > 1) {
            "..."
        } else {
            ""
        };
        if a.is_required_set() {
            synopsis.push_str(&format!(" <{}>{many}", esc(&n)));
        } else {
            synopsis.push_str(&format!(" [{}]{many}", esc(&n)));
        }
    }
    if cmd.has_subcommands() {
        synopsis.push_str(" <\\fICOMMAND\\fR>");
    }
    out.push_str(&format!(".SH SYNOPSIS\n{synopsis}\n"));
    out.push_str(&format!(".SH DESCRIPTION\n{}\n", esc(&long)));
    if !positionals.is_empty() {
        out.push_str(".SH ARGUMENTS\n");
        for a in &positionals {
            let n = a
                .get_value_names()
                .and_then(|v| v.first().map(|s| s.to_string()))
                .unwrap_or_else(|| a.get_id().to_string().to_uppercase());
            out.push_str(&format!(
                ".TP\n\\fI{}\\fR\n{}\n",
                esc(&n),
                esc(&a.get_help().map(|h| h.to_string()).unwrap_or_default())
            ));
        }
    }
    if !opts.is_empty() {
        out.push_str(".SH OPTIONS\n");
        for a in &opts {
            let mut flags = Vec::new();
            if let Some(s) = a.get_short() {
                flags.push(format!("\\fB\\-{s}\\fR"));
            }
            if let Some(l) = a.get_long() {
                flags.push(format!("\\fB\\-\\-{}\\fR", esc(l)));
            }
            let mut line = flags.join(", ");
            if a.get_action().takes_values() {
                let n = a
                    .get_value_names()
                    .and_then(|v| v.first().map(|s| s.to_string()))
                    .unwrap_or_else(|| a.get_id().to_string().to_uppercase());
                line.push_str(&format!(" <{}>", esc(&n)));
            }
            out.push_str(&format!(
                ".TP\n{line}\n{}\n",
                esc(&a.get_help().map(|h| h.to_string()).unwrap_or_default())
            ));
        }
    }
    if cmd.has_subcommands() {
        out.push_str(".SH COMMANDS\n");
        for sub in cmd.get_subcommands() {
            if sub.is_hide_set() || sub.get_name() == "help" {
                continue;
            }
            out.push_str(&format!(
                ".TP\n\\fB{}\\fR(1)\n{}\n",
                esc(&format!("{name}-{}", sub.get_name())),
                esc(&sub.get_about().map(|a| a.to_string()).unwrap_or_default())
            ));
        }
    }
    let mut see: Vec<String> = Vec::new();
    if path.len() > 1 {
        see.push(format!(
            "\\fB{}\\fR(1)",
            esc(&path[..path.len() - 1].join("-"))
        ));
    }
    see.push("\\fBuv\\fR(1)".to_owned());
    out.push_str(&format!(".SH SEE ALSO\n{}\n", see.join(", ")));
    out.push_str(".SH PROJECT\nhttps://github.com/nextria\\-ca/vivar\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_cover_every_subcommand() {
        let cmd = Command::new("vivar")
            .about("Fast, license-safe Python environment manager")
            .subcommand(
                Command::new("add")
                    .about("Add dependencies")
                    .long_about("Add dependencies, checked against policy first.")
                    .arg(
                        clap::Arg::new("group")
                            .long("group")
                            .help("Dependency group"),
                    )
                    .arg(
                        clap::Arg::new("packages")
                            .required(true)
                            .num_args(1..)
                            .help("Requirement specs"),
                    ),
            )
            .subcommand(
                Command::new("python").about("Manage Pythons").subcommand(
                    Command::new("install").about("Install a build").arg(
                        clap::Arg::new("version")
                            .required(true)
                            .help("Series or full version"),
                    ),
                ),
            );
        let dir = std::env::temp_dir().join(format!("vivar-man-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let names = write_all(&cmd, &dir).unwrap();
        assert_eq!(
            names,
            vec![
                "vivar-add.1",
                "vivar-python-install.1",
                "vivar-python.1",
                "vivar.1"
            ]
        );
        let add = fs::read_to_string(dir.join("vivar-add.1")).unwrap();
        assert!(add.contains(".TH \"VIVAR\\-ADD\" \"1\""), "{add}");
        assert!(
            add.contains("\\fBvivar add\\fR [\\fIOPTIONS\\fR] <PACKAGES>..."),
            "{add}"
        );
        assert!(add.contains("\\fB\\-\\-group\\fR <GROUP>"), "{add}");
        assert!(add.contains("checked against policy first"), "{add}");
        let install = fs::read_to_string(dir.join("vivar-python-install.1")).unwrap();
        assert!(install.contains("<VERSION>"), "{install}");
        let top = fs::read_to_string(dir.join("vivar.1")).unwrap();
        assert!(
            top.contains(".SH COMMANDS") && top.contains("\\fBvivar\\-python\\fR(1)"),
            "{top}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
