//! `fabcli guide` and `fabcli playbook`: agent guidance compiled into
//! the binary, so it always matches the commands it describes. Neither
//! needs a login or a token file, and the content needs no network.

use crate::error::FabCliError;
use crate::output::print_json;
use clap::builder::PossibleValuesParser;
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// `(name, contents of <dir>/<name><ext>)` for each name, so the key
/// and the file name can't differ.
macro_rules! embed {
    ($dir:literal, $ext:literal: $($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", $dir, "/", $name, $ext)))),*]
    };
}

/// The primer `guide` prints without a topic (at most 2,048 characters).
const PRIMER: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/guide/index.md"));

/// Guide topics, compiled in from `guide/<topic>.md`.
pub const TOPICS: &[(&str, &str)] =
    embed!("guide", ".md": "auth", "search", "library", "claim", "download");

/// Playbooks, compiled in from `playbooks/<name>.json` (format: `Playbook`).
const PLAYBOOKS: &[(&str, &str)] = embed!(
    "playbooks", ".json":
    "first-run",
    "search-and-inspect",
    "check-ownership",
    "download-owned",
    "library-since",
    "find-by-seller",
    "free-new-assets",
    "claim-free-batch",
    "claim-promos",
    "verify-library",
);

#[derive(Subcommand, Debug)]
pub enum PlaybookCommand {
    /// List the playbooks: name, title, summary, tags, requires_auth
    List {
        /// Only playbooks with this tag (e.g. `claim`)
        #[arg(long)]
        tag: Option<String>,
    },
    /// Print one playbook as JSON, with each step's shell `line`
    Show {
        /// Playbook name, from `fabcli playbook list`
        #[arg(value_parser = PossibleValuesParser::new(names(PLAYBOOKS)))]
        name: String,
    },
}

/// The keys of an `embed!` table: clap's possible values for a topic or
/// playbook name, so clap rejects an unknown one (exit 6) and lists the
/// valid ones in the error and in `--help`.
pub fn names(table: &'static [(&'static str, &'static str)]) -> impl Iterator<Item = &'static str> {
    table.iter().map(|(name, _)| *name)
}

/// The entry clap already checked `name` against.
fn lookup(table: &'static [(&'static str, &'static str)], name: &str) -> &'static str {
    table
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, text)| *text)
        .expect("clap checks the name against the table")
}

/// A recipe: commands to run in order. `<Name>` placeholders in a
/// command come from `inputs` or from an earlier step's `save`
/// (placeholder -> dot path into that step's stdout JSON).
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Playbook {
    name: String,
    title: String,
    summary: String,
    tags: Vec<String>,
    requires_auth: bool,
    #[serde(default)]
    inputs: Vec<Input>,
    steps: Vec<Step>,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Input {
    name: String,
    description: String,
    example: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Step {
    /// Arguments after `fabcli`.
    command: Vec<String>,
    why: String,
    /// What to look at in the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    check: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    save: BTreeMap<String, String>,
    /// Index of an earlier step whose stdout is piped into this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stdin: Option<usize>,
    /// Error `kind` -> what to do about it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    on_error: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    optional: bool,
    /// A step the user does themselves (`auth login`'s sign-in window).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    interactive: bool,
    /// `command` as one shell line; filled in by `playbook show`.
    #[serde(skip_deserializing, skip_serializing_if = "String::is_empty")]
    line: String,
}

fn parse(json: &str) -> Playbook {
    serde_json::from_str(json).expect("embedded playbooks are validated by the unit tests")
}

fn playbooks() -> impl Iterator<Item = Playbook> {
    PLAYBOOKS.iter().map(|(_, json)| parse(json))
}

fn guide_value(topic: Option<&str>) -> Value {
    let (topic, text) = match topic {
        None => ("index", PRIMER),
        Some(topic) => (topic, lookup(TOPICS, topic)),
    };
    let playbooks: Vec<Value> = playbooks()
        .map(|p| json!({"name": p.name, "title": p.title, "summary": p.summary}))
        .collect();
    json!({
        "topic": topic,
        "text": text,
        "topics": names(TOPICS).collect::<Vec<_>>(),
        "playbooks": playbooks,
    })
}

fn playbook_value(command: PlaybookCommand) -> Value {
    match command {
        PlaybookCommand::List { tag } => {
            let list: Vec<Value> = playbooks()
                .filter(|p| tag.as_ref().is_none_or(|t| p.tags.contains(t)))
                .map(|p| {
                    json!({
                        "name": p.name,
                        "title": p.title,
                        "summary": p.summary,
                        "tags": p.tags,
                        "requires_auth": p.requires_auth,
                    })
                })
                .collect();
            json!({ "playbooks": list })
        }
        PlaybookCommand::Show { name } => {
            let mut playbook = parse(lookup(PLAYBOOKS, &name));
            // In order, so a `stdin` step can start from its source's finished line.
            for i in 0..playbook.steps.len() {
                let own = shell_line(&playbook.steps[i].command);
                playbook.steps[i].line = match playbook.steps[i].stdin {
                    Some(src) => format!("{} | {own}", playbook.steps[src].line),
                    None => own,
                };
            }
            serde_json::to_value(&playbook).expect("a playbook always serializes")
        }
    }
}

/// `fabcli <args>` for a POSIX shell: an argument with anything unsafe
/// is single-quoted. That includes `<Placeholder>`s, so a line pasted
/// before its placeholders are replaced is never a shell redirection,
/// and a replacement with spaces stays one argument.
fn shell_line(args: &[String]) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c);
    let quoted = args.iter().map(|a| {
        if !a.is_empty() && a.chars().all(safe) {
            a.clone()
        } else {
            format!("'{}'", a.replace('\'', r"'\''"))
        }
    });
    std::iter::once("fabcli".to_string())
        .chain(quoted)
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn guide(topic: Option<String>, pretty: bool) -> Result<(), FabCliError> {
    print_json(&guide_value(topic.as_deref()), pretty);
    Ok(())
}

pub fn playbook(command: PlaybookCommand, pretty: bool) -> Result<(), FabCliError> {
    print_json(&playbook_value(command), pretty);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;
    use std::collections::BTreeSet;

    /// The names of the `<Name>` placeholders in `arg`.
    fn placeholders(arg: &str) -> Vec<&str> {
        let mut found = Vec::new();
        let mut rest = arg;
        while let Some(start) = rest.find('<') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('>') else { break };
            let name = &after[..end];
            if name.starts_with(|c: char| c.is_ascii_alphabetic())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                found.push(name);
            }
            rest = &after[end + 1..];
        }
        found
    }

    /// `arg` with each placeholder replaced by `values[name]`.
    fn substitute(arg: &str, values: &BTreeMap<String, String>) -> String {
        let mut out = arg.to_string();
        for name in placeholders(arg) {
            out = out.replace(&format!("<{name}>"), &values[name]);
        }
        out
    }

    const STDIN_FLAGS: [&str; 3] = ["--stdin", "--from-stdin", "--from-stdin-json"];

    #[test]
    fn primer_is_short() {
        assert!(
            PRIMER.chars().count() <= 2048,
            "guide/index.md is {} characters",
            PRIMER.chars().count()
        );
    }

    #[test]
    fn playbooks_are_valid_commands() {
        for (file, text) in PLAYBOOKS {
            let p: Playbook = serde_json::from_str(text).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(p.name, *file, "{file}: name doesn't match the file");
            assert!(!p.steps.is_empty(), "{file}: no steps");
            let mut values: BTreeMap<String, String> = p
                .inputs
                .iter()
                .map(|i| (i.name.clone(), i.example.clone()))
                .collect();
            for (i, step) in p.steps.iter().enumerate() {
                let at = format!("{file} step {i}");
                assert!(!step.command.is_empty(), "{at}: empty command");
                for arg in &step.command {
                    for name in placeholders(arg) {
                        assert!(
                            values.contains_key(name),
                            "{at}: <{name}> is neither an input nor saved by an earlier step"
                        );
                    }
                }
                if let Some(src) = step.stdin {
                    assert!(src < i, "{at}: stdin {src} is not an earlier step");
                    assert!(
                        step.command
                            .iter()
                            .any(|a| STDIN_FLAGS.contains(&a.as_str())),
                        "{at}: stdin is piped in, but the command reads none of {STDIN_FLAGS:?}"
                    );
                }
                for kind in step.on_error.keys() {
                    assert!(
                        crate::error::KINDS.contains(&kind.as_str()),
                        "{at}: on_error names unknown kind '{kind}'"
                    );
                }
                let argv = std::iter::once("fabcli".to_string())
                    .chain(step.command.iter().map(|a| substitute(a, &values)));
                if let Err(e) = Cli::try_parse_from(argv) {
                    panic!("{at}: {:?} doesn't parse: {e}", step.command);
                }
                for name in step.save.keys() {
                    values.insert(name.clone(), "x".into());
                }
            }
        }
    }

    /// Every file in `guide/` and `playbooks/` is embedded, so a new
    /// one can't be forgotten in the tables above.
    #[test]
    fn every_content_file_is_embedded() {
        let files = |dir: &str, ext: &str| -> BTreeSet<String> {
            std::fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir))
                .unwrap()
                .filter_map(|e| {
                    let name = e.unwrap().file_name().into_string().unwrap();
                    name.strip_suffix(ext).map(str::to_string)
                })
                .collect()
        };
        let mut topics: BTreeSet<String> = TOPICS.iter().map(|(n, _)| n.to_string()).collect();
        topics.insert("index".into());
        assert_eq!(files("guide", ".md"), topics);
        let playbooks: BTreeSet<String> = PLAYBOOKS.iter().map(|(n, _)| n.to_string()).collect();
        assert_eq!(files("playbooks", ".json"), playbooks);
    }

    #[test]
    fn shell_line_quotes_unsafe_arguments() {
        let args: Vec<String> = [
            "search",
            "-q",
            "sci-fi props",
            "--filter",
            "is_free=1",
            "<Uid>",
        ]
        .map(String::from)
        .into();
        assert_eq!(
            shell_line(&args),
            "fabcli search -q 'sci-fi props' --filter is_free=1 '<Uid>'"
        );
    }
}
