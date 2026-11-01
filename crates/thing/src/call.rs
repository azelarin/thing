use std::{ffi::OsString, time::Duration};

use thing_runtime::CallValue;
use thing_wayland::command::{self, CallOptions, OutputSelection};

const HELP: &str = "Usage: thing call [--instance <name>] [--output <name> | --all-outputs] [--timeout <seconds>] <command> [<json-arguments>]\n\nRun a command that a running configuration declared with ui.command, and\nprint its result as json. arguments are one json value, passed to the\nhandler as luau data.\n\nOptions:\n      --instance <name>    The instance started with 'thing run --instance' (default: default)\n      --output <name>      The output whose configuration runs the command\n      --all-outputs        Run the command on every output, in order\n      --timeout <seconds>  How long to wait for the reply (default: 10)\n  -h, --help               Show this help\n\nWithout --output or --all-outputs, the instance must run on exactly one output.\n";

type Result<T> = std::result::Result<T, (u8, String)>;

fn usage(message: impl Into<String>) -> (u8, String) {
    (2, format!("{}; see 'thing call --help'", message.into()))
}

pub(super) fn run(mut args: impl Iterator<Item = OsString>) -> Result<()> {
    let mut instance = command::DEFAULT_INSTANCE.to_owned();
    let mut output = None;
    let mut all = false;
    let mut timeout = Duration::from_secs(10);
    let mut positional = Vec::new();
    let mut positional_only = false;
    while let Some(argument) = args.next() {
        let text = argument
            .into_string()
            .map_err(|_| usage("arguments must be valid UTF-8"))?;
        if positional_only || !text.starts_with("--") && text != "-h" {
            positional.push(text);
            continue;
        }
        if text == "--" {
            positional_only = true;
            continue;
        }
        if text == "--help" || text == "-h" {
            print!("{HELP}");
            return Ok(());
        }
        if text == "--all-outputs" {
            all = true;
            continue;
        }
        let (option, inline) = text
            .split_once('=')
            .map_or((text.as_str(), None), |(option, value)| {
                (option, Some(value.to_owned()))
            });
        if !matches!(option, "--instance" | "--output" | "--timeout") {
            return Err(usage(format!("unknown call option {option:?}")));
        }
        let value = match inline {
            Some(value) => value,
            None => args
                .next()
                .ok_or_else(|| usage(format!("expected a value after {option}")))?
                .into_string()
                .map_err(|_| usage(format!("{option} must be valid UTF-8")))?,
        };
        match option {
            "--instance" => instance = value,
            "--output" => output = Some(value),
            _ => {
                let seconds: f64 = value
                    .parse()
                    .ok()
                    .filter(|seconds: &f64| {
                        seconds.is_finite() && *seconds > 0.0 && *seconds <= 600.0
                    })
                    .ok_or_else(|| usage("--timeout must be between 0 and 600 seconds"))?;
                timeout = Duration::from_secs_f64(seconds);
            }
        }
    }
    let selection = match (output, all) {
        (Some(_), true) => return Err(usage("choose either --output or --all-outputs")),
        (Some(name), false) => OutputSelection::Named(name),
        (None, true) => OutputSelection::All,
        (None, false) => OutputSelection::Implicit,
    };
    let mut positional = positional.into_iter();
    let name = positional
        .next()
        .ok_or_else(|| usage("expected a command name"))?;
    let arguments = match positional.next() {
        Some(text) => command::parse_arguments(&text).map_err(usage)?,
        None => CallValue::Nil,
    };
    if positional.next().is_some() {
        return Err(usage(
            "expected one json argument; wrap several values in a list or object",
        ));
    }
    let printed = command::call(&CallOptions {
        instance,
        selection,
        command: name,
        arguments,
        timeout,
    })?;
    println!("{printed}");
    Ok(())
}
