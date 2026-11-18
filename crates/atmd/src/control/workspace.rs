//! Workspace creation and shared sidebar setup with caller-resolved inputs.
//!
//! These operations do not choose a server, read the CLI environment/config,
//! attach a terminal, or print output. Callers validate names and check for an
//! exact existing session before creation. Partial effects are not rolled back.

use std::path::{Path, PathBuf};

use atm_tmux::{TmuxClient, TmuxError};

use super::shell_quote;

/// Failures running workspace setup commands or writing generated scripts.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("tmux command missing subcommand")]
    MissingSubcommand,
    #[error("tmux {args:?} failed")]
    Tmux {
        args: Vec<String>,
        #[source]
        source: TmuxError,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Caller-resolved script location, namespace and shell command. The socket
/// labels script files only; the supplied client and prefix determine routing.
#[derive(Debug, Clone)]
pub struct WorkspaceHooks {
    pub script_dir: PathBuf,
    pub socket: Option<String>,
    pub tmux_prefix: String,
}

const SIDEBAR_PCT: u32 = 16;
const SIDEBAR_MIN: u32 = 20;
const SIDEBAR_MAX: u32 = 40;

/// `run-shell`'s shell argument, with each word quoted and format hashes doubled.
#[must_use]
pub fn run_shell_arg(path: &Path, args: &[&str]) -> String {
    let mut cmd = shell_quote(&path.display().to_string());
    for arg in args {
        cmd.push(' ');
        cmd.push_str(&shell_quote(arg));
    }
    cmd.replace('#', "##")
}

/// Quote a shell command for tmux without escaping its deliberate formats.
#[must_use]
pub fn run_shell_command_from_arg(arg: &str) -> String {
    let arg = arg
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('\n', "\\n");
    format!("run-shell \"{arg}\"")
}

/// Namespace scripts by their baked-in socket routing, not just the session.
/// Hex preserves arbitrary labels in one lossless filesystem component.
#[must_use]
pub fn workspace_script_path(
    base: &Path,
    socket: Option<&str>,
    session_name: &str,
    kind: &str,
) -> PathBuf {
    let namespace = socket.map_or_else(
        || "implicit".to_string(),
        |label| {
            format!(
                "label-{}",
                label
                    .bytes()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            )
        },
    );
    base.join(namespace)
        .join(format!("{kind}-{session_name}.sh"))
}

/// Run a tmux command through the shared client, returning trimmed stdout.
pub async fn tmux_run<C>(client: &C, args: &[&str]) -> Result<String, WorkspaceError>
where
    C: TmuxClient + ?Sized,
{
    let Some((subcommand, rest)) = args.split_first() else {
        return Err(WorkspaceError::MissingSubcommand);
    };
    let output = client
        .raw_command(subcommand, rest)
        .await
        .map_err(|source| WorkspaceError::Tmux {
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            source,
        })?;
    Ok(output.trim().to_string())
}

/// Exact session target, including the session terminator for pane-taking calls.
#[must_use]
pub fn exact_session(name: &str) -> String {
    format!("={name}:")
}

/// Only tmux's own failed command means absent; transport failures stay errors.
pub async fn has_session<C>(client: &C, session: &str) -> Result<bool, WorkspaceError>
where
    C: TmuxClient + ?Sized,
{
    match tmux_run(client, &["has-session", "-t", &exact_session(session)]).await {
        Ok(_) => Ok(true),
        Err(WorkspaceError::Tmux {
            source: TmuxError::CommandFailed { .. },
            ..
        }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Calculate sidebar width from terminal columns, clamped to [SIDEBAR_MIN, SIDEBAR_MAX].
fn sidebar_width(cols: u16) -> u32 {
    (cols as u32 * SIDEBAR_PCT / 100).clamp(SIDEBAR_MIN, SIDEBAR_MAX)
}

/// Write the resize-sidebar script and install its hooks and prefix bindings.
pub async fn install_resize_hooks<C>(
    client: &C,
    session_name: &str,
    hooks: &WorkspaceHooks,
) -> Result<(), WorkspaceError>
where
    C: TmuxClient + ?Sized,
{
    let script_dir = &hooks.script_dir;
    let script_path = workspace_script_path(
        script_dir,
        hooks.socket.as_deref(),
        session_name,
        "resize-sidebar",
    );
    std::fs::create_dir_all(script_path.parent().unwrap_or(script_dir))?;
    std::fs::write(
        &script_path,
        format!(
            "#!/bin/sh\n\
             # Exit silently: an empty target would resolve to the current window.\n\
             [ -n \"$1\" ] || exit 0\n\
             W=$({tmux} display-message -p -t \"$1\" '#{{window_width}}')\n\
             S=$((W * {pct} / 100))\n\
             [ $S -lt {min} ] && S={min}\n\
             [ $S -gt {max} ] && S={max}\n\
             for p in $({tmux} list-panes -t \"$1\" -F '#{{pane_id}}:#{{@atm-sidebar}}:#{{pane_width}}:#{{@atm-sidebar-tries}}'); do\n\
               id=${{p%%:*}}; n=${{p##*:}}\n\
               case $p in\n\
                 *:1:$S:*) [ -n \"$n\" ] && {tmux} set-option -pu -t \"$id\" @atm-sidebar-tries;;\n\
                 *:1:*) if [ \"$2\" = hook ]; then\n\
                     [ \"${{n:-0}}\" -ge 5 ] && continue\n\
                     {tmux} set-option -p -t \"$id\" @atm-sidebar-tries $((${{n:-0}} + 1))\n\
                   fi\n\
                   {tmux} resize-pane -t \"$id\" -x \"$S\";;\n\
               esac\n\
             done\n",
            tmux = hooks.tmux_prefix,
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
    // The hook/keybinding queue has its window even when child tmux calls
    // would resolve current through a different client or leaked TMUX_PANE.
    let hook_cmd = run_shell_command_from_arg(&format!(
        "{} '#{{window_id}}'",
        run_shell_arg(&script_path, &[])
    ));
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
    // The script's own resize-pane re-fires this hook; preserve its five
    // unsuccessful hook-mode tries and reset on reaching the target width.
    let pane_hook_cmd = run_shell_command_from_arg(&format!(
        "{} '#{{window_id}}' hook",
        run_shell_arg(&script_path, &[])
    ));
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
    // Focus the window's tagged pane; pane indexes shift with layout changes.
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
pub async fn inject_sidebar<C>(
    client: &C,
    target_pane: &str,
    session_name: &str,
    cols: u16,
) -> Result<String, WorkspaceError>
where
    C: TmuxClient + ?Sized,
{
    let width = sidebar_width(cols).to_string();
    let atm_pane = tmux_run(
        client,
        &[
            "split-window",
            "-fhb",
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
    // Pane titles can be overwritten by the shell; @atm-sidebar persists.
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

/// Write and install the inject-sidebar script for the after-new-window hook.
pub async fn install_new_window_hook<C>(
    client: &C,
    session_name: &str,
    hooks: &WorkspaceHooks,
) -> Result<(), WorkspaceError>
where
    C: TmuxClient + ?Sized,
{
    let script_dir = &hooks.script_dir;
    let script_path = workspace_script_path(
        script_dir,
        hooks.socket.as_deref(),
        session_name,
        "inject-sidebar",
    );
    std::fs::create_dir_all(script_path.parent().unwrap_or(script_dir))?;
    std::fs::write(
        &script_path,
        format!(
            "#!/bin/sh\n\
             # Exit silently: a nonzero run-shell exit displays an error in tmux.\n\
             [ -n \"$1\" ] || exit 0\n\
             # The reserved chat window stays separate from workspace sidebars.\n\
             NAME=$({tmux} display-message -p -t \"$1\" '#{{window_name}}') || exit 0\n\
             [ \"$NAME\" = chat ] && exit 0\n\
             W=$({tmux} display-message -p -t \"$1\" '#{{window_width}}')\n\
             S=$((W * {pct} / 100))\n\
             [ $S -lt {min} ] && S={min}\n\
             [ $S -gt {max} ] && S={max}\n\
             ORIG=$({tmux} display-message -p -t \"$1\" '#{{pane_id}}')\n\
             PANE=$({tmux} split-window -hb -t \"$ORIG\" -l $S -P -F '#{{pane_id}}')\n\
             {tmux} select-pane -t \"$PANE\" -T atm-sidebar\n\
             {tmux} set-option -p -t \"$PANE\" @atm-sidebar 1\n\
             {tmux} send-keys -t \"$PANE\" \"atm --compact --tmux-session '{session}'\" Enter\n\
             {tmux} select-pane -t \"$ORIG\"\n",
            tmux = hooks.tmux_prefix,
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
    // window_id targets even new-window -d; hook_window is empty (3.6a/3.8).
    let hook_cmd = run_shell_command_from_arg(&format!(
        "{} '#{{window_id}}'",
        run_shell_arg(&script_path, &[])
    ));
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

/// Create a prechecked workspace and focus its agent pane, without attaching.
/// Terminal size, command, scripts and routing are supplied by the caller.
/// Any failure leaves the already completed steps intact, with no retry.
pub async fn create_workspace<C>(
    client: &C,
    session_name: &str,
    agent_command: &str,
    editor: bool,
    terminal_size: (u16, u16),
    hooks: &WorkspaceHooks,
) -> Result<String, WorkspaceError>
where
    C: TmuxClient + ?Sized,
{
    let (cols, rows) = terminal_size;
    let cols_str = cols.to_string();
    let rows_str = rows.to_string();
    let agent_pane = tmux_run(
        client,
        &[
            "new-session",
            "-d",
            "-s",
            session_name,
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
    inject_sidebar(client, &agent_pane, session_name, cols).await?;
    tmux_run(
        client,
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
    if editor {
        tmux_run(
            client,
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
    // Enter stays separate: tmux treats an argument ending in ';' as a separator.
    tmux_run(client, &["send-keys", "-t", &agent_pane, agent_command]).await?;
    tmux_run(client, &["send-keys", "-t", &agent_pane, "Enter"]).await?;
    install_resize_hooks(client, session_name, hooks).await?;
    install_new_window_hook(client, session_name, hooks).await?;
    tmux_run(client, &["select-pane", "-t", &agent_pane]).await?;
    Ok(agent_pane)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_tmux::mock::MockCall;
    use atm_tmux::{MockTmuxClient, TmuxError};

    fn raw(subcommand: &str, args: &[&str]) -> MockCall {
        MockCall::RawCommand {
            subcommand: subcommand.to_string(),
            args: args.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    // Catches using process defaults instead of the explicit size/command,
    // changing split order/targets, or folding a trailing semicolon into Enter.
    #[tokio::test]
    async fn create_uses_explicit_inputs_and_preserves_the_full_sequence() {
        for editor in [false, true] {
            let dir = tempfile::tempdir().expect("script directory");
            let hooks = WorkspaceHooks {
                script_dir: dir.path().join("scripts"),
                socket: Some("sock".to_string()),
                tmux_prefix: "chosen-tmux -L 'sock'".to_string(),
            };
            let client = MockTmuxClient::new();
            client.set_next_raw_output(" %10\n");
            client.set_next_raw_output("%20\n");
            let pane = create_workspace(
                &client,
                "my.proj",
                "chosen-agent 'model' tail;",
                editor,
                (250, 60),
                &hooks,
            )
            .await
            .expect("create");
            assert_eq!(pane, "%10");
            let mut want = vec![
                raw(
                    "new-session",
                    &[
                        "-d",
                        "-s",
                        "my.proj",
                        "-x",
                        "250",
                        "-y",
                        "60",
                        "-P",
                        "-F",
                        "#{pane_id}",
                    ],
                ),
                raw(
                    "split-window",
                    &["-fhb", "-t", "%10", "-l", "40", "-P", "-F", "#{pane_id}"],
                ),
                raw("select-pane", &["-t", "%20", "-T", "atm-sidebar"]),
                raw("set-option", &["-p", "-t", "%20", "@atm-sidebar", "1"]),
                raw(
                    "send-keys",
                    &[
                        "-t",
                        "%20",
                        "atm --compact --tmux-session 'my.proj'",
                        "Enter",
                    ],
                ),
                raw(
                    "split-window",
                    &["-v", "-t", "%10", "-l", "20%", "-P", "-F", "#{pane_id}"],
                ),
            ];
            if editor {
                want.push(raw(
                    "split-window",
                    &["-hb", "-t", "%10", "-l", "50%", "-P", "-F", "#{pane_id}"],
                ));
            }
            let resize = hooks
                .script_dir
                .join("label-736f636b/resize-sidebar-my.proj.sh");
            let inject = hooks
                .script_dir
                .join("label-736f636b/inject-sidebar-my.proj.sh");
            let resize_cmd = format!("run-shell \"'{}' '#{{window_id}}'\"", resize.display());
            let pane_cmd = format!("run-shell \"'{}' '#{{window_id}}' hook\"", resize.display());
            let inject_cmd = format!("run-shell \"'{}' '#{{window_id}}'\"", inject.display());
            want.extend([
                raw("send-keys", &["-t", "%10", "chosen-agent 'model' tail;"]),
                raw("send-keys", &["-t", "%10", "Enter"]),
                raw(
                    "set-hook",
                    &["-t", "=my.proj:", "after-resize-window", &resize_cmd],
                ),
                raw(
                    "set-hook",
                    &["-t", "=my.proj:", "after-resize-pane", &pane_cmd],
                ),
                raw("bind-key", &["-T", "prefix", "R", &resize_cmd]),
                raw(
                    "bind-key",
                    &[
                        "-T",
                        "prefix",
                        "a",
                        "run-shell",
                        "-C",
                        "select-pane -t '#{P:#{?#{@atm-sidebar},#{pane_id},}}'",
                    ],
                ),
                raw(
                    "set-hook",
                    &["-t", "=my.proj:", "after-new-window", &inject_cmd],
                ),
                raw("select-pane", &["-t", "%10"]),
            ]);
            assert_eq!(client.calls(), want);
            assert!(resize.is_file());
            assert!(inject.is_file());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&resize).unwrap().permissions().mode() & 0o777,
                    0o755
                );
                assert_eq!(
                    std::fs::metadata(&inject).unwrap().permissions().mode() & 0o777,
                    0o755
                );
            }
        }
    }

    // Catches a dependency on terminal/environment defaults and width rounding.
    #[tokio::test]
    async fn sidebar_width_comes_only_from_the_supplied_columns() {
        for (cols, width) in [(80, "20"), (199, "31"), (400, "40")] {
            let client = MockTmuxClient::new();
            client.set_next_raw_output("%21\n");
            assert_eq!(
                inject_sidebar(&client, "%17", "ws", cols).await.unwrap(),
                "%21"
            );
            assert_eq!(
                client.calls(),
                vec![
                    raw(
                        "split-window",
                        &["-fhb", "-t", "%17", "-l", width, "-P", "-F", "#{pane_id}"]
                    ),
                    raw("select-pane", &["-t", "%21", "-T", "atm-sidebar"]),
                    raw("set-option", &["-p", "-t", "%21", "@atm-sidebar", "1"]),
                    raw(
                        "send-keys",
                        &["-t", "%21", "atm --compact --tmux-session 'ws'", "Enter"]
                    ),
                ]
            );
        }
    }

    // Catches swallowing a tmux failure or continuing with an empty pane id.
    #[tokio::test]
    async fn create_stops_on_a_tmux_error_without_writing_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = WorkspaceHooks {
            script_dir: dir.path().join("absent"),
            socket: None,
            tmux_prefix: "tmux".to_string(),
        };
        let client = MockTmuxClient::new();
        client.set_next_error(TmuxError::NotFound);
        let error = create_workspace(&client, "ws", "agent", false, (80, 24), &hooks)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            WorkspaceError::Tmux {
                source: TmuxError::NotFound,
                ..
            }
        ));
        assert_eq!(
            client.calls(),
            vec![raw(
                "new-session",
                &[
                    "-d",
                    "-s",
                    "ws",
                    "-x",
                    "80",
                    "-y",
                    "24",
                    "-P",
                    "-F",
                    "#{pane_id}"
                ]
            )]
        );
        assert!(!hooks.script_dir.exists());
    }

    // Catches moving installation earlier, ignoring an I/O failure, or adding rollback.
    #[tokio::test]
    async fn create_retains_partial_effects_when_script_installation_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-directory");
        std::fs::write(&path, "occupied").unwrap();
        let hooks = WorkspaceHooks {
            script_dir: path.clone(),
            socket: None,
            tmux_prefix: "tmux".to_string(),
        };
        let client = MockTmuxClient::new();
        client.set_next_raw_output("%10");
        client.set_next_raw_output("%20");
        let error = create_workspace(&client, "ws", "agent;", false, (80, 24), &hooks)
            .await
            .unwrap_err();
        assert!(matches!(error, WorkspaceError::Io(_)));
        let calls = client.calls();
        assert_eq!(calls.len(), 8);
        assert_eq!(
            calls[6..],
            [
                raw("send-keys", &["-t", "%10", "agent;"]),
                raw("send-keys", &["-t", "%10", "Enter"])
            ]
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "occupied");
    }

    // Catches injecting into reserved chat, skipping similarly named ordinary
    // windows, or mutating an implicit window after a missing/failed target.
    #[cfg(unix)]
    #[tokio::test]
    async fn inject_hook_skips_only_reserved_chat_window() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("chosen-tmux");
        let log = dir.path().join("calls");
        std::fs::write(
            &fake,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
shift
case $1 in
  display-message) case $* in
    *window_name*) case $4 in
      @71) echo chat;;
      @72) echo Chat;;
      @73) echo chat-notes;;
      @74) printf '%s\n' 'work $(false) "quoted" spaces';;
      @75) exit 1;;
    esac;;
    *window_width*) echo 250;;
    *) echo '%6';;
  esac;;
  split-window) echo '%8';;
esac
"#,
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hooks = WorkspaceHooks {
            script_dir: dir.path().join("scripts"),
            socket: None,
            tmux_prefix: format!("'{}' --chosen", fake.display()),
        };
        install_new_window_hook(&MockTmuxClient::new(), "ws", &hooks)
            .await
            .unwrap();
        for (target, inject) in [
            (None, false),
            (Some(""), false),
            (Some("@71"), false),
            (Some("@72"), true),
            (Some("@73"), true),
            (Some("@74"), true),
            (Some("@75"), false),
        ] {
            std::fs::write(&log, "").unwrap();
            let mut command = tokio::process::Command::new("/bin/sh");
            command
                .arg(hooks.script_dir.join("implicit/inject-sidebar-ws.sh"))
                .args(target)
                .kill_on_drop(true);
            let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
                .await
                .expect("script hang guard")
                .expect("run generated script");
            assert!(
                output.status.success() && output.stdout.is_empty() && output.stderr.is_empty(),
                "{target:?}: {output:?}"
            );
            let calls = std::fs::read_to_string(&log).unwrap();
            let mut expected = Vec::new();
            if let Some(target) = target.filter(|target| !target.is_empty()) {
                expected.push(format!(
                    "--chosen display-message -p -t {target} #{{window_name}}"
                ));
                if inject {
                    expected.extend([
                        format!("--chosen display-message -p -t {target} #{{window_width}}"),
                        format!("--chosen display-message -p -t {target} #{{pane_id}}"),
                        "--chosen split-window -hb -t %6 -l 40 -P -F #{pane_id}".into(),
                        "--chosen select-pane -t %8 -T atm-sidebar".into(),
                        "--chosen set-option -p -t %8 @atm-sidebar 1".into(),
                        "--chosen send-keys -t %8 atm --compact --tmux-session 'ws' Enter".into(),
                        "--chosen select-pane -t %6".into(),
                    ]);
                }
            }
            assert_eq!(calls.lines().collect::<Vec<_>>(), expected, "{target:?}");
        }
    }

    // Runs the generated artifacts: catches using the daemon's implicit tmux
    // instead of the caller-supplied prefix, and lost window/pane targets.
    #[cfg(unix)]
    #[tokio::test]
    async fn hook_scripts_execute_the_supplied_command_with_explicit_targets() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("chosen-tmux");
        let log = dir.path().join("calls");
        std::fs::write(&fake, format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nshift\ncase $1 in\ndisplay-message) case $* in *window_name*) echo work;; *window_width*) echo 250;; *) echo '%6';; esac;;\nlist-panes) echo '%7:1:20:';;\nsplit-window) echo '%8';;\nesac\n", log.display()
        )).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hooks = WorkspaceHooks {
            script_dir: dir.path().join("scripts"),
            socket: None,
            tmux_prefix: format!("'{}' --chosen", fake.display()),
        };
        let client = MockTmuxClient::new();
        install_resize_hooks(&client, "ws", &hooks).await.unwrap();
        install_new_window_hook(&client, "ws", &hooks)
            .await
            .unwrap();
        for kind in ["resize-sidebar", "inject-sidebar"] {
            let status = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                tokio::process::Command::new("/bin/sh")
                    .arg(hooks.script_dir.join(format!("implicit/{kind}-ws.sh")))
                    .arg("@71")
                    .kill_on_drop(true)
                    .status(),
            )
            .await
            .expect("script hang guard")
            .expect("run generated script");
            assert!(status.success());
        }
        assert_eq!(std::fs::read_to_string(log).unwrap().lines().collect::<Vec<_>>(), [
            "--chosen display-message -p -t @71 #{window_width}",
            "--chosen list-panes -t @71 -F #{pane_id}:#{@atm-sidebar}:#{pane_width}:#{@atm-sidebar-tries}",
            "--chosen resize-pane -t %7 -x 40",
            "--chosen display-message -p -t @71 #{window_name}",
            "--chosen display-message -p -t @71 #{window_width}",
            "--chosen display-message -p -t @71 #{pane_id}",
            "--chosen split-window -hb -t %6 -l 40 -P -F #{pane_id}",
            "--chosen select-pane -t %8 -T atm-sidebar",
            "--chosen set-option -p -t %8 @atm-sidebar 1",
            "--chosen send-keys -t %8 atm --compact --tmux-session 'ws' Enter",
            "--chosen select-pane -t %6",
        ]);
    }
}
