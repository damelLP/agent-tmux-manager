//! Register channel methods in the existing request router.

use super::{channel_route::RouteHandle, channels, messages::MessageHandle, OrgHandle};
use crate::server::methods::{parse_params, MethodError, MethodRouter};
use atm_comms::channel_types::{
    ChannelEditParams, ChannelHistoryParams, ChannelReactParams, ChannelRemoveParams,
};
use atm_org::{MessageId, PostMutation};
use atm_protocol::error_code;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use std::time::Duration;

const READ_BUDGET: Duration = Duration::from_secs(8); // One actor snapshot (5 s) plus projection.
const SEEN_BUDGET: Duration = Duration::from_secs(20); // Two snapshots (5 s each), execute (6 s), margin.
const POST_BUDGET: Duration = Duration::from_secs(25); // The post's own 20 s service budget, margin.
const ROUTE_BUDGET: Duration = Duration::from_secs(25); // Includes queuing behind a routing pass.

pub(crate) fn register(
    router: &mut MethodRouter,
    org: OrgHandle,
    now: fn() -> DateTime<Utc>,
) -> Result<(), MethodError> {
    let list_org = org.clone();
    router.register("channel.list", READ_BUDGET, move |caller, params| {
        let org = list_org.clone();
        async move {
            let p = parse_params(params)?;
            let result = channels::list(&org, &caller, p).await?;
            serde_json::to_value(result).map_err(channels::json_error)
        }
    })?;
    let history_org = org.clone();
    router.register("channel.history", READ_BUDGET, move |caller, params| {
        let org = history_org.clone();
        async move {
            let p = parse_params(params)?;
            validate_history(&p)?;
            let result = channels::history(&org, &caller, p).await?;
            serde_json::to_value(result).map_err(channels::json_error)
        }
    })?;
    router.register("channel.seen", SEEN_BUDGET, move |caller, params| {
        let org = org.clone();
        async move {
            let p = parse_params(params)?;
            let result = channels::seen(&org, &caller, p, now()).await?;
            serde_json::to_value(result).map_err(channels::json_error)
        }
    })
}

/// `channel.post` lives on the message service: its wakes are messages.
pub(crate) fn register_post(
    router: &mut MethodRouter,
    messages: MessageHandle,
) -> Result<(), MethodError> {
    let posting = messages.clone();
    router.register("channel.post", POST_BUDGET, move |caller, params| {
        let posting = posting.clone();
        async move {
            let p = parse_params(params)?;
            let result = posting.post(p, caller).await?;
            serde_json::to_value(result).map_err(channels::json_error)
        }
    })?;
    register_mutation(router, &messages, "channel.edit", |p: ChannelEditParams| {
        (p.post, PostMutation::Edit(p.body))
    })?;
    register_mutation(
        router,
        &messages,
        "channel.remove",
        |p: ChannelRemoveParams| (p.post, PostMutation::Remove),
    )?;
    register_mutation(
        router,
        &messages,
        "channel.react",
        |p: ChannelReactParams| (p.post, PostMutation::React(p.emoji)),
    )
}

fn register_mutation<P: DeserializeOwned + 'static>(
    router: &mut MethodRouter,
    messages: &MessageHandle,
    name: &'static str,
    decode: fn(P) -> (MessageId, PostMutation),
) -> Result<(), MethodError> {
    let messages = messages.clone();
    router.register(name, POST_BUDGET, move |caller, params| {
        let messages = messages.clone();
        async move {
            let (post, change) = decode(parse_params(params)?);
            let result = messages.mutate_post(caller, post, change).await?;
            serde_json::to_value(result).map_err(channels::json_error)
        }
    })
}

pub(crate) fn register_routing(
    router: &mut MethodRouter,
    routes: Option<RouteHandle>,
) -> Result<(), MethodError> {
    let attach = routes.clone();
    router.register_scoped(
        "router.attach",
        ROUTE_BUDGET,
        move |caller, scope, params| {
            let attach = attach.clone();
            async move {
                let result = attach
                    .ok_or_else(routing_unavailable)?
                    .attach(&caller, scope, parse_params(params)?)
                    .await?;
                serde_json::to_value(result).map_err(channels::json_error)
            }
        },
    )?;
    router.register_scoped(
        "channel.route",
        ROUTE_BUDGET,
        move |caller, scope, params| {
            let routes = routes.clone();
            async move {
                let result = routes
                    .ok_or_else(routing_unavailable)?
                    .answer(&caller, scope, parse_params(params)?)
                    .await?;
                serde_json::to_value(result).map_err(channels::json_error)
            }
        },
    )
}

fn routing_unavailable() -> MethodError {
    MethodError::new(error_code::UNAVAILABLE, "channel routing is not configured")
}

fn validate_history(p: &ChannelHistoryParams) -> Result<(), MethodError> {
    if p.thread.is_some() && p.include_shared_replies {
        return Err(MethodError::new(
            error_code::INVALID_PARAMS,
            "include_shared_replies requires a main history page",
        ));
    }
    if !(1..=200).contains(&p.limit) {
        return Err(MethodError::new(
            error_code::INVALID_PARAMS,
            "limit must be between 1 and 200",
        ));
    }
    Ok(())
}
