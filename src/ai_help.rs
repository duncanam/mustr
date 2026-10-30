//! Version-matched agent guidance and a generated public CLI reference.
//!
//! Rendering reads no configuration or live state and invokes no subprocesses.
//! Environment-bound argument values must never appear in the generated help.

use std::io::{self, Write};

use clap::{ColorChoice, Command, CommandFactory};

use crate::cli::Cli;

const GUIDE: &str = include_str!("../docs/ai-help.md");

pub(crate) fn print() -> io::Result<()> {
    io::stdout().lock().write_all(render().as_bytes())
}

fn render() -> String {
    let mut output = format!(
        "# mustr {} — agent guide\n\n{GUIDE}\n## Public CLI reference\n\nGenerated from this binary's Clap definitions. Hidden adapter/bootstrap interfaces are intentionally excluded.\n",
        env!("CARGO_PKG_VERSION")
    );
    append_command(Cli::command(), "", &mut output);
    output
}

fn append_command(command: Command, parent: &str, output: &mut String) {
    if command.is_hide_set() {
        return;
    }
    let path = if parent.is_empty() {
        command.get_name().to_owned()
    } else {
        format!("{parent} {}", command.get_name())
    };
    let mut command = command
        .bin_name(path.clone())
        .color(ColorChoice::Never)
        .term_width(100)
        .mut_args(|arg| arg.hide_env_values(true));
    let help = command.render_long_help();
    output.push_str(&format!("\n### `{path}`\n\n```text\n{help}```\n"));
    let generated_help = !command.is_disable_help_subcommand_set();
    for child in command.get_subcommands() {
        // Clap's synthetic help command is a proxy, not a separate workflow.
        if generated_help && child.get_name() == "help" {
            continue;
        }
        append_command(child.clone(), &path, output);
    }
}
