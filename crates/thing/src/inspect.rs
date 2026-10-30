use std::{ffi::OsString, path::PathBuf};

use thing_wayland::LayoutInspector;

const HELP: &str = "Usage: thing inspect --output-size <WIDTHxHEIGHT> [--output-name <NAME>] [--scale <FACTOR>] <entry.luau>\n\nReport initial widget layout without opening a window.\nDimensions are logical pixels; scale defaults to 1 and is rounded to 1/120.\n";

type Result<T> = std::result::Result<T, (u8, String)>;

pub(super) fn run(mut args: impl Iterator<Item = OsString>) -> Result<()> {
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
            if !matches!(option, "--output-size" | "--output-name" | "--scale") {
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
