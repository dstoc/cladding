//! Fixed-endpoint multiplexed byte-stream transport for Cladding.
//!
//! A bridge session has one host-selected [`HostTarget`]. The wire protocol
//! carries only a startup handshake and Yamux streams; it has no destination
//! request or routing message.

use futures_util::future::poll_fn;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use yamux::{Config, Connection, Mode, Stream};

/// Maximum active logical streams in one bridge session.
pub const MAX_CONCURRENT_STREAMS: usize = 64;

const PROTOCOL_VERSION: u16 = 1;
const HANDSHAKE_LEN: usize = 8;
const READY: u8 = 0;
const LISTENER_BIND_FAILURE: u8 = 1;
const STARTUP_FAILURE: u8 = 2;
const ACK: u8 = 3;
const MAX_CONNECTION_RECEIVE_WINDOW: usize =
    MAX_CONCURRENT_STREAMS * yamux::DEFAULT_CREDIT as usize;

/// The one destination selected by trusted host-side code for a bridge session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostTarget {
    /// Connect every logical stream to this host TCP endpoint.
    Tcp { host: String, port: u16 },
    /// Connect every logical stream to this host Unix-domain socket.
    #[cfg(unix)]
    Unix(PathBuf),
}

impl HostTarget {
    pub fn tcp(host: impl Into<String>, port: u16) -> Self {
        Self::Tcp {
            host: host.into(),
            port,
        }
    }

    #[cfg(unix)]
    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Self::Unix(path.into())
    }

    async fn connect(&self) -> io::Result<TargetStream> {
        match self {
            Self::Tcp { host, port } => TcpStream::connect((host.as_str(), *port))
                .await
                .map(TargetStream::Tcp),
            #[cfg(unix)]
            Self::Unix(path) => tokio::net::UnixStream::connect(path)
                .await
                .map(TargetStream::Unix),
        }
    }
}

enum TargetStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl AsyncRead for TargetStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            #[cfg(unix)]
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TargetStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        match &mut *self {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            #[cfg(unix)]
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            #[cfg(unix)]
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            #[cfg(unix)]
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// Errors that the host can identify before or during a bridge session.
#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("bridge listener failed to bind")]
    ListenerBindFailure,
    #[error("bridge startup failed before it became ready")]
    StartupFailure,
    #[error("bridge protocol is incompatible")]
    ProtocolIncompatible,
    #[error("bridge transport failed: {0}")]
    Transport(#[source] io::Error),
    #[error("bridge mux failed: {0}")]
    Mux(#[source] yamux::ConnectionError),
}

impl From<io::Error> for BridgeError {
    fn from(error: io::Error) -> Self {
        Self::Transport(error)
    }
}

impl From<yamux::ConnectionError> for BridgeError {
    fn from(error: yamux::ConnectionError) -> Self {
        Self::Mux(error)
    }
}

/// Bind a loopback listener and serve its accepted connections over one mux transport.
///
/// The helper sends a readiness response only after the listener binds. The host
/// destination is deliberately absent from this API and from the remote protocol.
pub async fn run_listener<T>(
    address: std::net::SocketAddr,
    mut transport: T,
) -> Result<(), BridgeError>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    if !address.ip().is_loopback() {
        report_startup_status(&mut transport, STARTUP_FAILURE).await?;
        return Err(BridgeError::StartupFailure);
    }

    let listener = match TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            report_startup_status(&mut transport, LISTENER_BIND_FAILURE).await?;
            return Err(BridgeError::Transport(error));
        }
    };

    send_handshake(&mut transport, *b"CLBR", READY).await?;
    read_ack(&mut transport).await?;
    run_client_side(listener, transport).await
}

/// Report a helper-side argument or setup failure to the host before exiting.
pub async fn report_startup_failure<T>(transport: &mut T) -> io::Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    report_startup_status(transport, STARTUP_FAILURE).await
}

/// Run the host half of a bridge session against one fixed target.
///
/// Each remote-opened Yamux stream connects to `target`; no stream can select
/// or alter that destination.
pub async fn run_host_session<T>(mut transport: T, target: HostTarget) -> Result<(), BridgeError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    read_handshake(&mut transport).await?;
    send_handshake(&mut transport, *b"CLBA", ACK).await?;

    let mut connection = Connection::new(transport.compat(), yamux_config(), Mode::Server);
    let mut relays = JoinSet::new();

    let result = loop {
        tokio::select! {
            incoming = poll_fn(|cx| connection.poll_next_inbound(cx)) => {
                match incoming {
                    Some(Ok(stream)) => {
                        let target = target.clone();
                        relays.spawn(async move { relay_to_target(stream, target).await });
                    }
                    Some(Err(error)) => break Err(BridgeError::Mux(error)),
                    None => break Ok(()),
                }
            }
            Some(_finished_stream) = relays.join_next(), if !relays.is_empty() => {}
        }
    };

    relays.abort_all();
    while relays.join_next().await.is_some() {}
    result
}

async fn run_client_side<T>(listener: TcpListener, transport: T) -> Result<(), BridgeError>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let connection = Connection::new(transport.compat(), yamux_config(), Mode::Client);
    let (open_tx, open_rx) = tokio::sync::mpsc::channel(MAX_CONCURRENT_STREAMS);
    let mut mux_task = tokio::spawn(run_client_mux(connection, open_rx));
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_STREAMS));
    let mut relays = JoinSet::new();
    let mut mux_finished = false;

    let result = loop {
        tokio::select! {
            result = &mut mux_task => {
                mux_finished = true;
                break match result {
                    Ok(result) => result,
                    Err(_) => Err(BridgeError::StartupFailure),
                };
            }
            accepted = async {
                let permit = permits.clone().acquire_owned().await
                    .map_err(|_| BridgeError::StartupFailure)?;
                let socket = listener.accept().await.map_err(BridgeError::from)?;
                Ok::<_, BridgeError>((permit, socket))
            } => {
                let (permit, (socket, _)) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error),
                };
                let (stream_tx, stream_rx) = tokio::sync::oneshot::channel();
                if open_tx.send(stream_tx).await.is_err() {
                    break Err(BridgeError::StartupFailure);
                }
                relays.spawn(async move {
                    let _permit = permit;
                    let Ok(stream) = stream_rx.await else {
                        return;
                    };
                    let mut stream = stream.compat();
                    let mut socket = socket;
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                });
            }
            Some(_finished_stream) = relays.join_next(), if !relays.is_empty() => {}
        }
    };

    if !mux_finished {
        mux_task.abort();
        let _ = mux_task.await;
    }
    relays.abort_all();
    while relays.join_next().await.is_some() {}
    result
}

async fn run_client_mux<T>(
    mut connection: Connection<T>,
    mut requests: tokio::sync::mpsc::Receiver<tokio::sync::oneshot::Sender<Stream>>,
) -> Result<(), BridgeError>
where
    T: futures_util::io::AsyncRead + futures_util::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut pending = VecDeque::new();
    let mut opening: Option<(Stream, tokio::sync::oneshot::Sender<Stream>)> = None;

    loop {
        if pending.is_empty() && opening.is_none() {
            tokio::select! {
                request = requests.recv() => match request {
                    Some(request) => pending.push_back(request),
                    None => return Ok(()),
                },
                incoming = poll_fn(|cx| connection.poll_next_inbound(cx)) => {
                    match incoming {
                        Some(Ok(_unexpected_inbound_stream)) => {}
                        Some(Err(error)) => return Err(BridgeError::Mux(error)),
                        None => return Ok(()),
                    }
                }
            }
        } else {
            let event = poll_fn(|cx| {
                if opening.is_none() && !pending.is_empty() {
                    match connection.poll_new_outbound(cx) {
                        std::task::Poll::Ready(Ok(stream)) => {
                            let request = pending.pop_front().expect("pending stream request");
                            opening = Some((stream, request));
                        }
                        std::task::Poll::Ready(Err(error)) => {
                            return std::task::Poll::Ready(Err(BridgeError::Mux(error)));
                        }
                        std::task::Poll::Pending => {}
                    }
                }

                if let Some((stream, _)) = opening.as_mut() {
                    // Yamux opens outbound streams lazily, on their first write.
                    // An empty write sends only the SYN frame, so the fixed host
                    // target can connect before the local client sends payload.
                    match futures_util::io::AsyncWrite::poll_write(
                        std::pin::Pin::new(stream),
                        cx,
                        &[],
                    ) {
                        std::task::Poll::Ready(Ok(_)) => {
                            let (stream, request) = opening.take().expect("opening stream");
                            let _ = request.send(stream);
                            return std::task::Poll::Ready(Ok(()));
                        }
                        std::task::Poll::Ready(Err(error)) => {
                            return std::task::Poll::Ready(Err(BridgeError::Transport(error)));
                        }
                        std::task::Poll::Pending => {}
                    }
                }

                match connection.poll_next_inbound(cx) {
                    std::task::Poll::Ready(Some(Ok(_unexpected_inbound_stream))) => {
                        std::task::Poll::Ready(Ok(()))
                    }
                    std::task::Poll::Ready(Some(Err(error))) => {
                        std::task::Poll::Ready(Err(BridgeError::Mux(error)))
                    }
                    std::task::Poll::Ready(None) => {
                        std::task::Poll::Ready(Err(BridgeError::StartupFailure))
                    }
                    std::task::Poll::Pending => std::task::Poll::Pending,
                }
            })
            .await;
            event?;
        }
    }
}

async fn relay_to_target(stream: Stream, target: HostTarget) {
    let Ok(mut endpoint) = target.connect().await else {
        return;
    };
    let mut stream = stream.compat();
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut endpoint).await;
}

fn yamux_config() -> Config {
    let mut config = Config::default();
    config
        .set_max_num_streams(MAX_CONCURRENT_STREAMS)
        .set_max_connection_receive_window(Some(MAX_CONNECTION_RECEIVE_WINDOW));
    config
}

async fn send_handshake<T>(transport: &mut T, magic: [u8; 4], status: u8) -> io::Result<()>
where
    T: AsyncWrite + Unpin,
{
    let mut handshake = [0; HANDSHAKE_LEN];
    handshake[..4].copy_from_slice(&magic);
    handshake[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    handshake[6] = status;
    transport.write_all(&handshake).await?;
    transport.flush().await
}

async fn report_startup_status<T>(transport: &mut T, status: u8) -> io::Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    send_handshake(transport, *b"CLBR", status).await
}

async fn read_handshake<T>(transport: &mut T) -> Result<(), BridgeError>
where
    T: AsyncRead + Unpin,
{
    let mut handshake = [0; HANDSHAKE_LEN];
    transport
        .read_exact(&mut handshake)
        .await
        .map_err(|_| BridgeError::StartupFailure)?;
    if &handshake[..4] != b"CLBR"
        || u16::from_be_bytes([handshake[4], handshake[5]]) != PROTOCOL_VERSION
    {
        return Err(BridgeError::ProtocolIncompatible);
    }
    match handshake[6] {
        READY => Ok(()),
        LISTENER_BIND_FAILURE => Err(BridgeError::ListenerBindFailure),
        STARTUP_FAILURE => Err(BridgeError::StartupFailure),
        _ => Err(BridgeError::ProtocolIncompatible),
    }
}

async fn read_ack<T>(transport: &mut T) -> Result<(), BridgeError>
where
    T: AsyncRead + Unpin,
{
    let mut ack = [0; HANDSHAKE_LEN];
    transport
        .read_exact(&mut ack)
        .await
        .map_err(|_| BridgeError::ProtocolIncompatible)?;
    if &ack[..4] != b"CLBA"
        || u16::from_be_bytes([ack[4], ack[5]]) != PROTOCOL_VERSION
        || ack[6] != ACK
    {
        return Err(BridgeError::ProtocolIncompatible);
    }
    Ok(())
}

/// Parse the bridge helper's deliberately narrow listener-only command line.
///
/// No target or routing option exists on the remote helper.
pub fn parse_listener_args(args: &[String]) -> Result<std::net::SocketAddr, String> {
    if args.len() != 3 || args[0] != "listen" || args[1] != "--tcp" {
        return Err("usage: cladding-bridge listen --tcp 127.0.0.1:<port>".to_string());
    }
    let address = args[2]
        .parse::<std::net::SocketAddr>()
        .map_err(|_| "listener must be a loopback IP address and port".to_string())?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err("listener must be a loopback IP address and nonzero port".to_string());
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn start_tcp_echo() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
        let address = listener.local_addr().expect("echo address");
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = [0; 8192];
                    loop {
                        match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(size) => {
                                if stream.write_all(&buffer[..size]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        (address, task)
    }

    async fn start_ready_session(
        target: HostTarget,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<Result<(), BridgeError>>,
        tokio::task::JoinHandle<Result<(), BridgeError>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let (bridge_transport, host_transport) = tokio::io::duplex(128 * 1024);
        let bridge = tokio::spawn(async move {
            let mut transport = bridge_transport;
            send_handshake(&mut transport, *b"CLBR", READY).await?;
            read_ack(&mut transport).await?;
            run_client_side(listener, transport).await
        });
        let host = tokio::spawn(async move { run_host_session(host_transport, target).await });
        (address, bridge, host)
    }

    #[tokio::test]
    async fn relays_concurrent_binary_streams_and_preserves_half_close() {
        let (target, target_task) = start_tcp_echo().await;
        let (listener, bridge, host) =
            start_ready_session(HostTarget::tcp(target.ip().to_string(), target.port())).await;

        let mut clients = Vec::new();
        for index in 0..8u8 {
            let mut client = TcpStream::connect(listener)
                .await
                .expect("connect listener");
            let payload = [vec![index, 0, 255], vec![index; 32 * 1024]].concat();
            client.write_all(&payload).await.expect("write payload");
            client.shutdown().await.expect("half close");
            clients.push((client, payload));
        }

        for (mut client, expected) in clients {
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
                .await
                .expect("read response timeout")
                .expect("read response");
            assert_eq!(response, expected);
        }

        host.abort();
        bridge.abort();
        target_task.abort();
    }

    #[tokio::test]
    async fn reports_listener_bind_failure_and_protocol_mismatch() {
        let occupied = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind occupied");
        let address = occupied.local_addr().expect("occupied address");
        let (bridge_transport, host_transport) = tokio::io::duplex(1024);
        let bridge = tokio::spawn(async move { run_listener(address, bridge_transport).await });
        let host = run_host_session(host_transport, HostTarget::tcp("127.0.0.1", 1)).await;
        assert!(matches!(host, Err(BridgeError::ListenerBindFailure)));
        assert!(bridge.await.expect("bridge task").is_err());

        let (mut remote, host_transport) = tokio::io::duplex(1024);
        let host = tokio::spawn(async move {
            run_host_session(host_transport, HostTarget::tcp("127.0.0.1", 1)).await
        });
        send_handshake(&mut remote, *b"CLBR", STARTUP_FAILURE)
            .await
            .expect("write startup failure");
        assert!(matches!(
            host.await.expect("host task"),
            Err(BridgeError::StartupFailure)
        ));

        let (mut remote, host_transport) = tokio::io::duplex(1024);
        let host = tokio::spawn(async move {
            run_host_session(host_transport, HostTarget::tcp("127.0.0.1", 1)).await
        });
        let mut incompatible = [0; HANDSHAKE_LEN];
        incompatible[..4].copy_from_slice(b"CLBR");
        incompatible[4..6].copy_from_slice(&(PROTOCOL_VERSION + 1).to_be_bytes());
        incompatible[6] = READY;
        remote
            .write_all(&incompatible)
            .await
            .expect("write incompatible hello");
        assert!(matches!(
            host.await.expect("host task"),
            Err(BridgeError::ProtocolIncompatible)
        ));
    }

    #[tokio::test]
    async fn listener_binds_before_sending_ready() {
        let reservation = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve port");
        let address = reservation.local_addr().expect("listener address");
        drop(reservation);
        let (bridge_transport, mut host_transport) = tokio::io::duplex(1024);
        let bridge = tokio::spawn(async move { run_listener(address, bridge_transport).await });

        read_handshake(&mut host_transport)
            .await
            .expect("ready handshake");
        send_handshake(&mut host_transport, *b"CLBA", ACK)
            .await
            .expect("send ack");
        let _client = TcpStream::connect(address)
            .await
            .expect("listener is ready");
        drop(host_transport);
        let _bridge_result = tokio::time::timeout(Duration::from_secs(2), bridge)
            .await
            .expect("bridge shutdown timeout")
            .expect("bridge task");
    }

    #[tokio::test]
    async fn listener_rejects_non_loopback_and_destination_arguments() {
        assert!(
            parse_listener_args(&[
                "listen".to_string(),
                "--tcp".to_string(),
                "0.0.0.0:15432".to_string(),
            ])
            .is_err()
        );
        assert!(
            parse_listener_args(&[
                "listen".to_string(),
                "--tcp".to_string(),
                "127.0.0.1:15432".to_string(),
                "--target".to_string(),
                "127.0.0.1:5432".to_string(),
            ])
            .is_err()
        );
        assert!(
            parse_listener_args(&[
                "listen".to_string(),
                "--tcp".to_string(),
                "127.0.0.1:15432".to_string(),
            ])
            .is_ok()
        );
    }

    #[tokio::test]
    async fn fixed_tcp_target_is_selected_by_host() {
        let (target, target_task) = start_tcp_echo().await;
        let (listener, bridge, host) =
            start_ready_session(HostTarget::tcp(target.ip().to_string(), target.port())).await;
        let mut client = TcpStream::connect(listener)
            .await
            .expect("connect listener");
        client.write_all(b"tcp fixed target").await.expect("write");
        client.shutdown().await.expect("half close");
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("read response");
        assert_eq!(response, b"tcp fixed target");
        host.abort();
        bridge.abort();
        target_task.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fixed_unix_target_is_selected_by_host() {
        use tokio::net::UnixListener;

        let path =
            std::env::temp_dir().join(format!("cladding-bridge-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind unix echo");
        let target_task = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0; 1024];
                loop {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(size) => {
                            if socket.write_all(&buffer[..size]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        let (listener, bridge, host) = start_ready_session(HostTarget::unix(path.clone())).await;
        let mut client = TcpStream::connect(listener)
            .await
            .expect("connect listener");
        client.write_all(b"unix fixed target").await.expect("write");
        client.shutdown().await.expect("half close");
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
            .await
            .expect("read response timeout")
            .expect("read response");
        assert_eq!(response, b"unix fixed target");
        host.abort();
        bridge.abort();
        target_task.abort();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn transport_shutdown_closes_the_listener_session() {
        let (target, target_task) = start_tcp_echo().await;
        let (listener, bridge, host) =
            start_ready_session(HostTarget::tcp(target.ip().to_string(), target.port())).await;
        host.abort();
        let result = tokio::time::timeout(Duration::from_secs(2), bridge)
            .await
            .expect("bridge shutdown timeout");
        assert!(result.expect("bridge task").is_err());
        assert!(TcpStream::connect(listener).await.is_err());
        target_task.abort();
    }

    #[tokio::test]
    async fn active_streams_are_bounded() {
        let target_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind target listener");
        let target = target_listener.local_addr().expect("target address");
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accepted_count = Arc::clone(&accepted);
        let target_task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = target_listener.accept().await {
                accepted_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut byte = [0; 1];
                    loop {
                        match socket.read(&mut byte).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                });
            }
        });
        let (listener, bridge, host) =
            start_ready_session(HostTarget::tcp(target.ip().to_string(), target.port())).await;
        let mut clients = Vec::new();
        for _ in 0..MAX_CONCURRENT_STREAMS {
            let mut client = TcpStream::connect(listener)
                .await
                .expect("connect listener");
            client.write_all(b"x").await.expect("write active marker");
            clients.push(client);
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while accepted.load(std::sync::atomic::Ordering::SeqCst) < MAX_CONCURRENT_STREAMS {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "only {} target streams connected; host_done={}, bridge_done={}",
                accepted.load(std::sync::atomic::Ordering::SeqCst),
                host.is_finished(),
                bridge.is_finished(),
            )
        });

        let extra = TcpStream::connect(listener)
            .await
            .expect("connect queued listener");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            MAX_CONCURRENT_STREAMS
        );

        drop(clients);
        tokio::time::timeout(Duration::from_secs(10), async {
            while accepted.load(std::sync::atomic::Ordering::SeqCst)
                < MAX_CONCURRENT_STREAMS + 1
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "stream capacity was not reused; connected {} targets, host_done={}, bridge_done={}",
                accepted.load(std::sync::atomic::Ordering::SeqCst),
                host.is_finished(),
                bridge.is_finished(),
            )
        });

        drop(extra);
        host.abort();
        bridge.abort();
        target_task.abort();
        let _bounded_config = yamux_config();
    }

    #[tokio::test]
    async fn failed_stream_does_not_stop_another_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind target");
        let target = listener.local_addr().expect("target address");
        let target_task = tokio::spawn(async move {
            let mut accepted = 0;
            while let Ok((mut socket, _)) = listener.accept().await {
                accepted += 1;
                tokio::spawn(async move {
                    if accepted == 1 {
                        return;
                    }
                    let mut buffer = [0; 1024];
                    loop {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(size) => {
                                if socket.write_all(&buffer[..size]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        let (listener, bridge, host) =
            start_ready_session(HostTarget::tcp(target.ip().to_string(), target.port())).await;

        let mut failed = TcpStream::connect(listener)
            .await
            .expect("connect failed stream");
        failed.write_all(b"x").await.expect("write failure marker");
        let mut failure_response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            failed.read_to_end(&mut failure_response),
        )
        .await
        .expect("failed stream close timeout")
        .expect("read failed stream");
        assert!(failure_response.is_empty());
        let mut successful = TcpStream::connect(listener)
            .await
            .expect("connect good stream");
        successful
            .write_all(b"ok")
            .await
            .expect("write good stream");
        successful.shutdown().await.expect("half close good stream");
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            successful.read_to_end(&mut response),
        )
        .await
        .expect("stream relay timeout")
        .expect("read successful stream");
        assert_eq!(response, b"ok");
        host.abort();
        bridge.abort();
        target_task.abort();
    }
}
