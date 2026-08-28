use std::{env, path::PathBuf, process::ExitCode};
use thing_runtime::Runtime;

const HELP: &str = "thing — a programmable Wayland shell\n\nUsage: thing <command> <entry.luau>\n\nCommands:\n  run    Open a panel on each Wayland output\n  check  Evaluate a configuration and validate its widget tree\n\nOptions:\n  -h, --help     Show this help\n  -V, --version  Show the version\n";

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
    let first = args.next();
    let argument = if first.as_deref() == Some(std::ffi::OsStr::new("--")) {
        args.next()
    } else {
        first
    };
    let path = argument
        .map(PathBuf::from)
        .ok_or_else(|| (2, "expected an entry file; see 'thing --help'".into()))?;
    if args.next().is_some() {
        return Err((2, "expected exactly one entry file".into()));
    }
    if command == "run" {
        return thing_wayland::run(&path).map_err(|error| (1, error));
    }
    let runtime = Runtime::load(&path).map_err(|error| (1, error.to_string()))?;
    println!(
        "{}: valid ({} file(s))",
        path.display(),
        runtime.files().len()
    );
    Ok(())
}
