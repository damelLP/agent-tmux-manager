use std::{future::Future, io, mem, path::Path, time::Duration};

use anyhow::{ensure, Result};
use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{sleep_until, timeout, Instant},
};
use tokio_util::sync::CancellationToken;

use crate::policy::{self, Denial};

pub(crate) const MAX_TEXT: usize = (1 << 20) - 1;
pub(crate) const IO: Duration = Duration::from_secs(10);
pub(crate) const IDLE: Duration = Duration::from_secs(300);

#[derive(Debug)]
pub(crate) enum End {
    Unavailable,
    Disconnected,
    Idle,
    Client,
    Closing,
}

impl End {
    pub(crate) fn close_frame(&self) -> Option<CloseFrame> {
        let (code, reason) = match self {
            Self::Unavailable => (close_code::ERROR, "daemon unavailable"),
            Self::Disconnected => (close_code::ERROR, "daemon disconnected"),
            Self::Idle => (close_code::AWAY, "idle timeout"),
            Self::Client | Self::Closing => return None,
        };
        Some(CloseFrame {
            code,
            reason: reason.into(),
        })
    }
}

pub(crate) async fn run(mut ws: WebSocket, socket: &Path, stop: &CancellationToken) {
    let result = tokio::select! {
        biased;
        _ = stop.cancelled() => return,
        result = relay(&mut ws, socket) => result,
    };
    // The Unix stream is already dropped, even if the close write stalls.
    let deadline = Duration::from_secs(2);
    match result {
        // Tungstenite's next read flushes its queued close acknowledgment.
        Err(End::Closing) => {
            let _ = timeout(deadline, ws.recv()).await;
        }
        Err(end) => {
            if let Some(reply) = end.close_frame() {
                let _ = timeout(deadline, ws.send(Message::Close(Some(reply)))).await;
            }
        }
        Ok(()) => {}
    }
}

async fn bounded<T, E>(
    duration: Duration,
    io: impl Future<Output = Result<T, E>>,
    end: End,
) -> Result<T, End> {
    let result = timeout(duration, io).await;
    result.ok().and_then(Result::ok).ok_or(end)
}

pub(crate) async fn dial(socket: &Path) -> io::Result<UnixStream> {
    timeout(Duration::from_secs(5), UnixStream::connect(socket)).await?
}

pub(crate) async fn read_before_idle<T>(
    at: Instant,
    read: impl Future<Output = T>,
) -> Result<T, End> {
    // At the deadline prefer the protocol close over a simultaneous TCP read timeout.
    tokio::select! {
        biased;
        _ = sleep_until(at) => Err(End::Idle),
        value = read => Ok(value),
    }
}

async fn relay(ws: &mut WebSocket, socket: &Path) -> Result<(), End> {
    let mut stream = dial(socket).await.map_err(|_| End::Unavailable)?;
    let (read, mut write) = stream.split();
    let mut read = BufReader::new(read);
    let mut partial = Vec::new();
    let mut first = true;
    let mut client_at = Instant::now() + IO;
    let mut server_at = Instant::now() + IDLE;
    loop {
        let outbound = tokio::select! {
            record = read_before_idle(server_at, read_line(&mut read, &mut partial)) => {
                let text = record?.map_err(|_| End::Disconnected)?;
                server_at = Instant::now() + IDLE;
                Some(Message::Text(text.into()))
            }
            received = read_before_idle(client_at, ws.recv()) => {
                let Some(message) = received? else { return Ok(()); };
                let response = match message.map_err(|_| End::Client)? {
                    Message::Text(text) => match policy::classify(text.as_str(), first) {
                        Ok(()) => {
                            bounded(IO, async {
                                write.write_all(text.as_bytes()).await?;
                                write.write_all(b"\n").await
                            }, End::Disconnected).await?;
                            if first { first = false; server_at = Instant::now() + IO; }
                            None
                        }
                        Err(denial) => Some(Message::Text(denial.frame().into())),
                    },
                    Message::Binary(_) => Some(Message::Text(Denial::default().frame().into())),
                    Message::Close(_) => return Err(End::Closing),
                    Message::Ping(_) | Message::Pong(_) => None,
                };
                if !first { client_at = Instant::now() + IDLE; }
                response
            }
        };
        if let Some(message) = outbound {
            bounded(IO, ws.send(message), End::Client).await?;
        }
    }
}

/// The caller owns partial bytes: read_until is cancellation-safe and this
/// buffer outlives the select branch. Do not carry a partial write forward.
pub(crate) async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    partial: &mut Vec<u8>,
) -> Result<String> {
    let available = (MAX_TEXT + 1).saturating_sub(partial.len());
    let mut bounded = reader.take(available as u64);
    bounded.read_until(b'\n', partial).await?;
    ensure!(partial.pop() == Some(b'\n'), "missing Unix record LF");
    Ok(String::from_utf8(mem::take(partial))?)
}
