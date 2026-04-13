mod auth;
mod chatgpt_api;
mod cli;
mod commands;
mod display;
mod model;
mod registry;
mod sessions;

use anyhow::Result;

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<()> {
    let command = cli::parse();
    commands::run(command)
}
