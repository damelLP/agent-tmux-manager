use anyhow::{bail, Context, Result};
use atm_tmux::{RealTmuxClient, TmuxClient};
#[cfg(test)]
use atmd::control::workspace::run_shell_command_from_arg;
use atmd::control::workspace::{
    self, exact_session, run_shell_arg, workspace_script_path, WorkspaceHooks,
};

use super::spawn::{build_spawn_command_for_harness, resolve_spawn_harness, tmux_shell_prefix};
#[cfg(test)]
use super::TEST_ENV_LOCK;

/// [`run_shell_arg`] as one tmux command string, for `set-hook` and
/// `bind-key`, which parse it: inside tmux's double quotes `'` is literal,
/// `\`, `"` and `$` (an environment variable) need a `\`, and a newline is
/// written `\n` because tmux drops the spaces after a raw one.
#[cfg(test)]
fn run_shell_command(path: &std::path::Path, args: &[&str]) -> String {
    run_shell_command_from_arg(&run_shell_arg(path, args))
}

// ============================================================================
// Workspace Helpers
// ============================================================================

/// Resolves the tmux socket label used by workspace helpers.
fn effective_tmux_socket(socket: &Option<String>) -> Option<String> {
    socket.clone().or_else(|| {
        std::env::var(atm_tmux::client::TMUX_SOCKET_ENV)
            .ok()
            .filter(|s| !s.is_empty())
    })
}

/// Builds a tmux client for workspace orchestration.
fn workspace_tmux_client(socket: &Option<String>) -> RealTmuxClient {
    effective_tmux_socket(socket)
        .map(RealTmuxClient::with_socket)
        .unwrap_or_default()
}

// Preserve the CLI helpers' anyhow error-chain formatting at this boundary.
async fn tmux_run<C: TmuxClient + ?Sized>(client: &C, args: &[&str]) -> Result<String> {
    Ok(workspace::tmux_run(client, args).await?)
}

async fn has_session<C: TmuxClient + ?Sized>(client: &C, session: &str) -> Result<bool> {
    Ok(workspace::has_session(client, session).await?)
}

async fn inject_sidebar<C: TmuxClient + ?Sized>(
    client: &C,
    target_pane: &str,
    session_name: &str,
    cols: u16,
) -> Result<String> {
    Ok(workspace::inject_sidebar(client, target_pane, session_name, cols).await?)
}

/// Validate a session name (alphanumeric, -, _, . only).
fn validate_session_name(name: &str) -> Result<()> {
    if name.contains(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.') {
        bail!(
            "Session name '{}' contains unsupported characters (use alphanumeric, -, _, .)",
            name
        );
    }
    Ok(())
}

/// Default session name from current directory basename.
fn default_session_name() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "atm-workspace".to_string())
}

/// Resolve environment-dependent hook inputs at the CLI boundary.
fn workspace_hooks(socket: &Option<String>) -> WorkspaceHooks {
    let socket = effective_tmux_socket(socket);
    WorkspaceHooks {
        tmux_prefix: tmux_shell_prefix(socket.clone()),
        socket,
        script_dir: dirs::data_local_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join("atm"),
    }
}

/// Keep attach's hook-install entry points while sharing their explicit-input effects.
async fn install_resize_hooks<C>(
    client: &C,
    socket: &Option<String>,
    session_name: &str,
) -> Result<()>
where
    C: TmuxClient + ?Sized,
{
    workspace::install_resize_hooks(client, session_name, &workspace_hooks(socket)).await?;
    Ok(())
}

async fn install_new_window_hook<C>(
    client: &C,
    socket: &Option<String>,
    session_name: &str,
) -> Result<()>
where
    C: TmuxClient + ?Sized,
{
    workspace::install_new_window_hook(client, session_name, &workspace_hooks(socket)).await?;
    Ok(())
}

/// Exec into `tmux attach-session` (replaces the current process on Unix).
///
/// Honors `ATM_NO_ATTACH=1`: skips the attach and returns Ok(()), which
/// lets integration tests drive the workspace setup steps without
/// blocking on a real terminal. Undocumented user-facing because the
/// only sane reason to skip the attach is testing.
fn exec_attach(socket: &Option<String>, session_name: &str) -> Result<()> {
    if std::env::var("ATM_NO_ATTACH").is_ok_and(|v| v == "1") {
        return Ok(());
    }

    let mut cmd = std::process::Command::new("tmux");
    if let Some(s) = effective_tmux_socket(socket) {
        cmd.arg("-L").arg(s);
    }
    cmd.arg("attach-session")
        .arg("-t")
        .arg(exact_session(session_name));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        bail!("Failed to attach to tmux session: {err}");
    }

    #[cfg(not(unix))]
    {
        let status = cmd.status().context("Failed to attach to tmux session")?;
        if !status.success() {
            bail!("tmux attach failed with status {status}");
        }
        Ok(())
    }
}

// ============================================================================
// Workspace Create
// ============================================================================

pub(super) async fn cmd_workspace(name: Option<String>, isolate: bool, editor: bool) -> Result<()> {
    // 0. Resolve the agent command before any tmux call, so a bad harness
    //    config fails with nothing created
    let agent_cmd = build_spawn_command_for_harness(&resolve_spawn_harness(None)?, None, None);

    // 1. Determine session name, sanitized to safe characters
    let session_name = name.unwrap_or_else(default_session_name);
    validate_session_name(&session_name)?;

    // 2. Build tmux base command args (handles --isolate socket)
    let socket_name = if isolate {
        Some(format!("atm-{session_name}"))
    } else {
        None
    };
    let client = workspace_tmux_client(&socket_name);

    // 3. Check if session already exists
    if has_session(&client, &session_name).await? {
        if isolate {
            bail!(
                "Workspace '{}' already exists on socket 'atm-{}'. \
                 Attach with: atm workspace attach {} --isolate",
                session_name,
                session_name,
                session_name
            );
        } else {
            bail!(
                "Workspace '{}' already exists. \
                 Attach with: atm workspace attach {}",
                session_name,
                session_name
            );
        }
    }

    // 4–10. Create at the caller's terminal size, preserving the split/launch/
    // hook/focus sequence. Attaching stays local to the CLI.
    let terminal_size = crossterm::terminal::size().unwrap_or((80, 24));
    workspace::create_workspace(
        &client,
        &session_name,
        &agent_cmd,
        editor,
        terminal_size,
        &workspace_hooks(&socket_name),
    )
    .await?;
    exec_attach(&socket_name, &session_name)
}

// ============================================================================
// Workspace Attach
// ============================================================================

pub(super) async fn cmd_workspace_attach(session: Option<String>, isolate: bool) -> Result<()> {
    // 1. Resolve target session
    let (session_name, socket_name) = if let Some(name) = session {
        validate_session_name(&name)?;
        let socket = if isolate {
            Some(format!("atm-{name}"))
        } else {
            None
        };
        (name, socket)
    } else {
        // Auto-pick most recently attached session
        let socket = if isolate {
            // For isolate without a name, we can't know which socket to use
            bail!(
                "Session name is required with --isolate. \
                 Usage: atm workspace attach <session> --isolate"
            );
        } else {
            None
        };
        let output = tmux_run(
            &workspace_tmux_client(&socket),
            &[
                "list-sessions",
                "-F",
                "#{session_last_attached} #{session_name}",
            ],
        )
        .await?;
        let mut sessions: Vec<_> = output
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(2, ' ');
                let ts: u64 = parts.next()?.parse().ok()?;
                let name = parts.next()?.to_string();
                Some((ts, name))
            })
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.0)); // most recent first
        let session_name = sessions
            .into_iter()
            .next()
            .map(|(_, name)| name)
            .ok_or_else(|| anyhow::anyhow!("No tmux sessions found"))?;
        validate_session_name(&session_name).context(
            "Can't attach to the most recently attached tmux session; \
             rename it with `tmux rename-session`",
        )?;
        (session_name, socket)
    };
    let client = workspace_tmux_client(&socket_name);

    // 2. Verify session exists
    if !has_session(&client, &session_name).await? {
        if let Some(ref s) = socket_name {
            bail!("Session '{}' not found on socket '{}'", session_name, s);
        } else {
            bail!("Session '{}' not found", session_name);
        }
    }

    // 3. List all windows and their panes, inject sidebar where needed
    let windows_output = tmux_run(
        &client,
        &[
            "list-windows",
            "-t",
            &exact_session(&session_name),
            "-F",
            "#{window_id} #{window_width}",
        ],
    )
    .await?;

    for line in windows_output.lines() {
        let mut parts = line.splitn(2, ' ');
        let window_id = parts.next().unwrap_or_default();
        let cols: u16 = parts.next().and_then(|w| w.parse().ok()).unwrap_or(80);
        // Check if this window already has an atm-sidebar pane (using @atm-sidebar option,
        // which survives ATM crashes unlike pane titles that the shell can overwrite)
        let panes_output = tmux_run(
            &client,
            &[
                "list-panes",
                "-t",
                window_id,
                "-F",
                "#{pane_id}:#{@atm-sidebar}",
            ],
        )
        .await?;

        let has_sidebar = panes_output.lines().any(|line| line.ends_with(":1"));
        if has_sidebar {
            continue;
        }

        // Pick the first pane in the window as the split target
        let first_pane = panes_output
            .lines()
            .next()
            .and_then(|line| line.split(':').next())
            .ok_or_else(|| anyhow::anyhow!("Window {} has no panes", window_id))?;

        inject_sidebar(&client, first_pane, &session_name, cols)
            .await
            .with_context(|| {
                format!(
                    "Failed to inject sidebar into window {window_id}; \
                     re-run `atm workspace attach` to complete injection"
                )
            })?;
    }

    // 5. Install hooks for resize and new windows
    install_resize_hooks(&client, &socket_name, &session_name).await?;
    install_new_window_hook(&client, &socket_name, &session_name).await?;

    // 6. Run the resize script once to correct sidebar widths before attaching
    let script_path = workspace_script_path(
        &dirs::data_local_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join("atm"),
        effective_tmux_socket(&socket_name).as_deref(),
        &session_name,
        "resize-sidebar",
    );
    for window in windows_output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
    {
        let _ = tmux_run(
            &client,
            &["run-shell", &run_shell_arg(&script_path, &[window])],
        )
        .await;
    }

    // 7. Attach
    exec_attach(&socket_name, &session_name)
}

#[cfg(test)]
mod shell_quote_tests {
    use super::{run_shell_arg, run_shell_command};
    use std::path::Path;

    #[test]
    fn script_paths_keep_socket_labels_distinct_and_lossless() {
        let base = Path::new("data/atm");
        for (socket, namespace) in [
            (None, "implicit"),
            (Some("implicit"), "label-696d706c69636974"),
            (Some(""), "label-"),
            (Some("a/b"), "label-612f62"),
            (Some("a_b"), "label-615f62"),
            (Some("a\\b"), "label-615c62"),
            (Some(".."), "label-2e2e"),
            (Some("#'\"/"), "label-2327222f"),
            (Some("é"), "label-c3a9"),
        ] {
            let want = base.join(namespace).join("resize-sidebar-same.sh");
            for _ in 0..2 {
                assert_eq!(
                    super::workspace_script_path(base, socket, "same", "resize-sidebar"),
                    want
                );
            }
            assert_eq!(
                super::workspace_script_path(base, socket, "same", "inject-sidebar"),
                base.join(namespace).join("inject-sidebar-same.sh")
            );
        }
    }

    #[test]
    fn run_shell_arg_quotes_each_word_and_doubles_hash() {
        assert_eq!(
            run_shell_arg(Path::new("/data/atm/r.sh"), &[]),
            "'/data/atm/r.sh'"
        );
        // macOS's data dir has a space; run-shell would expand `#S`.
        assert_eq!(
            run_shell_arg(Path::new("/Application Support/it's #S/r.sh"), &["hook"]),
            "'/Application Support/it'\\''s ##S/r.sh' 'hook'"
        );
    }

    #[test]
    fn run_shell_command_escapes_for_tmux_double_quotes() {
        assert_eq!(
            run_shell_command(Path::new("/data dir/r.sh"), &[]),
            "run-shell \"'/data dir/r.sh'\""
        );
        assert_eq!(
            run_shell_command(Path::new("/a\"b $HOME it's #S\\c/r.sh"), &["hook"]),
            r#"run-shell "'/a\"b \$HOME it'\\''s ##S\\c/r.sh' 'hook'""#
        );
        // Inside double quotes tmux drops the spaces after a raw newline.
        assert_eq!(
            run_shell_command(Path::new("/a\n  b/r.sh"), &[]),
            r#"run-shell "'/a\n  b/r.sh'""#
        );
    }
}

#[cfg(test)]
mod has_session_tests {
    use super::has_session;
    use atm_tmux::mock::MockCall;
    use atm_tmux::{MockTmuxClient, TmuxError};
    use std::time::Duration;

    /// A bare `-t ws` prefix-matches `ws2` (tmux 3.8), so `workspace
    /// create ws` would refuse and `workspace attach ws` would pick `ws2`.
    #[tokio::test]
    async fn test_has_session_matches_the_name_exactly() {
        let client = MockTmuxClient::new();
        has_session(&client, "ws").await.expect("has-session");
        assert_eq!(
            client.calls(),
            vec![MockCall::RawCommand {
                subcommand: "has-session".to_string(),
                args: vec!["-t".to_string(), "=ws:".to_string()],
            }]
        );
    }

    /// `set-hook -t` takes a pane, so a bare `api` is first looked up as a
    /// window in the caller's own session (tmux 3.8): run from a session
    /// with a window named `api`, the hooks would land there.
    #[test]
    fn test_workspace_hooks_target_the_session_exactly() {
        let _guard = super::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The installers write their scripts under XDG_DATA_HOME
        let data_home = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", data_home.path());

        let client = MockTmuxClient::new();
        let socket = Some("atm-my.proj".to_string());
        let installed = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                super::install_resize_hooks(&client, &socket, "my.proj").await?;
                super::install_new_window_hook(&client, &socket, "my.proj").await
            });
        match prev {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        installed.expect("install hooks");

        let targets: Vec<String> = client
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                MockCall::RawCommand { subcommand, args } if subcommand == "set-hook" => {
                    args.get(1).cloned()
                }
                _ => None,
            })
            .collect();
        assert_eq!(targets, vec!["=my.proj:"; 3]);

        // Each hook and prefix-R quote their script path; the e2e test proves
        // the quoting with real tmux but skips where tmux is missing.
        let commands: Vec<String> = client
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                MockCall::RawCommand { subcommand, args }
                    if subcommand == "set-hook"
                        || (subcommand == "bind-key"
                            && args.get(2).is_some_and(|key| key == "R")) =>
                {
                    args.get(3).cloned()
                }
                _ => None,
            })
            .collect();
        let script = |name: &str| {
            data_home
                .path()
                .join("atm/label-61746d2d6d792e70726f6a")
                .join(name)
        };
        let resize = script("resize-sidebar-my.proj.sh");
        assert_eq!(
            commands,
            vec![
                super::run_shell_command_from_arg(&format!(
                    "{} '#{{window_id}}'",
                    super::run_shell_arg(&resize, &[])
                )),
                super::run_shell_command_from_arg(&format!(
                    "{} '#{{window_id}}' hook",
                    super::run_shell_arg(&resize, &[])
                )),
                super::run_shell_command_from_arg(&format!(
                    "{} '#{{window_id}}'",
                    super::run_shell_arg(&resize, &[])
                )),
                super::run_shell_command_from_arg(&format!(
                    "{} '#{{window_id}}'",
                    super::run_shell_arg(&script("inject-sidebar-my.proj.sh"), &[])
                )),
            ]
        );
    }

    /// tmux's own "no" (`can't find session`, or no server yet) means the
    /// session is absent. A timeout or a missing tmux means tmux didn't
    /// answer, so the workspace commands must not report "not found" or
    /// create a duplicate.
    #[tokio::test]
    async fn test_has_session_keeps_a_timeout_an_error() {
        let client = MockTmuxClient::new();
        assert!(has_session(&client, "ws").await.expect("exists"));

        client.set_next_error(TmuxError::CommandFailed {
            command: "has-session".to_string(),
            stderr: "can't find session: ws".to_string(),
        });
        assert!(!has_session(&client, "ws").await.expect("absent"));

        client.set_next_error(TmuxError::Timeout {
            command: "has-session".to_string(),
            timeout: Duration::from_secs(5),
        });
        let result = has_session(&client, "ws").await;
        assert!(
            matches!(&result, Err(e) if format!("{e:#}").contains("timed out")),
            "a timeout read as {result:?}"
        );

        // Nor is failing to run tmux at all.
        client.set_next_error(TmuxError::NotFound);
        let result = has_session(&client, "ws").await;
        assert!(result.is_err(), "a missing tmux read as {result:?}");
    }
}
