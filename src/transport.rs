use crate::error::Error;
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

pub async fn connect(path: &Path, port: u32) -> Result<UnixStream, Error> {
    tokio::time::timeout(Duration::from_secs(5), handshake(path, port))
        .await
        .map_err(|_| Error::unavailable("guest_connect_timeout"))?
}

async fn handshake(path: &Path, port: u32) -> Result<UnixStream, Error> {
    let mut stream = UnixStream::connect(path).await.map_err(|e| Error::io("vsock_connect", e))?;
    stream
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .await
        .map_err(|e| Error::io("vsock_write", e))?;
    let mut reply = Vec::new();
    while reply.len() < 32 {
        let byte = stream.read_u8().await.map_err(|e| Error::io("vsock_ack_read", e))?;
        if byte == b'\n' {
            let reply = std::str::from_utf8(&reply).unwrap_or("");
            if reply.strip_prefix("OK ").is_some_and(|p| p.parse::<u32>().is_ok()) {
                return Ok(stream);
            }
            break;
        }
        reply.push(byte);
    }
    Err(Error::unavailable("invalid_vsock_ack"))
}

pub struct Tunnel {
    pub guest: UnixStream,
    pub stop: tokio_util::sync::CancellationToken,
    pub permit: tokio::sync::OwnedSemaphorePermit,
}

impl Tunnel {
    pub async fn run(mut self, upgrade: hyper::upgrade::OnUpgrade) {
        let started = std::time::Instant::now();
        let operation = uuid::Uuid::new_v4().to_string();
        let _permit = self.permit;
        let result = tokio::select! {
            _ = self.stop.cancelled() => return,
            result = tokio::time::timeout(Duration::from_secs(5), upgrade) => result,
        };
        if let Ok(Ok(upgraded)) = result {
            let mut host = hyper_util::rt::TokioIo::new(upgraded);
            tokio::select! {
                _ = self.stop.cancelled() => {},
                result = tokio::io::copy_bidirectional(&mut host, &mut self.guest) => {
                    crate::error::audit(&operation, "vsock_tunnel", "closed", started, result.is_ok());
                    if let Ok((sent, received)) = result {
                        tracing::info!(operation_id = %operation, operation = "vsock_tunnel", stage = "transfer", sent_bytes=sent, received_bytes=received, result="success");
                    }
                }
            }
        } else {
            crate::error::audit(&operation, "vsock_tunnel", "upgrade", started, false);
        }
    }
}
