//! Parent ownership of a child rig's files and processes, including forced exit.
use super::{ownership, process, signal_if_unchanged};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const GUARD: Duration = Duration::from_secs(30);

fn start_identity(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.into())
}

fn line(stream: &UnixStream) -> std::io::Result<String> {
    let mut text = String::new();
    BufReader::new(stream).read_line(&mut text)?;
    Ok(text)
}

fn bounded(stream: &UnixStream) {
    stream.set_read_timeout(Some(GUARD)).unwrap();
    stream.set_write_timeout(Some(GUARD)).unwrap();
}

/// Called before event listeners, tmux or daemons start. Parent acknowledgment
/// transfers directory deletion to the guardian; child Drop still stops resources.
pub fn register(root: &Path) -> Option<UnixStream> {
    let endpoint = std::env::var_os("STANDIN_PARENT_SOCKET")?;
    let mut stream = UnixStream::connect(endpoint).expect("private parent guardian");
    bounded(&stream);
    let socket = root
        .join("tmux")
        .canonicalize()
        .unwrap()
        .join(format!("tmux-{}", unsafe { libc::getuid() }))
        .join("stand-in");
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "root": root, "socket": socket, "pid": std::process::id(),
            "start": start_identity(std::process::id()).unwrap(),
        })
    )
    .unwrap();
    assert_eq!(line(&stream).unwrap(), "owned\n");
    Some(stream)
}

pub fn ready(stream: &UnixStream) {
    let mut writer = stream;
    writer.write_all(b"ready\n").unwrap();
    assert_eq!(line(stream).unwrap(), "finish\n");
}

pub struct ChildGuard {
    dir: Option<tempfile::TempDir>,
    real: PathBuf,
    listener: UnixListener,
    child: Option<Child>,
    stream: Option<UnixStream>,
    root: Option<PathBuf>,
    socket: Option<PathBuf>,
    log: PathBuf,
}

impl ChildGuard {
    pub fn spawn(test: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let alias = dir.path().join("alias");
        let endpoint = dir.path().join("parent.sock");
        let log = dir.path().join("child.log");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let listener = UnixListener::bind(&endpoint).unwrap();
        listener.set_nonblocking(true).unwrap();
        // Own the base and listener before spawning a process that can create resources.
        let mut guard = Self {
            dir: Some(dir),
            real,
            listener,
            child: None,
            stream: None,
            root: None,
            socket: None,
            log,
        };
        let output = std::fs::File::create(&guard.log).unwrap();
        guard.child = Some(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env_remove("TMUX")
                .env_remove("TMUX_PANE")
                .env("STANDIN_SYMLINK_CHILD", "1")
                .env("STANDIN_PARENT_SOCKET", endpoint)
                .env("TMPDIR", alias)
                .stdin(Stdio::null())
                .stdout(output.try_clone().unwrap())
                .stderr(output)
                .spawn()
                .unwrap(),
        );
        guard
    }

    pub fn wait_ready(&mut self) {
        let mut fd = libc::pollfd {
            fd: self.listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(
            unsafe { libc::poll(&mut fd, 1, 30_000) } > 0,
            "child registration: {}",
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
        let (mut stream, _) = self.listener.accept().unwrap();
        bounded(&stream);
        let registration: serde_json::Value =
            serde_json::from_str(&line(&stream).unwrap()).unwrap();
        let child = self.child.as_ref().unwrap();
        assert_eq!(registration["pid"], child.id());
        assert_eq!(
            registration["start"].as_str(),
            start_identity(child.id()).as_deref()
        );
        let root = PathBuf::from(registration["root"].as_str().unwrap());
        let canonical = root.canonicalize().unwrap();
        let base = self.real.canonicalize().unwrap();
        assert!(root.is_absolute() && canonical.starts_with(&base) && canonical != base);
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("u11-"));
        let socket = root
            .join("tmux")
            .canonicalize()
            .unwrap()
            .join(format!("tmux-{}", unsafe { libc::getuid() }))
            .join("stand-in");
        assert_eq!(registration["socket"].as_str(), socket.to_str());
        self.root = Some(root);
        self.socket = Some(socket);
        // The parent records ownership before allowing any dependent resources.
        stream.write_all(b"owned\n").unwrap();
        self.stream = Some(stream);
        assert_eq!(line(self.stream.as_ref().unwrap()).unwrap(), "ready\n");
    }

    pub fn child_root(&self) -> &Path {
        self.root.as_deref().expect("registered child")
    }
    pub fn real(&self) -> &Path {
        &self.real
    }

    pub fn finish(&mut self, forced_exit: bool) {
        if forced_exit {
            self.child.as_mut().unwrap().kill().unwrap(); // Unreaped child cannot have a reused PID.
        } else {
            self.stream
                .as_mut()
                .unwrap()
                .write_all(b"finish\n")
                .unwrap();
        }
        let child = self.child.as_mut().unwrap();
        process::wait_exit(child.id(), GUARD).unwrap();
        let status = child.wait().unwrap();
        self.child = None;
        // Both normal return and forced exit retain files until the parent cleans up.
        assert!(self.child_root().exists());
        let cleanup = self.cleanup_resources();
        assert!(
            forced_exit || status.success(),
            "child failed: {}",
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
        cleanup.expect("parent-owned cleanup");
    }

    fn pids(
        &self,
        identity: fn(&Path, u32) -> Option<String>,
    ) -> Result<Vec<(u32, String)>, String> {
        let Some(root) = &self.root else {
            return Ok(Vec::new());
        };
        Ok(std::fs::read_dir("/proc")
            .map_err(|e| e.to_string())?
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_str()?.parse().ok()?;
                Some((pid, identity(root, pid)?))
            })
            .collect())
    }

    fn stop_daemons(&self) -> Result<(), String> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        for (pid, start) in self.pids(ownership::daemon_identity)? {
            for signal in [libc::SIGTERM, libc::SIGKILL] {
                signal_if_unchanged(
                    &start,
                    ownership::daemon_identity(root, pid).as_deref(),
                    || {
                        if unsafe { libc::kill(pid as i32, signal) } == 0 {
                            return Ok(());
                        }
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() == Some(libc::ESRCH) {
                            Ok(())
                        } else {
                            Err(error.to_string())
                        }
                    },
                )?;
                match process::wait_exit(
                    pid,
                    if signal == libc::SIGTERM {
                        Duration::from_secs(3)
                    } else {
                        GUARD
                    },
                ) {
                    Ok(()) => break,
                    Err(error)
                        if signal == libc::SIGTERM
                            && error.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        if self.pids(ownership::daemon_identity)?.is_empty() {
            Ok(())
        } else {
            Err("owned daemon remains".into())
        }
    }

    fn tmux(&self, args: &[&str]) -> std::process::Output {
        let root = self.child_root();
        process::run(
            Command::new("tmux")
                .env_clear()
                .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                .env("ATM_SOCKET", root.join("atm.sock"))
                .env("ATM_TMUX_SOCKET", "stand-in")
                .env("TMUX_TMPDIR", root.join("tmux"))
                .env("ATM_STATE_DIR", root.join("state"))
                .env("TMPDIR", root.join("tmp"))
                .env("HOME", root.join("home"))
                .args(["-S", self.socket.as_ref().unwrap().to_str().unwrap()])
                .args(args),
        )
    }

    fn stop_tmux(&self) -> Result<(), String> {
        if self.root.is_none() {
            return Ok(());
        }
        let output = self.tmux(&["display-message", "-p", "#{socket_path} #{pid}"]);
        if !output.status.success() {
            return if self.pids(ownership::tmux_identity)?.is_empty() {
                Ok(())
            } else {
                Err("owned server alive but socket unavailable; retaining directory".into())
            };
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let (path, pid) = text
            .trim()
            .rsplit_once(' ')
            .ok_or("invalid server identity")?;
        if Some(Path::new(path)) != self.socket.as_deref() {
            return Err("foreign socket".into());
        }
        let pid = pid.parse().map_err(|_| "invalid server PID")?;
        let start = ownership::tmux_identity(self.child_root(), pid).ok_or("foreign server")?;
        let recheck = self.tmux(&["display-message", "-p", "#{socket_path} #{pid}"]);
        if !recheck.status.success()
            || recheck.stdout != output.stdout
            || ownership::tmux_identity(self.child_root(), pid).as_ref() != Some(&start)
        {
            return Err("server identity changed".into());
        }
        let result = self.tmux(&["kill-server"]);
        if !result.status.success() {
            return Err(format!("private server cleanup: {result:?}"));
        }
        process::wait_exit(pid, GUARD).map_err(|e| e.to_string())
    }

    fn cleanup_resources(&mut self) -> Result<(), String> {
        let daemon = self.stop_daemons();
        let tmux = self.stop_tmux();
        daemon.and(tmux)?;
        if let Some(root) = self.root.take() {
            // This exact child-created directory was validated inside our base
            // before acknowledgment; it stayed present across forced child exit.
            std::fs::remove_dir_all(root).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            if process::wait_exit(child.id(), GUARD).is_err() {
                if let Some(dir) = self.dir.take() {
                    eprintln!("child did not exit; retained {}", dir.keep().display());
                }
                return;
            }
            let _ = child.wait();
        }
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.cleanup_resources()));
        if !matches!(result, Ok(Ok(()))) {
            // Never delete a socket directory on a failed ownership/cleanup check.
            if let Some(dir) = self.dir.take() {
                eprintln!("guardian cleanup failed; retained {}", dir.keep().display());
            }
        }
    }
}
