//! Private owner reads using the existing ATM 1.1 Unix protocol, not the WebSocket relay.
use std::{future::Future, path::Path, time::Duration};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{sleep, timeout, timeout_at, Instant},
};
use tokio_util::sync::CancellationToken;

use crate::relay::{self, IO};

fn text<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key)?.as_str()
}

#[derive(Deserialize)]
pub(super) struct Fact {
    pub(super) seq: u64,
    kind: String,
    body: Value,
}

#[derive(Clone, PartialEq, Eq)]
enum Source {
    Post {
        channel: String,
        thread: Option<String>,
    },
    Inbox(String),
    Decision,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Notice {
    seq: u64,
    source: Source,
}

#[derive(Serialize)]
pub(super) struct Preview {
    body: String,
    target: Value,
}

fn contains(row: &Value, key: &str, value: &str) -> bool {
    row.get(key)
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(value)))
}

fn has(row: &Value, key: &str) -> bool {
    row.get(key).is_some_and(|v| !v.is_null())
}

fn dm(author: &str) -> String {
    if author == "system" {
        "owner+system".into()
    } else {
        format!("{author}+owner")
    }
}

impl Notice {
    pub(super) fn select(f: &Fact) -> Option<Self> {
        let b = &f.body;
        let source = match f.kind.as_str() {
            "channel.posted" if !has(b, "decision") => {
                let author = text(b, "author")?;
                let channel = text(b, "channel")?;
                if author == "owner" || (channel != dm(author) && !contains(b, "mentions", "owner"))
                {
                    return None;
                }
                Source::Post {
                    channel: channel.into(),
                    thread: text(b, "thread").map(str::to_owned),
                }
            }
            "msg.sent"
                if text(b, "to") == Some("owner")
                    && !has(b, "post")
                    && !has(b, "delivery_notice") =>
            {
                let author = text(b, "from")?;
                if author == "owner" {
                    return None;
                }
                Source::Inbox(dm(author))
            }
            // Requesting pings once; a provisional take keeps that preview open, not a new ping.
            "decision.requested" if text(b, "requester")? != "owner" => Source::Decision,
            _ => return None,
        };
        Some(Self { seq: f.seq, source })
    }

    pub(super) fn cleared_by(&self, f: &Fact) -> bool {
        let b = &f.body;
        if f.kind == "channel.seen" {
            let channel = match &self.source {
                Source::Post { channel, .. } | Source::Inbox(channel) => channel,
                Source::Decision => return false,
            };
            return text(b, "reader") == Some("owner")
                && text(b, "channel") == Some(channel)
                && text(b, "through")
                    .and_then(|id| id.strip_prefix('m'))
                    .and_then(|n| n.parse::<u64>().ok())
                    .is_some_and(|n| n >= self.seq);
        }
        match (&self.source, f.kind.as_str()) {
            (Source::Post { .. }, "channel.post_removed") => {
                text(b, "post") == Some(&format!("m{}", self.seq))
            }
            (Source::Inbox(_), "msg.read") => contains(b, "message_ids", &format!("m{}", self.seq)),
            (
                Source::Decision,
                "decision.resolved" | "decision.overruled" | "decision.expired"
                | "decision.withdrawn",
            ) => text(b, "decision_id") == Some(&format!("d{}", self.seq)),
            _ => false,
        }
    }

    pub(super) async fn preview(&self, socket: &Path) -> Result<Option<Preview>> {
        let mut client = Client::open(socket).await?;
        let id = format!("m{}", self.seq);
        let (body, target) = match &self.source {
            Source::Post { channel, thread } => {
                let (params, target) = if let Some(thread) = thread {
                    let after = format!("m{}", self.seq.saturating_sub(1));
                    (
                        json!({"channel":channel,"limit":1,"thread":thread,"after":after}),
                        json!({"channel":channel,"thread":thread}),
                    )
                } else {
                    let before = self.seq.checked_add(1).map(|n| format!("m{n}"));
                    (
                        json!({"channel":channel,"limit":1,"before":before}),
                        json!({"channel":channel}),
                    )
                };
                let Some(result) = client.call("channel.history", params).await? else {
                    return Ok(None);
                };
                ensure!(
                    text(&result, "channel") == Some(channel),
                    "wrong notification channel"
                );
                let Some(row) = row(&result, "posts", "id", &id)? else {
                    return Ok(None);
                };
                if row.get("removed").and_then(Value::as_bool) == Some(true) {
                    return Ok(None);
                }
                (snippet(text(row, "body"))?, target)
            }
            Source::Inbox(channel) => {
                let result = client
                    .call(
                        "msg.inbox",
                        json!({"for":"owner","id":id,"limit":1,"unread_only":true}),
                    )
                    .await?;
                let Some(result) = result else {
                    return Ok(None);
                };
                let Some(row) = row(&result, "messages", "message_id", &id)? else {
                    return Ok(None);
                };
                if has(row, "read_at") {
                    return Ok(None);
                }
                (
                    snippet(row.get("content").and_then(|c| text(c, "body")))?,
                    json!({"channel":channel}),
                )
            }
            Source::Decision => {
                let id = format!("d{}", self.seq);
                let Some(result) = client.call("decision.get", json!({"id":id})).await? else {
                    return Ok(None);
                };
                let row = result
                    .get("decision")
                    .context("missing notification decision")?;
                ensure!(text(row, "id") == Some(&id), "wrong notification decision");
                let status = text(row, "status").context("missing notification decision status")?;
                if !matches!(status, "pending" | "taken_provisionally") {
                    return Ok(None);
                }
                (
                    snippet(row.get("request").and_then(|r| text(r, "title")))?,
                    json!({"decision":id}),
                )
            }
        };
        Ok(Some(Preview { body, target }))
    }
}

fn row<'a>(page: &'a Value, key: &str, by: &str, id: &str) -> Result<Option<&'a Value>> {
    Ok(page
        .get(key)
        .and_then(Value::as_array)
        .context("invalid notification page")?
        .iter()
        .find(|row| text(row, by) == Some(id)))
}

fn snippet(body: Option<&str>) -> Result<String> {
    Ok(body
        .context("missing notification text")?
        .trim()
        .chars()
        .take(160)
        .collect())
}

struct Client {
    stream: BufReader<UnixStream>,
    partial: Vec<u8>,
}

impl Client {
    async fn open(socket: &Path) -> Result<Self> {
        let mut client = Self {
            stream: BufReader::new(relay::dial(socket).await?),
            partial: Vec::new(),
        };
        client
            .send(json!({"type":"connect","client_id":"chat-notify"}))
            .await?;
        let hello = timeout(IO, client.recv()).await??;
        ensure!(
            text(&hello, "type") == Some("connected"),
            "notification handshake refused"
        );
        Ok(client)
    }

    async fn send(&mut self, mut frame: Value) -> Result<()> {
        frame
            .as_object_mut()
            .context("notification frame must be an object")?
            .insert("protocol_version".into(), json!({"major":1,"minor":1}));
        timeout(
            IO,
            self.stream
                .get_mut()
                .write_all(format!("{frame}\n").as_bytes()),
        )
        .await??;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Value> {
        let line = relay::read_line(&mut self.stream, &mut self.partial).await?;
        Ok(serde_json::from_str(&line)?)
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Option<Value>> {
        self.send(json!({"type":"request","id":"notice","method":method,"params":params}))
            .await?;
        let response = timeout(IO, self.recv()).await??;
        ensure!(
            text(&response, "type") == Some("response") && text(&response, "id") == Some("notice"),
            "wrong notification reply"
        );
        if let Some(result) = response.get("result") {
            Ok(Some(result.clone()))
        } else if response.get("error").and_then(|e| text(e, "code")) == Some("not_found") {
            Ok(None)
        } else {
            bail!("notification read unavailable")
        }
    }
}

pub(super) async fn follow<F: Future<Output = ()>>(
    socket: &Path,
    stop: &CancellationToken,
    mut accept: impl FnMut(Fact) -> F,
) {
    let mut cursor = None;
    let mut last_warning: Option<Instant> = None;
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            _ = session(socket, &mut cursor, &mut accept) => {},
        }
        if last_warning.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
            eprintln!("Chat notification watch unavailable; retrying");
            last_warning = Some(Instant::now());
        }
        tokio::select! {
            () = stop.cancelled() => return,
            () = sleep(Duration::from_secs(1)) => {},
        }
    }
}

async fn session<F: Future<Output = ()>>(
    socket: &Path,
    cursor: &mut Option<u64>,
    accept: &mut impl FnMut(Fact) -> F,
) -> Result<()> {
    let mut client = Client::open(socket).await?;
    if cursor.is_none() {
        *cursor = Some(
            client
                .call("channel.list", json!({}))
                .await?
                .and_then(|result| result.get("seq").and_then(Value::as_u64))
                .context("missing watch fence")?,
        );
    }
    client
        .send(json!({"type":"watch","cursor":cursor,"topics":["channel.","decision.","msg."]}))
        .await?;
    let period = Duration::from_secs(60);
    let mut deadline = Instant::now() + period;
    let mut awaiting = false;
    loop {
        match timeout_at(deadline, client.recv()).await {
            Ok(frame) => {
                let frame = frame?;
                match text(&frame, "type") {
                    Some("event") => {
                        let fact: Fact = serde_json::from_value(frame)?;
                        if cursor.is_none_or(|seq| fact.seq > seq) {
                            *cursor = Some(fact.seq);
                            accept(fact).await;
                        }
                    }
                    Some("pong")
                        if awaiting && frame.get("seq").and_then(Value::as_u64) == Some(1) =>
                    {
                        awaiting = false;
                        deadline = Instant::now() + period;
                    }
                    Some("error" | "rejected") => bail!("notification watch refused"),
                    _ => {}
                }
            }
            Err(_) if awaiting => bail!("notification heartbeat missed"),
            Err(_) => {
                client.send(json!({"type":"ping","seq":1})).await?;
                awaiting = true;
                deadline = Instant::now() + IO;
            }
        }
    }
}
