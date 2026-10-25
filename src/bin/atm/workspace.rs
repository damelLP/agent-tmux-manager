use anyhow::{bail, Context, Result};
use atm_tmux::{RealTmuxClient, TmuxClient, TmuxError};

#[cfg(test)]
use super::TEST_ENV_LOCK;
use super::{shell_quote, tmux_shell_prefix};

/// `run-shell`'s argument to run the script at `path` with `args`: each one
/// `sh -c` word, with `#` doubled because run-shell format-expands its
/// argument (tmux 3.8: `#S` becomes the session name).
pub(super) fn run_shell_arg(path: &std::path::Path, args: &[&str]) -> String {
    let mut cmd = shell_quote(&path.display().to_string());
    for arg in args {
        cmd.push(' ');
        cmd.push_str(&shell_quote(arg));
    }
    cmd.replace('#', "##")
}

/// [`run_shell_arg`] as one tmux command string, for `set-hook` and
/// `bind-key`, which parse it: inside tmux's double quotes `'` is literal,
/// `\`, `"` and `$` (an environment variable) need a `\`, and a newline is
/// written `\n` because tmux drops the spaces after a raw one.
pub(super) fn run_shell_command(path: &std::path::Path, args: &[&str]) -> String {
    let arg = run_shell_arg(path, args)
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('\n', "\\n");
    format!("run-shell \"{arg}\"")
}

// ============================================================================
// Workspace Helpers
// ============================================================================

const SIDEBAR_PCT: u32 = 16;
const SIDEBAR_MIN: u32 = 20;
const SIDEBAR_MAX: u32 = 40;

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

/// Run a tmux command through the shared client abstraction, returning trimmed stdout.
async fn tmux_run<C>(client: &C, args: &[&str]) -> Result<String>
where
    C: TmuxClient + ?Sized,
{
    let Some((subcommand, rest)) = args.split_first() else {
        bail!("tmux command missing subcommand");
    };
    let output = client
        .raw_command(subcommand, rest)
        .await
        .with_context(|| format!("tmux {:?} failed", args))?;
    Ok(output.trim().to_string())
}

/// The `-t` target for exactly the session `name` (tmux 3.8). Without `=`
/// tmux falls back to a prefix, so `ws` finds `ws2`. The trailing `:` makes
/// it a session where `-t` takes a pane (`set-hook`), which would first try
/// a bare name as a window of the caller's session, and ends the name
/// before tmux reads a `.` in it as the window.pane separator.
fn exact_session(name: &str) -> String {
    format!("={name}:")
}

/// Whether tmux has `session`. Only tmux's own "no" (a failed command)
/// means absent. A timeout, or tmux failing to run, stays an error: tmux
/// didn't answer, so the session may well exist.
async fn has_session<C>(client: &C, session: &str) -> Result<bool>
where
    C: TmuxClient + ?Sized,
{
    match tmux_run(client, &["has-session", "-t", &exact_session(session)]).await {
        Ok(_) => Ok(true),
        Err(e) if matches!(e.downcast_ref(), Some(TmuxError::CommandFailed { .. })) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Calculate sidebar width from terminal columns, clamped to [SIDEBAR_MIN, SIDEBAR_MAX].
fn sidebar_width(cols: u16) -> u32 {
    (cols as u32 * SIDEBAR_PCT / 100).clamp(SIDEBAR_MIN, SIDEBAR_MAX)
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

/// Write the resize-sidebar script and install the after-resize-window hook + prefix keybindings.
async fn install_resize_hooks<C>(
    client: &C,
    socket: &Option<String>,
    session_name: &str,
) -> Result<()>
where
    C: TmuxClient + ?Sized,
{
    let tmux_prefix = tmux_shell_prefix(effective_tmux_socket(socket));
    let script_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("atm");
    std::fs::create_dir_all(&script_dir)?;
    let script_path = script_dir.join(format!("resize-sidebar-{session_name}.sh"));
    std::fs::write(
        &script_path,
        format!(
            "#!/bin/sh\n\
             W=$({tmux} display-message -p '#{{window_width}}')\n\
             S=$((W * {pct} / 100))\n\
             [ $S -lt {min} ] && S={min}\n\
             [ $S -gt {max} ] && S={max}\n\
             for p in $({tmux} list-panes -F '#{{pane_id}}:#{{@atm-sidebar}}:#{{pane_width}}:#{{@atm-sidebar-tries}}'); do\n\
               id=${{p%%:*}}; n=${{p##*:}}\n\
               case $p in\n\
                 *:1:$S:*) [ -n \"$n\" ] && {tmux} set-option -pu -t \"$id\" @atm-sidebar-tries;;\n\
                 *:1:*) if [ \"$1\" = hook ]; then\n\
                     [ \"${{n:-0}}\" -ge 5 ] && continue\n\
                     {tmux} set-option -p -t \"$id\" @atm-sidebar-tries $((${{n:-0}} + 1))\n\
                   fi\n\
                   {tmux} resize-pane -t \"$id\" -x \"$S\";;\n\
               esac\n\
             done\n",
            tmux = tmux_prefix,
            pct = SIDEBAR_PCT,
            min = SIDEBAR_MIN,
            max = SIDEBAR_MAX,
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
    }
    let hook_cmd = run_shell_command(&script_path, &[]);
    tmux_run(
        client,
        &[
            "set-hook",
            "-t",
            &exact_session(session_name),
            "after-resize-window",
            &hook_cmd,
        ],
    )
    .await?;
    // Undo Claude Code teammate rebalances, which resize the first pane (our
    // sidebar) to 30%. The script's own resize-pane re-fires this hook, so in
    // `hook` mode it stops after 5 tries that don't reach the target width
    // (e.g. a window too small to fit it); reaching the target resets the count.
    let pane_hook_cmd = run_shell_command(&script_path, &["hook"]);
    tmux_run(
        client,
        &[
            "set-hook",
            "-t",
            &exact_session(session_name),
            "after-resize-pane",
            &pane_hook_cmd,
        ],
    )
    .await?;
    // NOTE: bind-key is global; the last workspace created wins for prefix-R.
    tmux_run(client, &["bind-key", "-T", "prefix", "R", &hook_cmd]).await?;
    // Focus the window's `@atm-sidebar` pane; pane indexes shift with layout changes.
    tmux_run(
        client,
        &[
            "bind-key",
            "-T",
            "prefix",
            "a",
            "run-shell",
            "-C",
            "select-pane -t '#{P:#{?#{@atm-sidebar},#{pane_id},}}'",
        ],
    )
    .await?;
    Ok(())
}

/// Inject an ATM sidebar into a target pane. Returns the sidebar pane ID.
async fn inject_sidebar<C>(
    client: &C,
    target_pane: &str,
    session_name: &str,
    cols: u16,
) -> Result<String>
where
    C: TmuxClient + ?Sized,
{
    let width = sidebar_width(cols).to_string();
    let atm_pane = tmux_run(
        client,
        &[
            "split-window",
            "-hb",
            "-t",
            target_pane,
            "-l",
            &width,
            "-P",
            "-F",
            "#{pane_id}",
        ],
    )
    .await?;
    // Tag with both a pane title (cosmetic) and a user option (for reliable detection).
    // Pane titles can be overwritten by the shell if ATM crashes; @atm-sidebar persists.
    tmux_run(
        client,
        &["select-pane", "-t", &atm_pane, "-T", "atm-sidebar"],
    )
    .await?;
    tmux_run(
        client,
        &["set-option", "-p", "-t", &atm_pane, "@atm-sidebar", "1"],
    )
    .await?;
    let atm_cmd = format!("atm --compact --tmux-session '{session_name}'");
    tmux_run(client, &["send-keys", "-t", &atm_pane, &atm_cmd, "Enter"]).await?;
    Ok(atm_pane)
}

/// Write the inject-sidebar script for the after-new-window hook.
async fn install_new_window_hook<C>(
    client: &C,
    socket: &Option<String>,
    session_name: &str,
) -> Result<()>
where
    C: TmuxClient + ?Sized,
{
    let tmux_prefix = tmux_shell_prefix(effective_tmux_socket(socket));
    let script_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("atm");
    std::fs::create_dir_all(&script_dir)?;
    let script_path = script_dir.join(format!("inject-sidebar-{session_name}.sh"));
    std::fs::write(
        &script_path,
        format!(
            "#!/bin/sh\n\
             W=$({tmux} display-message -p '#{{window_width}}')\n\
             S=$((W * {pct} / 100))\n\
             [ $S -lt {min} ] && S={min}\n\
             [ $S -gt {max} ] && S={max}\n\
             ORIG=$({tmux} display-message -p '#{{pane_id}}')\n\
             PANE=$({tmux} split-window -hb -l $S -P -F '#{{pane_id}}')\n\
             {tmux} select-pane -t \"$PANE\" -T atm-sidebar\n\
             {tmux} set-option -p -t \"$PANE\" @atm-sidebar 1\n\
             {tmux} send-keys -t \"$PANE\" \"atm --compact --tmux-session '{session}'\" Enter\n\
             {tmux} select-pane -t \"$ORIG\"\n",
            tmux = tmux_prefix,
            pct = SIDEBAR_PCT,
            min = SIDEBAR_MIN,
            max = SIDEBAR_MAX,
            session = session_name,
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
    }
    let hook_cmd = run_shell_command(&script_path, &[]);
    tmux_run(
        client,
        &[
            "set-hook",
            "-t",
            &exact_session(session_name),
            "after-new-window",
            &hook_cmd,
        ],
    )
    .await?;
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

    // 4. Create session at current terminal size so absolute pane sizes
    //    (like -l 30) are preserved when we attach later.
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let cols_str = cols.to_string();
    let rows_str = rows.to_string();
    let agent_pane = tmux_run(
        &client,
        &[
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-x",
            &cols_str,
            "-y",
            &rows_str,
            "-P",
            "-F",
            "#{pane_id}",
        ],
    )
    .await?;

    // 5. Inject ATM sidebar on the left
    inject_sidebar(&client, &agent_pane, &session_name, cols).await?;

    // 6. Split: shell below the agent pane (20% height)
    tmux_run(
        &client,
        &[
            "split-window",
            "-v",
            "-t",
            &agent_pane,
            "-l",
            "20%",
            "-P",
            "-F",
            "#{pane_id}",
        ],
    )
    .await?;

    // 7. If --editor: split agent pane horizontally, editor on the left
    if editor {
        tmux_run(
            &client,
            &[
                "split-window",
                "-hb",
                "-t",
                &agent_pane,
                "-l",
                "50%",
                "-P",
                "-F",
                "#{pane_id}",
            ],
        )
        .await?;
    }

    // 8. Launch claude in agent pane
    tmux_run(
        &client,
        &["send-keys", "-t", &agent_pane, "claude", "Enter"],
    )
    .await?;

    // 9. Install resize/new-window hooks + keybindings
    install_resize_hooks(&client, &socket_name, &session_name).await?;
    install_new_window_hook(&client, &socket_name, &session_name).await?;

    // 10. Focus the agent pane and attach
    tmux_run(&client, &["select-pane", "-t", &agent_pane]).await?;
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
    let script_path = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("atm")
        .join(format!("resize-sidebar-{session_name}.sh"));
    let _ = tmux_run(&client, &["run-shell", &run_shell_arg(&script_path, &[])]).await;

    // 7. Attach
    exec_attach(&socket_name, &session_name)
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
        let script = |name: &str| data_home.path().join("atm").join(name);
        let resize = script("resize-sidebar-my.proj.sh");
        assert_eq!(
            commands,
            vec![
                super::run_shell_command(&resize, &[]),
                super::run_shell_command(&resize, &["hook"]),
                super::run_shell_command(&resize, &[]),
                super::run_shell_command(&script("inject-sidebar-my.proj.sh"), &[]),
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
