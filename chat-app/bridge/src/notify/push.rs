use std::{
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use openssl::{
    bn::BigNumContext,
    ec::{EcGroup, EcKey, PointConversionForm},
    ecdsa::EcdsaSig,
    nid::Nid,
    pkey::Private,
    sha::sha256,
};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const AUD: &str = "https://fcm.googleapis.com";
pub(super) const TTL: u64 = 600;
pub(super) type Key = EcKey<Private>;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Endpoint(String);

impl Endpoint {
    pub(super) fn parse(url: &str) -> Result<Self> {
        let path = url.strip_prefix(AUD).context("unsupported push origin")?;
        let token = path
            .strip_prefix("/wp/")
            .or_else(|| path.strip_prefix("/fcm/send/"))
            .context("unsupported push path")?;
        // Besides pinning the HTTPS origin, this alphabet is literal in curl's config grammar.
        ensure!(
            !token.is_empty()
                && token.len() <= 1536
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.~:+=".contains(&b)),
            "invalid push token"
        );
        Ok(Self(url.into()))
    }
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

pub(super) fn new_key() -> Result<Key> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
    Ok(EcKey::generate(&group)?)
}

pub(super) fn public_key(key: &Key) -> Result<String> {
    let mut ctx = BigNumContext::new()?;
    Ok(B64.encode(key.public_key().to_bytes(
        key.group(),
        PointConversionForm::UNCOMPRESSED,
        &mut ctx,
    )?))
}

pub(super) fn request(key: &Key, ep: &Endpoint, now: u64) -> Result<String> {
    let claims = serde_json::to_vec(&json!({"aud":AUD,"exp":now.saturating_add(600)}))?;
    let input = format!(
        "{}.{}",
        B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#),
        B64.encode(claims)
    );
    let sig = EcdsaSig::sign(&sha256(input.as_bytes()), key)?;
    let mut fixed = sig.r().to_vec_padded(32)?;
    fixed.extend(sig.s().to_vec_padded(32)?);
    let auth = format!(
        "vapid t={input}.{}, k={}",
        B64.encode(fixed),
        public_key(key)?
    );
    Ok(format!("url = \"{}\"\nheader = \"Authorization: {auth}\"\nheader = \"TTL: {TTL}\"\nheader = \"Urgency: high\"\nheader = \"Topic: atm-chat\"\nheader = \"Content-Length: 0\"\n", ep.as_str()))
}

pub(super) fn curl() -> Command {
    let mut cmd = Command::new("curl");
    cmd.env_clear().args([
        "--disable",
        "--globoff",
        "--noproxy",
        "*",
        "--proto",
        "=https",
        "--silent",
        "--connect-timeout",
        "4",
        "--max-time",
        "8",
        "--request",
        "POST",
        "--output",
        "/dev/null",
        "--write-out",
        "%{http_code}",
        "--config",
        "-",
    ]);
    cmd
}

pub(super) async fn post(key: &Key, ep: &Endpoint, stop: &CancellationToken) -> Result<u16> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    run(curl(), request(key, ep, now)?, stop).await
}

pub(super) async fn run(mut cmd: Command, config: String, stop: &CancellationToken) -> Result<u16> {
    ensure!(!stop.is_cancelled(), "push cancelled");
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let send = async {
        child
            .stdin
            .take()
            .context("push stdin")?
            .write_all(config.as_bytes())
            .await?;
        let mut code = String::new();
        child
            .stdout
            .take()
            .context("push stdout")?
            .take(4)
            .read_to_string(&mut code)
            .await?;
        ensure!(child.wait().await?.success(), "push transport unavailable");
        Ok(code.parse::<u16>()?)
    };
    let result = tokio::select! {
        r = timeout(crate::relay::IO, send) => r.map_err(Into::into).and_then(|v| v),
        () = stop.cancelled() => Err(anyhow!("push cancelled")),
    };
    if result.is_err() {
        let _ = child.start_kill();
        let _ = timeout(Duration::from_secs(2), child.wait()).await;
    }
    result
}
