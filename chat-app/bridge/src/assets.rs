use std::{
    collections::HashMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    thread,
};

use anyhow::{ensure, Context, Result};
use axum::body::Bytes;
use tokio::{sync::oneshot, time::timeout};

use crate::relay::IO;

const HTML: &str = "text/html; charset=utf-8";
const MAX_BYTES: usize = 16 * 1024 * 1024;
pub(crate) type Assets = HashMap<String, (&'static str, Bytes)>;

pub(crate) async fn load(dir: Option<PathBuf>) -> Result<Assets> {
    let (tx, rx) = oneshot::channel();
    // A stuck build filesystem must not keep the async runtime alive on exit.
    thread::Builder::new()
        .name("chat-assets".into())
        .spawn(move || {
            let _ = tx.send(snapshot(dir));
        })?;
    timeout(IO, rx)
        .await
        .context("loading chat assets timed out")??
}

fn snapshot(dir: Option<PathBuf>) -> Result<Assets> {
    let root = match dir {
        Some(path) => path,
        None => {
            let root = env::current_exe()?.with_file_name("web").join("dist");
            if !root.try_exists()? {
                let index = Bytes::from_static(include_bytes!("../assets/index.html"));
                return Ok(Assets::from([("/index.html".into(), (HTML, index))]));
            }
            root
        }
    };
    ensure!(
        root.symlink_metadata()?.is_dir(),
        "page dir must be a real directory"
    );
    let mut assets = Assets::new();
    let mut left = MAX_BYTES;
    scan(&root, "/", &mut assets, &mut left)?;
    let sub = root.join("assets");
    if sub.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
        scan(&sub, "/assets/", &mut assets, &mut left)?;
    }
    ensure!(
        assets.contains_key("/index.html"),
        "page dir needs index.html"
    );
    Ok(assets)
}

fn scan(dir: &Path, prefix: &str, assets: &mut Assets, left: &mut usize) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| !name.starts_with('.')) else {
            continue;
        };
        if !entry.file_type()?.is_file() {
            continue;
        }
        ensure!(assets.len() < 128, "too many page assets");
        let path = entry.path();
        let mut content = Vec::new();
        let file = fs::File::open(&path)?;
        file.take(*left as u64 + 1).read_to_end(&mut content)?;
        ensure!(content.len() <= *left, "page assets exceed 16 MiB");
        *left -= content.len();
        let asset = (mime(&path), Bytes::from(content));
        assets.insert(format!("{prefix}{name}"), asset);
    }
    Ok(())
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => HTML,
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map" | "webmanifest") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
