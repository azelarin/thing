use std::{env, ffi::OsStr, path::PathBuf, process::ExitCode};
use thing_runtime::Runtime;
use thing_wayland::{Renderer, RunOptions};

const HELP: &str = "thing — a programmable Wayland shell\n\nUsage:\n  thing run [--renderer <opengl|vulkan>] [--notification-server] <entry.luau>\n  thing check <entry.luau>\n\nCommands:\n  run    Open a panel on each Wayland output\n  check  Evaluate a configuration and validate its widget tree\n\nOptions:\n      --renderer             Select the graphics backend for run (default: opengl)\n      --notification-server  Provide the desktop notification service\n  -h, --help                 Show this help\n  -V, --version              Show the version\n";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, message)) => {
            eprintln!("thing: {message}");
            ExitCode::from(code)
        }
    }
}

fn run() -> Result<(), (u8, String)> {
    let mut args = env::args_os().skip(1);
    let Some(command) = args.next() else {
        return Err((2, "expected a command; see 'thing --help'".into()));
    };
    if command == "--help" || command == "-h" {
        print!("{HELP}");
        return Ok(());
    }
    if command == "--version" || command == "-V" {
        println!("thing {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if command != "check" && command != "run" {
        return Err((
            2,
            format!("unknown command {command:?}; see 'thing --help'"),
        ));
    }
    let mut renderer = Renderer::default();
    let mut notification_server = false;
    let mut path = None;
    let mut positional_only = false;
    while let Some(argument) = args.next() {
        if !positional_only && argument == OsStr::new("--") {
            positional_only = true;
            continue;
        }
        if !positional_only && (argument == OsStr::new("--help") || argument == OsStr::new("-h")) {
            print!("{HELP}");
            return Ok(());
        }
        if command == "run" && !positional_only && argument == OsStr::new("--renderer") {
            let value = args
                .next()
                .ok_or_else(|| (2, "expected a value after --renderer".into()))?;
            renderer = parse_renderer(&value)?;
            continue;
        }
        if command == "run" && !positional_only && argument == OsStr::new("--notification-server") {
            notification_server = true;
            continue;
        }
        if command == "run"
            && !positional_only
            && let Some(value) = argument
                .to_str()
                .and_then(|value| value.strip_prefix("--renderer="))
        {
            renderer = value.parse().map_err(|error| (2, error))?;
            continue;
        }
        if !positional_only && argument.to_string_lossy().starts_with('-') {
            return Err((
                2,
                format!("unknown option {argument:?}; see 'thing --help'"),
            ));
        }
        if path.replace(PathBuf::from(argument)).is_some() {
            return Err((2, "expected exactly one entry file".into()));
        }
    }
    let path = path.ok_or_else(|| (2, "expected an entry file; see 'thing --help'".into()))?;
    if command == "run" {
        let options = RunOptions::default()
            .with_renderer(renderer)
            .with_notification_server(notification_server);
        return thing_wayland::run_with_options(&path, options).map_err(|error| (1, error));
    }
    let runtime = Runtime::load(&path).map_err(|error| (1, error.to_string()))?;
    println!(
        "{}: valid ({} file(s))",
        path.display(),
        runtime.files().len()
    );
    Ok(())
}

fn parse_renderer(value: &OsStr) -> Result<Renderer, (u8, String)> {
    value
        .to_str()
        .ok_or_else(|| (2, "renderer name must be valid UTF-8".into()))?
        .parse()
        .map_err(|error| (2, error))
}
