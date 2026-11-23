use serde::{de::IgnoredAny, Deserialize};
use serde_json::json;

use crate::relay::MAX_TEXT;

// Deserialize only the routing envelope. In particular, don't normalize or
// interpret the daemon-owned params. Serde rejects duplicate routing fields.
#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "protocol_version")]
    _version: IgnoredAny,
    method: Option<String>,
    id: Option<String>,
    from_pane: Option<String>,
    topics: Option<Vec<String>>,
}

#[derive(Default)]
pub(crate) struct Denial(Option<String>);

impl Denial {
    pub(crate) fn frame(self) -> String {
        const REASON: &str = "frame not permitted by chat bridge";
        self.0
            .map(|id| {
                json!({"type":"response","id":id,"error":{"code":"refused","message":REASON}})
                    .to_string()
            })
            .filter(|frame| frame.len() <= MAX_TEXT)
            .unwrap_or_else(|| {
                json!({"type":"error","code":"refused","message":REASON}).to_string()
            })
    }
}

pub(crate) fn classify(text: &str, first: bool) -> Result<(), Denial> {
    if text.len() > MAX_TEXT || text.contains('\n') || !text.trim_start().starts_with('{') {
        return Err(Denial::default());
    }
    let env: Envelope = serde_json::from_str(text).map_err(|_| Denial::default())?;
    match (first, env.kind.as_str(), env.method.as_deref()) {
        (true, "connect", _) | (false, "ping", _) => Ok(()),
        (false, "watch", _)
            if matches!(env.topics.as_deref(), Some([channel, decision, msg])
                if channel == "channel." && decision == "decision." && msg == "msg.") =>
        {
            Ok(())
        }
        (
            false,
            "request",
            Some(
                "channel.list" | "channel.history" | "channel.post" | "channel.seen"
                | "decision.list" | "decision.resolve" | "decision.withdraw" | "org.tree"
                | "agent.list" | "channel.edit" | "channel.remove" | "channel.react",
            ),
        ) if env.from_pane.is_none() && env.id.is_some() => Ok(()),
        _ => Err(Denial(if env.kind == "request" { env.id } else { None })),
    }
}
