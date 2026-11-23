//! An event-driven router client for tests; every answer is explicitly released.
use atm_comms::channel_types::{ChannelRouteParams, ChannelRouteResult, RouterAttachResult};
use atm_org::MessageId;
use atm_protocol::{ClientMessage, DaemonMessage, ResponseError, ResponsePayload};
use serde_json::{json, Value};
use std::{collections::VecDeque, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::oneshot,
};

pub struct FakeRouter<S> {
    stream: BufReader<S>,
    events: VecDeque<DaemonMessage>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> FakeRouter<S> {
    pub async fn connect(stream: S, label: &str) -> Self {
        let mut client = Self {
            stream: BufReader::new(stream),
            events: VecDeque::new(),
        };
        client
            .send(ClientMessage::connect(Some(label.into())))
            .await;
        assert!(matches!(
            client.receive().await,
            DaemonMessage::Connected { .. }
        ));
        client
    }

    async fn send(&mut self, message: ClientMessage) {
        let line = format!("{}\n", serde_json::to_string(&message).unwrap());
        tokio::time::timeout(
            Duration::from_secs(60),
            self.stream.get_mut().write_all(line.as_bytes()),
        )
        .await
        .unwrap()
        .unwrap();
    }

    async fn receive(&mut self) -> DaemonMessage {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(60), self.stream.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
    }

    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        pane: Option<&str>,
    ) -> Result<Value, ResponseError> {
        self.send(ClientMessage::request(
            "fake".into(),
            method.into(),
            params,
            pane.map(str::to_owned),
        ))
        .await;
        loop {
            match self.receive().await {
                DaemonMessage::Response { id, payload } => {
                    assert_eq!(id, "fake");
                    return match payload {
                        ResponsePayload::Result(v) => Ok(v),
                        ResponsePayload::Error(err) => Err(err),
                    };
                }
                event @ DaemonMessage::Event { .. } => self.events.push_back(event),
                other => panic!("unexpected {method} frame: {other:?}"),
            }
        }
    }

    pub async fn attach(&mut self) -> Result<RouterAttachResult, ResponseError> {
        self.call(
            "router.attach",
            json!({"name":"fake","model":"script"}),
            None,
        )
        .await
        .map(|v| serde_json::from_value(v).unwrap())
    }

    pub async fn watch_after(&mut self, cursor: u64) {
        self.send(ClientMessage::watch(vec!["channel.posted".into()], cursor))
            .await;
        // The request is read after Watch on this transport. Replay events
        // interleaved with its response are buffered for next_pending.
        self.call("daemon.info", json!({}), None).await.unwrap();
    }

    pub async fn next_pending(&mut self, matches: impl Fn(&str) -> bool) -> MessageId {
        loop {
            let event = match self.events.pop_front() {
                Some(event) => event,
                None => self.receive().await,
            };
            let DaemonMessage::Event {
                seq, kind, body, ..
            } = event
            else {
                panic!("unexpected watch frame {event:?}")
            };
            if kind == "channel.posted"
                && body["route"] == true
                && body["body"].as_str().is_some_and(&matches)
            {
                return MessageId::new(seq);
            }
        }
    }

    pub async fn answer(
        &mut self,
        answer: ChannelRouteParams,
    ) -> Result<ChannelRouteResult, ResponseError> {
        self.call("channel.route", serde_json::to_value(answer).unwrap(), None)
            .await
            .map(|v| serde_json::from_value(v).unwrap())
    }

    pub async fn answer_when_released(
        &mut self,
        answer: ChannelRouteParams,
        release: oneshot::Receiver<()>,
    ) -> Result<ChannelRouteResult, ResponseError> {
        release.await.unwrap();
        self.answer(answer).await
    }

    pub async fn disconnect(mut self) {
        self.send(ClientMessage::disconnect()).await;
        let mut line = String::new();
        assert_eq!(self.stream.read_line(&mut line).await.unwrap(), 0);
    }
}
