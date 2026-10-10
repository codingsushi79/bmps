mod cli;
mod config;
mod daemon;
mod docker;
mod instance;
mod ipc;
mod model;
mod paths;
mod release;
mod rescue;
mod runtime;
mod tui;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
