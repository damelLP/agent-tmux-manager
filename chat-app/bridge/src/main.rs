mod assets;
mod files;
mod http;
mod notify;
mod policy;
mod relay;
#[cfg(test)]
mod tests;

use std::{io::Write, net::Ipv4Addr, path::PathBuf};

use anyhow::{ensure, Context, Result};
use axum::http::{HeaderValue, Uri};
use clap::Parser;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Serve ATM chat on loopback, behind Tailscale Serve.
#[derive(Parser)]
struct Args {
    #[arg(long)]
    owner_login: String,
    #[arg(long)]
    public_origin: String,
    #[arg(long)]
    socket: PathBuf,
    #[arg(long, default_value_t = 8765)]
    port: u16,
    #[arg(long)]
    page_dir: Option<PathBuf>,
    /// Enable HTTP uploads and downloads from this directory.
    #[arg(long)]
    files_dir: Option<PathBuf>,
    /// Allow file reads under an existing canonical directory (repeatable).
    #[arg(long = "serve-root", value_name = "DIR", requires = "files_dir")]
    serve_roots: Vec<PathBuf>,
    /// Opt in to phone push; store private notification state outside served directories.
    #[arg(long)]
    notify_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let serve_port = origin_port(&args.public_origin)?;
    ensure!(
        !args.owner_login.is_empty() && args.owner_login.bytes().all(|b| b.is_ascii_graphic()),
        "invalid owner login"
    );
    ensure!(!args.socket.as_os_str().is_empty(), "empty socket path");
    let stop = CancellationToken::new();
    let notifications = if let Some(dir) = args.notify_dir {
        let mut excluded = args.serve_roots.clone();
        excluded.extend(args.files_dir.iter().cloned());
        excluded.extend(args.page_dir.clone().or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|p| p.with_file_name("web").join("dist"))
        }));
        match notify::Notifications::start(dir, excluded, args.socket.clone(), stop.clone()).await {
            Ok(n) => Some(n),
            Err(error) => {
                eprintln!("Chat notifications disabled: {error:#}");
                None
            }
        }
    } else {
        None
    };
    let state = http::AppState {
        login: HeaderValue::from_str(&args.owner_login).context("invalid owner login")?,
        origin: HeaderValue::from_str(&args.public_origin)?,
        assets: assets::load(args.page_dir).await?,
        socket: args.socket,
        stop: stop.clone(),
    };
    let store = if let Some(root) = args.files_dir {
        Some(
            files::Store::prepare(root, args.serve_roots)
                .await
                .context("prepare file roots")?,
        )
    } else {
        None
    };
    let app = http::router_with_features(state, store, notifications);
    let bind = TcpListener::bind((Ipv4Addr::LOCALHOST, args.port));
    let listener = tokio::time::timeout(relay::IO, bind).await??;
    let addr = listener.local_addr()?;
    let ready = format!(
        "Local: http://{addr}/\nTailnet: {}/\ntailscale serve --https={serve_port} http://{addr}",
        args.public_origin
    );
    tokio::task::spawn_blocking(move || writeln!(std::io::stdout().lock(), "{ready}")).await??;
    axum::serve(http::TimedListener(listener), app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            stop.cancel();
        })
        .await?;
    Ok(())
}

fn origin_port(origin: &str) -> Result<u16> {
    let uri: Uri = origin.parse().context("invalid public origin")?;
    let host = uri.host().context("public origin needs a host")?;
    let port = uri.port_u16().unwrap_or(443);
    let canonical = if port == 443 {
        format!("https://{host}")
    } else {
        format!("https://{host}:{port}")
    };
    let dns = host
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-'));
    ensure!(
        origin == canonical && port != 0 && dns,
        "expected canonical https://host[:port]"
    );
    Ok(port)
}

#[cfg(test)]
#[test]
fn serve_root_requires_files_dir() {
    let result = Args::try_parse_from([
        "atm-chat-bridge",
        "--owner-login",
        "fixture-owner",
        "--public-origin",
        "https://fixture.invalid",
        "--socket",
        "fixture.sock",
        "--serve-root",
        "/fixture/docs/plans",
    ]);
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(clap::error::ErrorKind::MissingRequiredArgument),
    );
}
