use mcp_run::{LOCAL_FAILURE_EXIT_CODE, run_remote_from_env};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exit_code = match run_remote_from_env(args).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            LOCAL_FAILURE_EXIT_CODE
        }
    };

    std::process::exit(exit_code);
}
