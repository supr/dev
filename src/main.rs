use clap::Parser;
use dev::{App, Cli, SystemRunner};
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let current_dir = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: could not determine the current directory: {error}");
            return ExitCode::FAILURE;
        }
    };

    let mut app = App::new(current_dir, SystemRunner);
    match app.execute(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
