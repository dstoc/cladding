use cladding_bridge::{parse_listener_args, report_startup_failure, run_listener};
use tokio::io::{self, AsyncWriteExt};

#[tokio::main]
async fn main() {
    let mut transport = tokio::io::join(io::stdin(), io::stdout());
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let address = match parse_listener_args(&args) {
        Ok(address) => address,
        Err(message) => {
            let _ = report_startup_failure(&mut transport).await;
            let _ = io::stderr()
                .write_all(format!("{message}\n").as_bytes())
                .await;
            std::process::exit(2);
        }
    };

    if let Err(error) = run_listener(address, transport).await {
        let _ = io::stderr()
            .write_all(format!("cladding-bridge: {error}\n").as_bytes())
            .await;
        std::process::exit(1);
    }
}
