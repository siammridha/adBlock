//! UDP and TCP listeners: frame DNS messages off the wire and hand them to
//! `DnsService`.

use std::sync::Arc;
use std::time::Duration;

use hickory_proto::op::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

use super::DnsService;
use crate::dns::error::{Error, Result};

const CLASSIC_UDP_LIMIT: u16 = 512;
const TCP_IDLE: Duration = Duration::from_secs(30);

pub(super) async fn bind(addr: std::net::SocketAddr) -> Result<(UdpSocket, TcpListener)> {
    let udp = UdpSocket::bind(addr)
        .await
        .map_err(|e| Error::Config(format!("binding udp {addr}: {e}")))?;
    let tcp = TcpListener::bind(addr)
        .await
        .map_err(|e| Error::Config(format!("binding tcp {addr}: {e}")))?;
    tracing::info!(%addr, "dns server listening (udp + tcp)");
    Ok((udp, tcp))
}

pub(super) fn spawn_listeners(
    svc: Arc<DnsService>,
    udp: UdpSocket,
    tcp: TcpListener,
) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let udp_task = tokio::spawn(serve_udp(svc.clone(), udp));
    let tcp_task = tokio::spawn(serve_tcp(svc, tcp));
    (udp_task, tcp_task)
}

async fn serve_udp(svc: Arc<DnsService>, socket: UdpSocket) {
    let socket = Arc::new(socket);
    let mut buf = [0u8; 4096];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "dns udp recv");
                continue;
            }
        };
        let Ok(request) = Message::from_vec(&buf[..n]) else {
            continue;
        };
        let svc = svc.clone();
        let socket = socket.clone();
        tokio::spawn(async move {
            let response = svc.handle(&request).await;
            if let Some(wire) = encode_udp(&response, &request) {
                let _ = socket.send_to(&wire, peer).await;
            }
        });
    }
}

fn encode_udp(response: &Message, request: &Message) -> Option<Vec<u8>> {
    let limit = request
        .edns
        .as_ref()
        .map_or(CLASSIC_UDP_LIMIT, |e| e.max_payload().max(CLASSIC_UDP_LIMIT))
        as usize;
    let wire = response.to_vec().ok()?;
    if wire.len() <= limit {
        return Some(wire);
    }
    let mut truncated = response.truncate();
    truncated.metadata.id = response.metadata.id;
    truncated.to_vec().ok()
}

async fn serve_tcp(svc: Arc<DnsService>, listener: TcpListener) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "dns tcp accept");
                continue;
            }
        };
        let svc = svc.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_tcp_conn(&svc, stream).await {
                tracing::debug!(error = %e, %peer, "dns tcp conn ended");
            }
        });
    }
}

async fn serve_tcp_conn(
    svc: &DnsService,
    mut stream: tokio::net::TcpStream,
) -> std::result::Result<(), String> {
    loop {
        let mut len = [0u8; 2];
        match tokio::time::timeout(TCP_IDLE, stream.read_exact(&mut len)).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => return Ok(()),
        }
        let n = usize::from(u16::from_be_bytes(len));
        let mut buf = vec![0u8; n];
        stream
            .read_exact(&mut buf)
            .await
            .map_err(|e| e.to_string())?;
        let Ok(request) = Message::from_vec(&buf) else {
            return Err("unparseable query".into());
        };
        let response = svc.handle(&request).await;
        let wire = response.to_vec().map_err(|e| e.to_string())?;
        let framed_len =
            u16::try_from(wire.len()).map_err(|_| "response too large for tcp".to_string())?;
        let mut framed = Vec::with_capacity(2 + wire.len());
        framed.extend_from_slice(&framed_len.to_be_bytes());
        framed.extend_from_slice(&wire);
        stream
            .write_all(&framed)
            .await
            .map_err(|e| e.to_string())?;
    }
}

#[cfg(test)]
pub(super) async fn bind_ephemeral(
    svc: Arc<DnsService>,
) -> (std::net::SocketAddr, std::net::SocketAddr) {
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (udp_addr, tcp_addr) = (udp.local_addr().unwrap(), tcp.local_addr().unwrap());
    tokio::spawn(serve_udp(svc.clone(), udp));
    tokio::spawn(serve_tcp(svc, tcp));
    (udp_addr, tcp_addr)
}
