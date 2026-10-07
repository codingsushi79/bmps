mod cli;
mod config;
mod daemon;
mod instance;
mod ipc;
mod model;
mod paths;
mod release;
mod runtime;
mod tui;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
