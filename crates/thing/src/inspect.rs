use std::{ffi::OsString, path::PathBuf, time::Duration};

use thing_wayland::{
    LayoutInspector,
    command::{self, InspectOptions, InspectQuery},
};

const HELP: &str = "Usage: thing inspect --output-size <WIDTHxHEIGHT> [--output-name <NAME>] [--scale <FACTOR>] <entry.luau>\n       thing inspect --live [--instance <name>] [--output <name> | --all-outputs] [--timeout <seconds>] [--keys] [--surface <name>] [--node <id> | --at-pointer] [--highlight <seconds>]\n\nWithout --live, report initial widget layout without opening a window.\nDimensions are logical pixels; scale defaults to 1 and is rounded to 1/120.\n\nWith --live, report the frames a running instance last presented, as json.\nThe instance must run with 'thing run --inspector'. Text, input values,\nlinks, and images are never reported.\n\nLive options:\n      --instance <name>     The instance started with 'thing run --instance' (default: default)\n      --output <name>       The output to inspect\n      --all-outputs         Inspect every output, in order\n      --timeout <seconds>   How long to wait for the reply (default: 10)\n      --keys                Include widget keys, which may contain your data\n      --surface <name>      Only the surfaces of this panel or window (default for the unnamed one)\n      --node <id>           Only this widget and its descendants\n      --at-pointer          Only the widget box under the pointer and its ancestors\n      --highlight <seconds> Outline the selected widget for 0.1 to 60 seconds\n  -h, --help                Show this help\n\nWithout --output or --all-outputs, the instance must run on exactly one output.\n";

const OFFLINE_OPTIONS: [&str; 3] = ["--output-size", "--output-name", "--scale"];
const LIVE_VALUE_OPTIONS: [&str; 6] = [
    "--instance",
    "--output",
    "--timeout",
    "--surface",
    "--node",
    "--highlight",
];
const LIVE_FLAGS: [&str; 3] = ["--all-outputs", "--keys", "--at-pointer"];

type Result<T> = std::result::Result<T, (u8, String)>;

pub(super) fn run(args: impl Iterator<Item = OsString>) -> Result<()> {
    let args: Vec<OsString> = args.collect();
    let mut options = args.iter();
    let mut live = false;
    while let Some(argument) = options.next() {
        if argument == "--" {
            break;
        }
        if argument == "--live" {
            live = true;
            break;
        }
        let argument = argument.to_string_lossy();
        if !argument.contains('=')
            && (OFFLINE_OPTIONS.contains(&argument.as_ref())
                || LIVE_VALUE_OPTIONS.contains(&argument.as_ref()))
        {
            options.next();
        }
    }
    if live {
        run_live(args.into_iter())
    } else {
        run_offline(args.into_iter())
    }
}

fn run_offline(mut args: impl Iterator<Item = OsString>) -> Result<()> {
    let mut size = None;
    let mut name = None;
    let mut scale = 120;
    let mut path = None;
    let mut positional_only = false;
    while let Some(argument) = args.next() {
        if !positional_only && argument == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && (argument == "--help" || argument == "-h") {
            print!("{HELP}");
            return Ok(());
        }
        if !positional_only && argument.to_string_lossy().starts_with('-') {
            let argument = argument
                .to_str()
                .ok_or_else(|| usage("option must be valid UTF-8"))?;
            let (option, inline) = argument
                .split_once('=')
                .map_or((argument, None), |(k, v)| (k, Some(v)));
            if LIVE_VALUE_OPTIONS.contains(&option) || LIVE_FLAGS.contains(&option) {
                return Err(usage(format!("{option} applies only with --live")));
            }
            if !OFFLINE_OPTIONS.contains(&option) {
                return Err(usage(format!("unknown inspect option {option:?}")));
            }
            let value = inline
                .map(OsString::from)
                .or_else(|| args.next())
                .ok_or_else(|| usage(format!("expected a value after {option}")))?;
            let value = value
                .to_str()
                .ok_or_else(|| usage(format!("{option} must be valid UTF-8")))?;
            match option {
                "--output-size" => size = Some(parse_size(value)?),
                "--output-name" => name = Some(value.to_owned()),
                "--scale" => scale = parse_scale(value)?,
                _ => unreachable!(),
            }
            continue;
        }
        if path.replace(PathBuf::from(argument)).is_some() {
            return Err(usage("expected exactly one entry file"));
        }
    }
    let size = size.ok_or_else(|| usage("inspect requires --output-size WIDTHxHEIGHT"))?;
    let path = path.ok_or_else(|| usage("expected an entry file"))?;
    let inspector = LayoutInspector::new(size, name, scale).map_err(usage)?;
    let report = inspector.inspect(&path).map_err(|error| (1, error))?;
    print!("{report}");
    Ok(())
}

fn run_live(mut args: impl Iterator<Item = OsString>) -> Result<()> {
    let mut instance = command::DEFAULT_INSTANCE.to_owned();
    let mut output = None;
    let mut all = false;
    let mut timeout = Duration::from_secs(10);
    let mut query = InspectQuery::default();
    while let Some(argument) = args.next() {
        let text = argument
            .into_string()
            .map_err(|_| usage("arguments must be valid UTF-8"))?;
        if text == "--help" || text == "-h" {
            print!("{HELP}");
            return Ok(());
        }
        if !text.starts_with('-') || text == "--" {
            return Err(usage(
                "live inspection reads a running instance and takes no entry file",
            ));
        }
        let (option, inline) = text
            .split_once('=')
            .map_or((text.as_str(), None), |(option, value)| {
                (option, Some(value.to_owned()))
            });
        if OFFLINE_OPTIONS.contains(&option) {
            return Err(usage(format!("{option} applies only without --live")));
        }
        if option == "--live" || LIVE_FLAGS.contains(&option) {
            if inline.is_some() {
                return Err(usage(format!("{option} does not take a value")));
            }
            match option {
                "--all-outputs" => all = true,
                "--keys" => query.keys = true,
                "--at-pointer" => query.at_pointer = true,
                _ => {}
            }
            continue;
        }
        if !LIVE_VALUE_OPTIONS.contains(&option) {
            return Err(usage(format!("unknown inspect option {option:?}")));
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
            "--timeout" => timeout = crate::call::parse_timeout(&value).map_err(usage)?,
            "--surface" => query.surface = Some(value),
            "--node" => {
                query.node = Some(
                    value
                        .parse()
                        .map_err(|_| usage("--node must be a positive whole number"))?,
                );
            }
            _ => {
                let seconds: f64 = value
                    .parse()
                    .ok()
                    .filter(|seconds: &f64| (0.1..=60.0).contains(seconds))
                    .ok_or_else(|| usage("--highlight must be between 0.1 and 60 seconds"))?;
                query.highlight = Some(Duration::from_secs_f64(seconds));
            }
        }
    }
    let selection = crate::call::output_selection(output, all).map_err(usage)?;
    let options = InspectOptions {
        instance,
        selection,
        query,
        timeout,
    };
    let printed = command::inspect(&options)?;
    println!("{printed}");
    Ok(())
}

fn parse_size(value: &str) -> Result<(u32, u32)> {
    let (width, height) = value
        .split_once('x')
        .ok_or_else(|| usage("expected WIDTHxHEIGHT after --output-size"))?;
    let dimension = |value: &str| {
        value
            .parse::<u32>()
            .map_err(|_| usage("output dimensions must be positive whole numbers"))
    };
    Ok((dimension(width)?, dimension(height)?))
}

fn parse_scale(value: &str) -> Result<u32> {
    let factor = value
        .parse::<f64>()
        .map_err(|_| usage("scale must be a positive number"))?;
    let units = (factor * 120.0).round();
    if !factor.is_finite() || !(1.0..=f64::from(u32::MAX)).contains(&units) {
        return Err(usage("scale must be a positive finite number"));
    }
    Ok(units as u32)
}

fn usage(message: impl Into<String>) -> (u8, String) {
    (2, message.into())
}
