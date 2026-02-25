mod assets;
mod cli;
mod config;
mod error;
mod fs_utils;
mod network;
mod podman;

fn main() {
    if let Err(err) = cli::run() {
        cli::print_error_and_exit(err);
    }
}
