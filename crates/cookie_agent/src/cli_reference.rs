//! Keeps the command reference in `docs/site/clients/cli.md` in step with the
//! clap definitions: every subcommand that runs, or takes options of its own,
//! has a heading naming it, and every long option appears under that heading.

use clap::CommandFactory as _;

use super::Cli;

const DOC_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/site/clients/cli.md"
);

/// The lines of the section under the heading `` `<path>` ``, down to the
/// next heading of the same or a higher level; `None` without such a heading.
fn section<'a>(lines: &[&'a str], path: &str) -> Option<Vec<&'a str>> {
    let title = format!("`{path}`");
    let start = lines.iter().position(|line| {
        let level = line.chars().take_while(|c| *c == '#').count();
        level > 0 && line[level..].trim() == title
    })?;
    let level = lines[start].chars().take_while(|c| *c == '#').count();
    let end = lines[start + 1..]
        .iter()
        .position(|line| {
            let next = line.chars().take_while(|c| *c == '#').count();
            next > 0 && next <= level && line[next..].starts_with(' ')
        })
        .map_or(lines.len(), |offset| start + 1 + offset);
    Some(lines[start..end].to_vec())
}

fn long_options(command: &clap::Command) -> Vec<String> {
    command
        .get_arguments()
        .filter_map(clap::Arg::get_long)
        .filter(|long| !matches!(*long, "help" | "version"))
        .map(|long| format!("--{long}"))
        .collect()
}

fn check(command: &clap::Command, path: &str, lines: &[&str], missing: &mut Vec<String>) {
    let options = long_options(command);
    let leaf = command.get_subcommands().next().is_none();
    if leaf || !options.is_empty() {
        match section(lines, path) {
            None => missing.push(format!("no heading `{path}`")),
            Some(section) => {
                for option in options {
                    if !section.iter().any(|line| line.contains(option.as_str())) {
                        missing.push(format!("`{path}` does not mention {option}"));
                    }
                }
            }
        }
    }
    for subcommand in command.get_subcommands() {
        check(
            subcommand,
            &format!("{path} {}", subcommand.get_name()),
            lines,
            missing,
        );
    }
}

#[test]
fn command_reference_names_every_subcommand_and_option() {
    let page = std::fs::read_to_string(DOC_PATH).expect("read the command line page");
    let lines = page.lines().collect::<Vec<_>>();
    let cli = Cli::command();
    let mut missing = Vec::new();
    // The top-level options are global, so they may be described anywhere.
    for option in long_options(&cli) {
        if !page.contains(option.as_str()) {
            missing.push(format!("the page does not mention {option}"));
        }
    }
    if section(&lines, "cookie").is_none() {
        missing.push("no heading `cookie`".to_owned());
    }
    for subcommand in cli.get_subcommands() {
        check(
            subcommand,
            &format!("cookie {}", subcommand.get_name()),
            &lines,
            &mut missing,
        );
    }
    assert!(
        missing.is_empty(),
        "docs/site/clients/cli.md is out of step with the CLI:\n{}",
        missing.join("\n")
    );
}
