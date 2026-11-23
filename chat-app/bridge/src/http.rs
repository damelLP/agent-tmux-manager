use std::{io, net::SocketAddr, path::PathBuf, pin::Pin, sync::Arc};

use axum::{
    body::Body,
    extract::{rejection::QueryRejection, ws::WebSocketUpgrade, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Router,
};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
};
use tokio_io_timeout::TimeoutStream;
use tokio_util::sync::CancellationToken;

use crate::{
    assets::Assets,
    files::{Failure, Store},
    notify::{self, Notifications},
    relay,
};

pub(crate) struct AppState {
    pub(crate) login: HeaderValue,
    pub(crate) origin: HeaderValue,
    pub(crate) assets: Assets,
    pub(crate) socket: PathBuf,
    pub(crate) stop: CancellationToken,
}

#[cfg(test)]
pub(crate) fn router(state: AppState) -> Router {
    router_with_features(state, None, None)
}

pub(crate) fn router_with_features(
    state: AppState,
    store: Option<Store>,
    notifications: Option<Arc<Notifications>>,
) -> Router {
    let state = Arc::new(state);
    let has_files = store.is_some();
    let mut router = Router::new()
        .route("/ws", get(upgrade))
        .route("/health", get(health));
    if let Some(store) = store {
        router = router.merge(
            Router::new()
                .route("/upload", post(upload))
                .route("/file", get(file))
                .layer(Extension(Arc::new(store))),
        );
    }
    if let Some(notifications) = notifications {
        router = router.merge(notify::routes(notifications));
    }
    router
        .fallback(get(page))
        .layer(middleware::from_fn_with_state(
            (state.clone(), has_files),
            authorize,
        ))
        .with_state(state)
}

fn single_match(headers: &HeaderMap, name: &str, expected: &HeaderValue) -> bool {
    let mut values = headers.get_all(name).iter();
    values.next() == Some(expected) && values.next().is_none()
}

async fn authorize(
    State((state, has_files)): State<(Arc<AppState>, bool)>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let is_file = has_files && path == "/file";
    let needs_origin = path == "/ws"
        || (has_files && path == "/upload")
        || (path == notify::SUBSCRIPTION && req.method() == Method::POST);
    let mut response = if !single_match(req.headers(), "tailscale-user-login", &state.login) {
        json_response(StatusCode::FORBIDDEN, r#"{"error":"identity"}"#)
    } else if needs_origin && !single_match(req.headers(), "origin", &state.origin) {
        json_response(StatusCode::FORBIDDEN, r#"{"error":"origin"}"#)
    } else if is_file
        && req
            .headers()
            .get_all("sec-fetch-site")
            .iter()
            .any(|site| matches!(site.as_bytes(), b"cross-site" | b"same-site"))
    {
        json_response(StatusCode::FORBIDDEN, r#"{"error":"site"}"#)
    } else {
        next.run(req).await
    };
    if is_file {
        for (name, value) in [
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "sandbox; default-src 'none'",
            ),
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "same-origin",
            ),
        ] {
            response
                .headers_mut()
                .insert(name, HeaderValue::from_static(value));
        }
    }
    response
}

pub(crate) fn json_response(status: StatusCode, body: impl IntoResponse) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

fn file_error(error: Failure) -> Response {
    let (status, code) = match error {
        Failure::Busy => (StatusCode::SERVICE_UNAVAILABLE, "busy"),
        Failure::Full => (StatusCode::INSUFFICIENT_STORAGE, "full"),
        Failure::Size => (StatusCode::PAYLOAD_TOO_LARGE, "size"),
        Failure::Path => (StatusCode::FORBIDDEN, "path"),
        Failure::Gone => (StatusCode::NOT_FOUND, "gone"),
        Failure::Store => (StatusCode::INTERNAL_SERVER_ERROR, "store"),
    };
    json_response(status, format!(r#"{{"error":"{code}"}}"#))
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
}
#[derive(Deserialize)]
struct ReadQuery {
    p: PathBuf,
    thumb: Option<u8>,
}

async fn upload(
    State(state): State<Arc<AppState>>,
    Extension(store): Extension<Arc<Store>>,
    query: Result<Query<UploadQuery>, QueryRejection>,
    body: Body,
) -> Response {
    let Ok(Query(query)) = query else {
        return json_response(StatusCode::BAD_REQUEST, r#"{"error":"name"}"#);
    };
    let result = tokio::select! {
        result = store.upload(query.name, body) => result,
        _ = state.stop.cancelled() => Err(Failure::Store),
    };
    match result.and_then(|upload| serde_json::to_string(&upload).map_err(|_| Failure::Store)) {
        Ok(json) => json_response(StatusCode::CREATED, json),
        Err(error) => file_error(error),
    }
}

async fn file(
    Extension(store): Extension<Arc<Store>>,
    query: Result<Query<ReadQuery>, QueryRejection>,
    method: Method,
) -> Response {
    let Ok(Query(query)) = query else {
        return file_error(Failure::Path);
    };
    let found = match store
        .read(query.p, method == Method::HEAD, query.thumb == Some(1))
        .await
    {
        Ok(found) => found,
        Err(error) => return file_error(error),
    };
    let cache = if found.mutable {
        "no-store"
    } else {
        "private, max-age=31536000, immutable"
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, found.mime),
            (header::CACHE_CONTROL, cache),
        ],
        found.data,
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(found.size));
    if let Some(name) = found.download_name {
        let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
        headers.insert(header::CONTENT_DISPOSITION, disposition);
    }
    response
}

async fn health(State(state): State<Arc<AppState>>) -> Response {
    // Socket liveness only: don't open a protocol session or expose dial details.
    let daemon = if relay::dial(&state.socket).await.is_ok() {
        "up"
    } else {
        "down"
    };
    json_response(
        StatusCode::OK,
        format!(r#"{{"bridge":"ok","daemon":"{daemon}","protocol":"1.1"}}"#),
    )
}

async fn page(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let path = match req.uri().path() {
        "/" => "/index.html",
        path => path,
    };
    match state.assets.get(path) {
        Some((mime, data)) => (
            [
                (header::CONTENT_TYPE, *mime),
                (header::CACHE_CONTROL, "no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            data.clone(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(relay::MAX_TEXT)
        .max_frame_size(relay::MAX_TEXT)
        .write_buffer_size(0)
        .max_write_buffer_size(relay::MAX_TEXT + 1024)
        .on_upgrade(
            move |stream| async move { relay::run(stream, &state.socket, &state.stop).await },
        )
}

pub(crate) struct TimedListener(pub(crate) TcpListener);

pub(crate) fn timed_io<T: AsyncRead + AsyncWrite>(io: T) -> Pin<Box<TimeoutStream<T>>> {
    let mut io = TimeoutStream::new(io);
    io.set_read_timeout(Some(relay::IDLE));
    io.set_write_timeout(Some(relay::IO));
    Box::pin(io)
}

impl axum::serve::Listener for TimedListener {
    type Io = Pin<Box<TimeoutStream<TcpStream>>>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (io, peer) = axum::serve::Listener::accept(&mut self.0).await;
        (timed_io(io), peer)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.0.local_addr()
    }
}
