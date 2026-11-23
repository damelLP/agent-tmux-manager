use std::{
    env, fs,
    future::Future,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tempfile::{tempdir, TempDir};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines},
    net::{TcpStream, UnixListener, UnixStream},
    process::{Child, ChildStdout, Command},
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, handshake::client::Response, Error},
    MaybeTlsStream, WebSocketStream,
};

pub const OWNER: &str = "owner@example.invalid";
pub const ORIGIN: &str = "https://chat.example.invalid:8443";
pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub async fn guard<T>(test: impl Future<Output = T>) -> T {
    timeout(Duration::from_secs(60), test)
        .await
        .expect("test hung")
}

pub struct Rig(TempDir);

impl Rig {
    pub fn new() -> Self {
        let dir = tempdir().unwrap();
        for name in ["config/atm", "home", "tmp", "tmux", "state", "xdg"] {
            fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        fs::write(
            dir.path().join("config/atm/config.toml"),
            "[delivery]\nstable_idle = { secs = 1, nanos = 0 }\n",
        )
        .unwrap();
        Self(dir)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }

    pub fn child(&self, binary: impl AsRef<Path>) -> Command {
        let mut cmd = Command::new(binary.as_ref());
        cmd.env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .env("NO_COLOR", "1")
            .env("ATM_SOCKET", self.path("daemon.sock"))
            .env("ATM_STATE_DIR", self.path("state"))
            .env("ATM_TMUX_SOCKET", "chatw1-test")
            .env("HOME", self.path("home"))
            .env("TMPDIR", self.path("tmp"))
            .env("TMUX_TMPDIR", self.path("tmux"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("xdg"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        cmd
    }

    pub async fn daemon(&self) -> Child {
        let bin = PathBuf::from(
            env::var_os("ATM_CHAT_TEST_ATMD").expect("build sealed atmd with chat-app/check.sh"),
        );
        assert!(bin.is_absolute() && bin.is_file());
        let mut child = self.child(bin).arg("start").spawn().unwrap();
        let mut stdout = lines(&mut child);
        until(&mut stdout, |line| {
            line.contains("Daemon server listening").then_some(())
        })
        .await;
        drain(stdout);
        child
    }

    pub async fn bridge(&self, socket: &Path) -> Bridge {
        self.bridge_with(socket, &[]).await
    }

    pub async fn bridge_with(&self, socket: &Path, extra: &[&Path]) -> Bridge {
        let mut child = self
            .child(env!("CARGO_BIN_EXE_atm-chat-bridge"))
            .args([
                "--port",
                "0",
                "--owner-login",
                OWNER,
                "--public-origin",
                ORIGIN,
                "--socket",
            ])
            .arg(socket)
            .args(extra)
            .spawn()
            .unwrap();
        let mut stdout = lines(&mut child);
        let addr = until(&mut stdout, |line| {
            line.strip_prefix("Local: http://")
                .and_then(|url| url.strip_suffix('/'))
                .map(str::to_owned)
        })
        .await;
        drain(stdout);
        Bridge { child, addr }
    }
}

fn lines(child: &mut Child) -> Lines<BufReader<ChildStdout>> {
    BufReader::new(child.stdout.take().unwrap()).lines()
}

async fn until<T>(
    lines: &mut Lines<BufReader<ChildStdout>>,
    event: impl Fn(&str) -> Option<T>,
) -> T {
    while let Some(line) = lines.next_line().await.unwrap() {
        if let Some(value) = event(&line) {
            return value;
        }
    }
    panic!("child exited before its readiness event");
}

fn drain(mut lines: Lines<BufReader<ChildStdout>>) {
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
}

pub struct Bridge {
    child: Child,
    addr: String,
}

impl Bridge {
    pub async fn ws(&self, logins: &[&str], origin: Option<&str>) -> Result<(Ws, Response), Error> {
        let mut req = format!("ws://{}/ws", self.addr)
            .into_client_request()
            .unwrap();
        for login in logins {
            req.headers_mut()
                .append("tailscale-user-login", login.parse().unwrap());
        }
        if let Some(origin) = origin {
            req.headers_mut().insert("origin", origin.parse().unwrap());
        }
        connect_async(req).await
    }

    pub async fn http(&self, method: &str, path: &str, logins: &[&str]) -> (String, String) {
        use tokio::io::AsyncReadExt;
        let mut stream = TcpStream::connect(&self.addr).await.unwrap();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.addr
        );
        for login in logins {
            request.push_str(&format!("Tailscale-User-Login: {login}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        (headers.to_owned(), body.to_owned())
    }

    pub async fn file_http(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        chunked: bool,
    ) -> (String, Vec<u8>) {
        use tokio::io::AsyncReadExt;
        let (mut rd, mut wr) = TcpStream::connect(&self.addr).await.unwrap().into_split();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.addr
        );
        for (key, value) in headers {
            head.push_str(&format!("{key}: {value}\r\n"));
        }
        if chunked {
            head.push_str("Transfer-Encoding: chunked\r\n\r\n");
        } else {
            head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        }
        wr.write_all(head.as_bytes()).await.unwrap();
        let write = async {
            if chunked {
                for bytes in body.chunks(64 * 1024) {
                    wr.write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
                        .await?;
                    wr.write_all(bytes).await?;
                    wr.write_all(b"\r\n").await?;
                }
                wr.write_all(b"0\r\n\r\n").await
            } else {
                wr.write_all(body).await
            }
        };
        let mut response = Vec::new();
        let (written, read) = tokio::join!(write, rd.read_to_end(&mut response));
        if let Err(error) = written {
            use std::io::ErrorKind::{BrokenPipe, ConnectionReset};
            assert!(
                matches!(error.kind(), BrokenPipe | ConnectionReset),
                "{error}"
            );
        }
        read.unwrap();
        let end = response
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
        (
            String::from_utf8(response[..end].to_vec()).unwrap(),
            response[(end + 4)..].to_vec(),
        )
    }

    pub async fn stop(mut self) {
        self.child.kill().await.unwrap();
    }
}

pub struct Tap {
    pub upstream: UnboundedReceiver<Vec<u8>>,
    pub downstream: UnboundedReceiver<Vec<u8>>,
    join: JoinHandle<std::io::Result<()>>,
}

impl Tap {
    pub fn start(rig: &Rig) -> Self {
        let listener = UnixListener::bind(rig.path("tap.sock")).unwrap();
        let daemon = rig.path("daemon.sock");
        let (up_tx, upstream) = unbounded_channel();
        let (down_tx, downstream) = unbounded_channel();
        let join = tokio::spawn(async move {
            let (bridge, _) = listener.accept().await?;
            let daemon = UnixStream::connect(daemon).await?;
            let (bridge_read, bridge_write) = bridge.into_split();
            let (daemon_read, daemon_write) = daemon.into_split();
            tokio::try_join!(
                copy_lines(bridge_read, daemon_write, up_tx),
                copy_lines(daemon_read, bridge_write, down_tx)
            )?;
            Ok(())
        });
        Self {
            upstream,
            downstream,
            join,
        }
    }

    pub async fn finish(self) {
        self.join.await.unwrap().unwrap();
    }
}

async fn copy_lines(
    read: impl AsyncRead + Unpin,
    mut write: impl AsyncWrite + Unpin,
    seen: UnboundedSender<Vec<u8>>,
) -> std::io::Result<()> {
    let mut read = BufReader::new(read);
    let mut line = Vec::new();
    while read.read_until(b'\n', &mut line).await? != 0 {
        write.write_all(&line).await?;
        let _ = seen.send(std::mem::take(&mut line));
    }
    write.shutdown().await
}
