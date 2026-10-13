use cladding_bridge::{HostTarget, run_host_session};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
};

#[tokio::test]
async fn helper_opens_server_first_streams_and_reuses_capacity() {
    let target = TcpListener::bind("127.0.0.1:0").await.expect("bind target");
    let target_addr = target.local_addr().expect("target address");
    let target_task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = target.accept().await {
            tokio::spawn(async move {
                socket.write_all(b"greeting").await.expect("write greeting");
                socket.shutdown().await.expect("half close target");
                let mut request = Vec::new();
                let _ = socket.read_to_end(&mut request).await;
            });
        }
    });

    let reservation = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve listener port");
    let listener_addr = reservation.local_addr().expect("listener address");
    drop(reservation);

    let mut helper = Command::new(env!("CARGO_BIN_EXE_cladding-bridge"))
        .args(["listen", "--tcp", &listener_addr.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start bridge helper");
    let stdin = helper.stdin.take().expect("helper stdin");
    let stdout = helper.stdout.take().expect("helper stdout");
    let host = tokio::spawn(run_host_session(
        tokio::io::join(stdout, stdin),
        HostTarget::tcp("127.0.0.1", target_addr.port()),
    ));

    // Open more streams than the configured limit, one at a time. A target
    // that writes first verifies that the helper announces each stream without
    // waiting for client data. Going past the limit verifies that completed
    // streams release capacity for later connections.
    for _ in 0..=(cladding_bridge::MAX_CONCURRENT_STREAMS) {
        let mut client = connect_when_ready(listener_addr).await;
        let mut greeting = [0; 8];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut greeting))
            .await
            .expect("server-first greeting timeout")
            .expect("read target greeting");
        assert_eq!(&greeting, b"greeting");
        drop(client);
    }

    helper.kill().await.expect("stop helper");
    host.abort();
    target_task.abort();
}

async fn connect_when_ready(address: std::net::SocketAddr) -> TcpStream {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match TcpStream::connect(address).await {
                Ok(stream) => return stream,
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("connect to helper: {error}"),
            }
        }
    })
    .await
    .expect("connect to helper timeout")
}
