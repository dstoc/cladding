use cladding::error::Error;

pub fn print_error_and_exit(err: Error) -> ! {
    eprintln!("{err}");
    std::process::exit(err.exit_code());
}
