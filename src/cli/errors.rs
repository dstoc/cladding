use cladding::error::Error;

pub fn print_error_and_exit(err: Error) -> ! {
    eprintln!("{err}");
    cladding::runtime_metrics::print_pending_summary();
    std::process::exit(err.exit_code());
}
