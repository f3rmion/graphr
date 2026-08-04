use std::env;
use std::process::ExitCode;

const USAGE: &str = "Usage: grapher --version";

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    if args.next().is_some_and(|arg| arg == "--version") && args.next().is_none() {
        println!("grapher {}", env!("CARGO_PKG_VERSION"));
        ExitCode::SUCCESS
    } else {
        eprintln!("{USAGE}");
        ExitCode::from(2)
    }
}
