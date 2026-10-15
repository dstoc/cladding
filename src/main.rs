mod cli;

fn main() {
    if let Err(err) = cli::run() {
        cli::print_error_and_exit(err);
    }
    cladding::runtime_metrics::print_pending_summary();
}
