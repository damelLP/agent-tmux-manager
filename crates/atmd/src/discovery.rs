//! Session discovery - finds existing coding-agent sessions.
//!
//! Scans `/proc` for running agent processes and registers them with
//! minimal data. Each process is tested against per-harness detectors
//! (Claude Code, pi, future) and the matching `Harness` is recorded so
//! the registry tags the session correctly. Full session data arrives
//! via status-line updates (Claude) or extension events (pi).
//!
//! # Async Safety
//!
//! All filesystem operations are run via `spawn_blocking` to avoid
//! blocking the async runtime.
//!
//! # Panic-Free Guarantees
//!
//! This module follows CLAUDE.md panic-free policy:
//! - No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, `todo!()`
//! - All fallible operations use `?`, pattern matching, or `unwrap_or`
//! - Discovery errors are logged but never fatal

use std::path::PathBuf;

use atm_core::{builtin_harnesses, Harness, HarnessDefinition, SessionId};
use thiserror::Error;
use tracing::{debug, info, trace, warn};

use crate::registry::RegistryHandle;
use crate::tmux::find_pane_for_pid;

// ============================================================================
// Constants
// ============================================================================

/// Default maximum age of a transcript file to be considered "active" (60 seconds).
pub const DEFAULT_TRANSCRIPT_MAX_AGE_SECS: u64 = 60;

// ============================================================================
// Error Types
// ============================================================================

/// Errors that can occur during discovery.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// Failed to read /proc directory
    #[error("failed to read /proc: {0}")]
    ProcReadError(String),

    /// Failed to read process information
    #[error("failed to read process {pid}: {message}")]
    ProcessReadError { pid: u32, message: String },

    /// No active transcript found
    #[error("no active transcript found for PID {0}")]
    NoActiveTranscript(u32),

    /// Registry error during registration
    #[error("registry error: {0}")]
    RegistryError(String),
}

// ============================================================================
// Result Type
// ============================================================================

/// Result of a discovery operation.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryResult {
    /// Number of sessions successfully discovered
    pub discovered: u32,
    /// Number of failures during discovery
    pub failed: u32,
}

// ============================================================================
// Discovered Process
// ============================================================================

/// Information about a running agent process discovered via /proc.
///
/// `harness` records which detector matched — used by the registry to
/// tag the session so the TUI shows the right vendor badge from the
/// first frame, before any adapter event arrives.
#[derive(Debug, Clone)]
struct DiscoveredProcess {
    /// Process ID
    pid: u32,
    /// Working directory
    cwd: PathBuf,
    /// Tmux pane ID if running in tmux
    tmux_pane: Option<String>,
    /// Which coding-agent harness this process belongs to.
    harness: Harness,
}

// ============================================================================
// Discovery Service
// ============================================================================

/// Service for discovering existing Claude Code sessions.
///
/// Scans `/proc` for Claude processes and registers them in the registry.
/// Called on daemon startup and when TUI requests a rescan.
pub struct DiscoveryService {
    registry: RegistryHandle,
    /// Maximum age in seconds for a transcript to be considered "active".
    transcript_max_age_secs: u64,
}

impl DiscoveryService {
    /// Creates a new discovery service with default settings.
    #[must_use]
    pub fn new(registry: RegistryHandle) -> Self {
        Self {
            registry,
            transcript_max_age_secs: DEFAULT_TRANSCRIPT_MAX_AGE_SECS,
        }
    }

    /// Creates a new discovery service with custom transcript age threshold.
    #[must_use]
    pub fn with_max_age(registry: RegistryHandle, transcript_max_age_secs: u64) -> Self {
        Self {
            registry,
            transcript_max_age_secs,
        }
    }

    /// Discover and register existing Claude sessions.
    ///
    /// Scans `/proc` for Claude processes, finds their transcripts,
    /// and registers minimal sessions in the registry.
    ///
    /// # Returns
    ///
    /// A `DiscoveryResult` with counts of discovered and failed sessions.
    /// Errors for individual sessions are logged but don't stop discovery.
    pub async fn discover(&self) -> DiscoveryResult {
        let mut result = DiscoveryResult::default();

        // Scan for agent processes (blocking I/O in spawn_blocking)
        let processes = match tokio::task::spawn_blocking(scan_agent_processes).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                warn!(error = %e, "Failed to scan for agent processes");
                return result;
            }
            Err(e) => {
                warn!(error = %e, "Discovery task panicked");
                return result;
            }
        };

        if processes.is_empty() {
            debug!("No agent processes found");
            return result;
        }

        debug!(count = processes.len(), "Found agent processes");

        // Try to discover each process
        let max_age_secs = self.transcript_max_age_secs;
        for process in processes {
            match self.discover_session(&process, max_age_secs).await {
                Ok(Some(session_id)) => {
                    debug!(
                        session_id = %session_id,
                        pid = process.pid,
                        "Discovered session"
                    );
                    result.discovered += 1;
                }
                Ok(None) => {
                    // Session already registered or no transcript found
                    debug!(
                        pid = process.pid,
                        "Skipped process (already registered or no transcript)"
                    );
                }
                Err(e) => {
                    debug!(
                        pid = process.pid,
                        error = %e,
                        "Failed to discover session"
                    );
                    result.failed += 1;
                }
            }
        }

        if result.discovered > 0 || result.failed > 0 {
            info!(
                discovered = result.discovered,
                failed = result.failed,
                "Discovery complete"
            );
        }

        result
    }

    /// Discovers a session for a Claude process.
    ///
    /// Always registers a pending session with ID `pending-{pid}`. The real
    /// session_id will be set when the first status line update arrives
    /// (which includes both session_id and pid).
    ///
    /// We intentionally avoid using transcript filenames as session IDs because:
    /// - Multiple Claude processes in the same directory share the same transcript folder
    /// - We cannot reliably map a transcript file to a specific PID
    /// - Using transcript-based IDs caused session deduplication bugs
    ///
    /// Returns:
    /// - `Ok(Some(session_id))` if session was discovered and registered
    /// - `Ok(None)` if session already exists
    /// - `Err` if registration failed
    async fn discover_session(
        &self,
        process: &DiscoveredProcess,
        #[allow(unused_variables)] max_age_secs: u64,
    ) -> Result<Option<SessionId>, DiscoveryError> {
        let pid = process.pid;
        let cwd = process.cwd.clone();
        let tmux_pane = process.tmux_pane.clone();
        let harness = process.harness;

        // Always use pending-{pid} as the initial session ID.
        // The real session_id will arrive via status line update or
        // adapter event.
        let session_id = SessionId::pending_from_pid(pid);

        debug!(
            pid,
            session_id = %session_id,
            tmux_pane = ?tmux_pane,
            harness = %harness,
            "Creating pending session for discovered agent process"
        );

        // Register the discovered session
        match self
            .registry
            .register_discovered(session_id.clone(), pid, cwd, tmux_pane, harness)
            .await
        {
            Ok(()) => Ok(Some(session_id)),
            Err(e) => Err(DiscoveryError::RegistryError(e.to_string())),
        }
    }
}

// ============================================================================
// Blocking Filesystem Operations
// ============================================================================

/// Maximum parent-chain depth walked when collecting ancestor PIDs.
const MAX_ANCESTOR_WALK: usize = 25;

/// A matched process plus the process-tree context needed to dedupe
/// wrapper/child pairs (see [`dedupe_wrapper_chains`]).
#[derive(Debug, Clone)]
struct ProcessMatch {
    process: DiscoveredProcess,
    /// Ancestor PIDs, nearest first, bounded by `MAX_ANCESTOR_WALK`.
    ancestor_pids: Vec<u32>,
    /// True if `/proc/{pid}/comm` equals the matched definition's
    /// binary name — the same test the vendor hook scripts use to
    /// resolve the agent PID they report to the daemon.
    comm_is_binary: bool,
}

/// Scans /proc for coding-agent processes.
///
/// Single pass: for each PID, dispatches through the built-in harness
/// registry. The first matching definition wins. Adding a new built-in
/// harness means adding one data record in atm-core; no caller changes.
///
/// Launcher-wrapper chains (e.g. Codex's `node .../bin/codex` wrapper
/// spawning the native `codex` binary) can match twice — once via
/// cmdline, once via exe. A dedupe pass collapses each such chain to
/// one process so a single agent never yields two sessions.
///
/// This function performs blocking I/O and should be called via
/// `spawn_blocking`.
fn scan_agent_processes() -> Result<Vec<DiscoveredProcess>, DiscoveryError> {
    let mut matches = Vec::new();

    // Read /proc directory
    let proc_dir =
        std::fs::read_dir("/proc").map_err(|e| DiscoveryError::ProcReadError(e.to_string()))?;

    for entry in proc_dir.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        // Only process numeric directories (PIDs)
        let pid: u32 = match name.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        if let Some(process_match) = detect_agent_process(pid) {
            matches.push(process_match);
        }
    }

    Ok(dedupe_wrapper_chains(matches))
}

/// Tries each registered harness detector against `pid`. Returns the
/// first match (with process-tree context for dedupe) or `None`.
fn detect_agent_process(pid: u32) -> Option<ProcessMatch> {
    builtin_harnesses()
        .filter(|definition| definition.discovery_enabled)
        .find_map(|definition| {
            check_harness_process(pid, definition).map(|process| ProcessMatch {
                comm_is_binary: process_comm(pid).as_deref() == Some(definition.binary),
                ancestor_pids: collect_ancestor_pids(pid),
                process,
            })
        })
}

/// Collapses ancestor/descendant chains of same-harness matches down to
/// a single process each.
///
/// A launcher wrapper and the agent binary it spawns can both satisfy a
/// harness's matchers (observed live with Codex: the node wrapper
/// matches via a path-like cmdline argument, its native child via
/// `/proc/{pid}/exe`), which would register one agent as two sessions.
///
/// Winner selection mirrors the vendor hook scripts' PID resolution
/// (walk the tree, prefer processes whose `comm` equals the harness
/// binary, keep the topmost) so the session created by discovery is the
/// same one later hook events reconcile onto — never a ghost.
///
/// Ancestry is only collapsed *within* a harness: an agent spawning a
/// different vendor's agent (e.g. Claude driving a Codex) stays two
/// sessions, as it should.
fn dedupe_wrapper_chains(matches: Vec<ProcessMatch>) -> Vec<DiscoveredProcess> {
    let keep: Vec<bool> = (0..matches.len())
        .map(|i| !(0..matches.len()).any(|j| j != i && supersedes(&matches[j], &matches[i])))
        .collect();

    matches
        .into_iter()
        .zip(keep)
        .filter_map(|(m, kept)| {
            if !kept {
                debug!(
                    pid = m.process.pid,
                    harness = %m.process.harness,
                    "Suppressing duplicate discovery of launcher-wrapper chain member"
                );
            }
            kept.then_some(m.process)
        })
        .collect()
}

/// True if `a` should be kept in place of `b`: same harness, related by
/// ancestry, and outranking `b` (comm-matching beats non-matching;
/// within the same class the ancestor wins).
fn supersedes(a: &ProcessMatch, b: &ProcessMatch) -> bool {
    if a.process.harness != b.process.harness {
        return false;
    }
    let a_is_ancestor = b.ancestor_pids.contains(&a.process.pid);
    let b_is_ancestor = a.ancestor_pids.contains(&b.process.pid);
    if !a_is_ancestor && !b_is_ancestor {
        return false;
    }
    match (a.comm_is_binary, b.comm_is_binary) {
        (true, false) => true,
        (false, true) => false,
        // Same comm class: the topmost process wins, matching the hook
        // scripts' walk-up-and-keep-highest PID resolution.
        _ => a_is_ancestor,
    }
}

/// Reads the parent PID from `/proc/{pid}/stat`.
///
/// The comm field (field 2) may contain spaces and parentheses, so the
/// ppid is parsed from after the *last* `)` — the kernel-documented
/// safe way to split this file.
fn read_parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    // Fields after comm: state, ppid, ...
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// Collects ancestor PIDs (nearest first), stopping at init or after
/// `MAX_ANCESTOR_WALK` hops.
fn collect_ancestor_pids(pid: u32) -> Vec<u32> {
    let mut ancestors = Vec::new();
    let mut current = pid;
    for _ in 0..MAX_ANCESTOR_WALK {
        match read_parent_pid(current) {
            Some(ppid) if ppid > 1 => {
                ancestors.push(ppid);
                current = ppid;
            }
            _ => break,
        }
    }
    ancestors
}

/// Reads `/proc/{pid}/comm`, trimmed.
fn process_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Checks if a PID matches a built-in harness definition.
///
/// First attempts to identify via `/proc/{pid}/exe`. Falls back to
/// `/proc/{pid}/cmdline` for shebang/node-based CLIs and path-like command
/// arguments. Bare command-name matches are deliberately limited to argv0 to
/// avoid false positives from arbitrary positional data.
fn check_harness_process(
    pid: u32,
    definition: &'static HarnessDefinition,
) -> Option<DiscoveredProcess> {
    if let Some(process) = check_via_exe(pid, definition) {
        return Some(process);
    }

    let result = check_via_cmdline(pid, definition);

    if result.is_some() {
        trace!(
            pid,
            harness = definition.id,
            "Detected harness via cmdline fallback (exe check failed)"
        );
    }

    result
}

/// Generic helper: tests `/proc/{pid}/exe` against a harness definition and
/// returns a `DiscoveredProcess` tagged with that harness on match.
fn check_via_exe(pid: u32, definition: &'static HarnessDefinition) -> Option<DiscoveredProcess> {
    let exe_path = format!("/proc/{pid}/exe");
    let exe = std::fs::read_link(&exe_path).ok()?;
    let exe_str = exe.to_string_lossy();

    if !definition
        .process_matchers
        .iter()
        .any(|matcher| matcher.matches(&exe_str))
    {
        return None;
    }

    get_process_info(pid, definition.harness)
}

/// Generic helper: scans `/proc/{pid}/cmdline` arguments and returns a
/// `DiscoveredProcess` tagged with the harness if a safe command-shaped arg
/// satisfies the definition's process matchers.
///
/// Bare command-name matches only count at argv0. Later args must be path-like
/// (contain `/`) to avoid treating arbitrary positional data as an agent
/// executable.
fn check_via_cmdline(
    pid: u32,
    definition: &'static HarnessDefinition,
) -> Option<DiscoveredProcess> {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    let cmdline_bytes = std::fs::read(&cmdline_path).ok()?;

    let matched = cmdline_bytes
        .split(|&b| b == 0)
        .filter_map(|bytes| std::str::from_utf8(bytes).ok())
        .filter(|s| !s.is_empty())
        .enumerate()
        .any(|(index, arg)| {
            // Skip flag arguments (e.g. --config)
            if arg.starts_with('-') {
                return false;
            }
            cmdline_arg_matches_definition(index, arg, definition)
        });

    if !matched {
        return None;
    }

    get_process_info(pid, definition.harness)
}

/// Returns true if one cmdline argument can identify a harness.
fn cmdline_arg_matches_definition(
    index: usize,
    arg: &str,
    definition: &'static HarnessDefinition,
) -> bool {
    let is_argv0 = index == 0;
    let is_path_like = arg.contains('/');
    if !is_path_like && (!is_argv0 || !definition.allow_bare_cmdline_match) {
        return false;
    }
    definition
        .process_matchers
        .iter()
        .any(|matcher| matcher.matches(arg))
}

/// Gets process info (cwd, tmux pane) for a PID.
fn get_process_info(pid: u32, harness: Harness) -> Option<DiscoveredProcess> {
    // Read working directory
    let cwd_path = format!("/proc/{pid}/cwd");
    let cwd = std::fs::read_link(&cwd_path).ok()?;

    // Try to find tmux pane for this process
    let tmux_pane = find_pane_for_pid(pid);

    Some(DiscoveredProcess {
        pid,
        cwd,
        tmux_pane,
        harness,
    })
}

// ============================================================================
// Helper Functions (test-only, no longer used in production)
// ============================================================================

#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::time::{Duration, SystemTime};

/// Maps a working directory to Claude's project directory.
///
/// Claude stores transcripts in `~/.claude/projects/{escaped-path}/`.
/// The path is escaped by replacing `/` with `-`.
///
/// Example: `/home/user/code/project` -> `~/.claude/projects/-home-user-code-project/`
///
/// Note: This function is no longer used in production discovery.
/// We now always use pending-{pid} and let status line updates provide the real session ID.
/// Kept for tests and potential future use.
#[cfg(test)]
fn cwd_to_project_dir(cwd: &Path) -> PathBuf {
    let escaped = cwd.to_string_lossy().replace('/', "-");

    // Get home directory from HOME environment variable
    let home = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));

    home.join(".claude/projects").join(escaped)
}

/// Finds the most recently modified transcript in a project directory.
///
/// Only considers:
/// - Files with `.jsonl` extension
/// - Files with UUID-like names (not `agent-*.jsonl` subagent transcripts)
/// - Files modified within the specified max age
///
/// This function performs blocking I/O and should be called via `spawn_blocking`.
///
/// Note: This function is no longer used in production discovery.
/// We now always use pending-{pid} and let status line updates provide the real session ID.
/// Kept for tests and potential future use.
#[cfg(test)]
fn find_active_transcript(project_dir: &Path, max_age_secs: u64) -> Option<PathBuf> {
    let now = SystemTime::now();
    let max_age = Duration::from_secs(max_age_secs);

    let entries = std::fs::read_dir(project_dir).ok()?;

    let mut candidates: Vec<(PathBuf, SystemTime)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();

            // Must be a .jsonl file
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                return None;
            }

            // Must be a UUID-like name (not agent-*)
            let stem = path.file_stem()?.to_string_lossy();
            if stem.starts_with("agent-") {
                return None;
            }

            // Check modification time
            let metadata = entry.metadata().ok()?;
            let mtime = metadata.modified().ok()?;

            // Must be modified within max_age
            let age = now.duration_since(mtime).ok()?;
            if age > max_age {
                return None;
            }

            Some((path, mtime))
        })
        .collect();

    // Sort by modification time (most recent first)
    candidates.sort_by_key(|c| std::cmp::Reverse(c.1));

    // Return the most recent
    candidates.into_iter().next().map(|(path, _)| path)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn test_cwd_to_project_dir_simple() {
        let cwd = Path::new("/home/user/code/project");
        let project_dir = cwd_to_project_dir(cwd);

        let expected_suffix = ".claude/projects/-home-user-code-project";
        assert!(
            project_dir.to_string_lossy().ends_with(expected_suffix),
            "Expected path to end with '{}', got '{}'",
            expected_suffix,
            project_dir.display()
        );
    }

    #[test]
    fn test_cwd_to_project_dir_root() {
        let cwd = Path::new("/");
        let project_dir = cwd_to_project_dir(cwd);

        // Root path becomes empty after escaping, so just check it ends with projects/
        assert!(project_dir.to_string_lossy().contains(".claude/projects"));
    }

    #[test]
    fn test_cwd_to_project_dir_nested() {
        let cwd = Path::new("/home/user/very/deeply/nested/project");
        let project_dir = cwd_to_project_dir(cwd);

        let expected_suffix = "-home-user-very-deeply-nested-project";
        assert!(
            project_dir.to_string_lossy().ends_with(expected_suffix),
            "Got: {}",
            project_dir.display()
        );
    }

    #[test]
    fn test_find_active_transcript_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let result = find_active_transcript(temp_dir.path(), DEFAULT_TRANSCRIPT_MAX_AGE_SECS);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_active_transcript_no_jsonl() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("test.txt"), "not jsonl").unwrap();
        let result = find_active_transcript(temp_dir.path(), DEFAULT_TRANSCRIPT_MAX_AGE_SECS);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_active_transcript_ignores_agent_files() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("agent-abc123.jsonl"), "{}").unwrap();
        let result = find_active_transcript(temp_dir.path(), DEFAULT_TRANSCRIPT_MAX_AGE_SECS);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_active_transcript_finds_recent() {
        let temp_dir = TempDir::new().unwrap();
        let session_file = temp_dir
            .path()
            .join("226f3c14-cc34-4118-804b-b7d442aa2363.jsonl");
        fs::write(&session_file, "{}").unwrap();

        let result = find_active_transcript(temp_dir.path(), DEFAULT_TRANSCRIPT_MAX_AGE_SECS);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), session_file);
    }

    #[test]
    fn test_find_active_transcript_picks_most_recent() {
        let temp_dir = TempDir::new().unwrap();

        // Create two files with different modification times
        let older = temp_dir
            .path()
            .join("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl");
        let newer = temp_dir
            .path()
            .join("ffffffff-0000-1111-2222-333333333333.jsonl");

        fs::write(&older, "old").unwrap();
        // Small delay to ensure different mtime
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&newer, "new").unwrap();

        let result = find_active_transcript(temp_dir.path(), DEFAULT_TRANSCRIPT_MAX_AGE_SECS);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), newer);
    }

    #[test]
    fn test_find_active_transcript_respects_custom_max_age() {
        let temp_dir = TempDir::new().unwrap();
        let session_file = temp_dir
            .path()
            .join("226f3c14-cc34-4118-804b-b7d442aa2363.jsonl");
        fs::write(&session_file, "{}").unwrap();

        // With a very short max age (0 seconds), file should not be found
        // (file was just created, so mtime age > 0)
        // Use 1ms sleep to ensure file is "old"
        std::thread::sleep(Duration::from_millis(1));
        let result = find_active_transcript(temp_dir.path(), 0);
        assert!(result.is_none());

        // With default max age, file should be found
        let result = find_active_transcript(temp_dir.path(), DEFAULT_TRANSCRIPT_MAX_AGE_SECS);
        assert!(result.is_some());
    }

    #[test]
    fn test_discovery_result_default() {
        let result = DiscoveryResult::default();
        assert_eq!(result.discovered, 0);
        assert_eq!(result.failed, 0);
    }

    // ========================================================================
    // Tests for registry-backed process matching
    // ========================================================================

    fn matches_harness_path(harness_id: &str, path: &str) -> bool {
        atm_core::find_harness_definition(harness_id)
            .map(|definition| {
                definition
                    .process_matchers
                    .iter()
                    .any(|matcher| matcher.matches(path))
            })
            .unwrap_or(false)
    }

    #[test]
    fn test_claude_registry_matcher_absolute_path() {
        assert!(matches_harness_path("claude", "/usr/local/bin/claude"));
        assert!(matches_harness_path(
            "claude",
            "/home/user/.local/bin/claude"
        ));
    }

    #[test]
    fn test_claude_registry_matcher_bare_command() {
        assert!(matches_harness_path("claude", "claude"));
    }

    #[test]
    fn test_claude_registry_matcher_versioned_install() {
        assert!(matches_harness_path(
            "claude",
            "/home/user/.local/share/claude/versions/1.2.3/claude"
        ));
        assert!(matches_harness_path(
            "claude",
            "~/.local/share/claude/versions/0.5.0/node"
        ));
    }

    #[test]
    fn test_claude_registry_matcher_rejects_non_claude() {
        assert!(!matches_harness_path("claude", "/usr/bin/bash"));
        assert!(!matches_harness_path("claude", "vim"));
        assert!(!matches_harness_path("claude", "/home/user/claudette"));
        assert!(!matches_harness_path("claude", "claude-dev"));
    }

    #[test]
    fn test_pi_registry_matcher_rejects_bare_cmdline_match() {
        let pi = atm_core::find_harness_definition("pi");
        assert!(pi.is_some_and(|definition| !definition.allow_bare_cmdline_match));
        assert!(matches_harness_path("pi", "/usr/bin/pi"));
        assert!(!matches_harness_path("pi", "not-pi"));
    }

    #[test]
    fn test_cmdline_matching_only_allows_bare_match_on_argv0() {
        let claude = atm_core::find_harness_definition("claude")
            .unwrap_or_else(atm_core::default_harness_definition);
        assert!(cmdline_arg_matches_definition(0, "claude", claude));
        assert!(!cmdline_arg_matches_definition(1, "claude", claude));
        assert!(cmdline_arg_matches_definition(
            1,
            "/usr/local/bin/claude",
            claude
        ));
    }

    #[test]
    fn test_cmdline_matching_rejects_bare_pi_positional_arg() {
        let pi = atm_core::find_harness_definition("pi")
            .unwrap_or_else(atm_core::default_harness_definition);
        assert!(!cmdline_arg_matches_definition(0, "pi", pi));
        assert!(!cmdline_arg_matches_definition(2, "pi", pi));
        assert!(cmdline_arg_matches_definition(
            1,
            "/home/user/.npm/pi-coding-agent/bin/pi.js",
            pi
        ));
    }

    #[test]
    fn test_only_adapter_backed_harnesses_are_discovery_enabled() {
        let enabled: Vec<&str> = atm_core::builtin_harnesses()
            .filter(|definition| definition.discovery_enabled)
            .map(|definition| definition.id)
            .collect();
        assert_eq!(enabled, vec!["claude", "pi", "codex"]);
    }

    // ========================================================================
    // Wrapper-chain dedupe
    // ========================================================================

    fn fake_match(
        pid: u32,
        harness: Harness,
        ancestor_pids: Vec<u32>,
        comm_is_binary: bool,
    ) -> ProcessMatch {
        ProcessMatch {
            process: DiscoveredProcess {
                pid,
                cwd: PathBuf::from("/work"),
                tmux_pane: Some("%1".to_string()),
                harness,
            },
            ancestor_pids,
            comm_is_binary,
        }
    }

    fn kept_pids(matches: Vec<ProcessMatch>) -> Vec<u32> {
        let mut pids: Vec<u32> = dedupe_wrapper_chains(matches)
            .into_iter()
            .map(|p| p.pid)
            .collect();
        pids.sort_unstable();
        pids
    }

    #[test]
    fn codex_wrapper_and_native_child_collapse_to_native() {
        // The live-observed shape: node wrapper (comm "node"/"MainThread")
        // matched via cmdline, native child (comm "codex") matched via exe.
        // The hook script reports the native child's PID, so discovery
        // must keep exactly that one.
        let wrapper = fake_match(100, Harness::Codex, vec![50, 1], false);
        let native = fake_match(200, Harness::Codex, vec![100, 50, 1], true);
        assert_eq!(kept_pids(vec![wrapper, native]), vec![200]);
    }

    #[test]
    fn independent_same_harness_processes_are_all_kept() {
        // Two unrelated codex sessions (no ancestry link) must both stay.
        let a = fake_match(100, Harness::Codex, vec![50, 1], true);
        let b = fake_match(300, Harness::Codex, vec![60, 1], true);
        assert_eq!(kept_pids(vec![a, b]), vec![100, 300]);
    }

    #[test]
    fn chain_where_both_comm_match_keeps_topmost() {
        // If wrapper and child BOTH have the binary comm, the hook
        // scripts' walk keeps the highest one — discovery must agree.
        let parent = fake_match(100, Harness::ClaudeCode, vec![50, 1], true);
        let child = fake_match(200, Harness::ClaudeCode, vec![100, 50, 1], true);
        assert_eq!(kept_pids(vec![parent, child]), vec![100]);
    }

    #[test]
    fn chain_without_any_comm_match_keeps_topmost() {
        let parent = fake_match(100, Harness::Codex, vec![50, 1], false);
        let child = fake_match(200, Harness::Codex, vec![100, 50, 1], false);
        assert_eq!(kept_pids(vec![parent, child]), vec![100]);
    }

    #[test]
    fn cross_harness_ancestry_is_never_deduped() {
        // An agent spawning a different vendor's agent (e.g. Claude
        // driving a codex) is two real sessions, not a wrapper chain.
        let claude = fake_match(100, Harness::ClaudeCode, vec![50, 1], true);
        let codex = fake_match(200, Harness::Codex, vec![100, 50, 1], true);
        assert_eq!(kept_pids(vec![claude, codex]), vec![100, 200]);
    }

    #[test]
    fn three_deep_chain_with_comm_match_in_middle_keeps_it() {
        // wrapper -> real binary -> matched grandchild helper: the
        // comm-matching member wins over both relatives.
        let wrapper = fake_match(100, Harness::Codex, vec![1], false);
        let real = fake_match(200, Harness::Codex, vec![100, 1], true);
        let helper = fake_match(300, Harness::Codex, vec![200, 100, 1], false);
        assert_eq!(kept_pids(vec![wrapper, real, helper]), vec![200]);
    }
}
