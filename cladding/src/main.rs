mod cli;

fn main() {
    if let Err(err) = cli::run() {
        cli::print_error_and_exit(err);
    }
}
