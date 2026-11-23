//! One bounded configuration lane, serialized through durable registration.
//!
//! A caller timeout does not drop the pending filesystem receiver. A stuck read
//! therefore occupies one detached worker, not successive replacement threads.
//! Shutdown drops the receiver without joining a possibly stuck filesystem call.
//! No Git subprocesses: only the supported main/linked-checkout layout is read.

use super::{agents::AgentError, OrgHandle};
use atm_org::{
    AgentId, AwaySettings, AwaySettingsError, OrgCommand, OrgState, ProjectFile, ProjectId,
    RoleSpec,
};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{timeout_at, Instant},
};
use tokio_util::sync::CancellationToken;

const BUDGET: Duration = Duration::from_secs(5);
const CAPACITY: usize = 32;

#[derive(Debug, Clone)]
pub(super) struct PreparedAgent {
    pub agent_id: AgentId,
    pub root: PathBuf,
    pub role: RoleSpec,
    pub cwd: PathBuf,
    pub trust_dirs: Vec<PathBuf>,
}

impl PreparedAgent {
    pub(super) fn role_path(&self) -> PathBuf {
        role_path(&self.root, &self.agent_id.name)
    }
}

enum Operation {
    Prepare(String),
    Identify(String),
    Filter,
    AwaySettings,
}
struct Input {
    root: PathBuf,
    operation: Operation,
}
enum Output {
    Prepared(Box<PreparedAgent>),
    Identified(AgentId),
    Filtered(Option<ProjectId>),
    AwaySettings(AwaySettings),
}
type Resolution = (Output, Option<OrgCommand>);
struct Request {
    input: Input,
    deadline: Instant,
    reply: oneshot::Sender<Result<Output, AgentError>>,
}

#[derive(Clone, Debug)]
pub(super) struct AgentProjects {
    requests: mpsc::Sender<Request>,
}

impl AgentProjects {
    pub fn spawn(org: OrgHandle, cancel: CancellationToken) -> (Self, JoinHandle<()>) {
        Self::spawn_with(org, cancel, resolve)
    }

    fn spawn_with<F>(
        org: OrgHandle,
        cancel: CancellationToken,
        resolve: F,
    ) -> (Self, JoinHandle<()>)
    where
        F: Fn(Input, OrgState) -> Result<Resolution, AgentError> + Send + Sync + 'static,
    {
        let (requests, mut receive) = mpsc::channel::<Request>(CAPACITY);
        let resolve = Arc::new(resolve);
        let task = tokio::spawn(async move {
            loop {
                let request = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    request = receive.recv() => match request {
                        Some(request) => request,
                        None => break,
                    },
                };
                if request.reply.is_closed() || Instant::now() >= request.deadline {
                    let _ = request.reply.send(Err(AgentError::Timeout));
                    continue;
                }
                let work = async {
                    // Snapshot, allocation, publication and registration share
                    // this lane. No second caller allocates from a stale state.
                    let state = org.snapshot().await?;
                    let resolve = resolve.clone();
                    let (send, pending) = oneshot::channel();
                    std::thread::Builder::new()
                        .name("atm-agent-config".into())
                        .spawn(move || {
                            let _ = send.send(resolve(request.input, state));
                        })
                        .map_err(|e| AgentError::Internal(format!("configuration worker: {e}")))?;
                    // Retain this receiver even when the RPC caller times out.
                    let (output, command) = pending.await.map_err(|_| unavailable())??;
                    if let Some(command) = command {
                        org.execute(command, chrono::Utc::now()).await?;
                    }
                    Ok(output)
                };
                let result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    result = work => result,
                };
                let _ = request.reply.send(result);
            }
        });
        (Self { requests }, task)
    }

    async fn request(&self, root: PathBuf, operation: Operation) -> Result<Output, AgentError> {
        super::agent_methods::validate_root(&root)?;
        if let Operation::Prepare(name) | Operation::Identify(name) = &operation {
            super::agent_methods::validate_name(name)?;
        }
        let deadline = Instant::now() + BUDGET;
        let (reply, response) = oneshot::channel();
        self.requests
            .try_send(Request {
                input: Input { root, operation },
                deadline,
                reply,
            })
            .map_err(|_| {
                AgentError::Unavailable("project configuration lane is busy or stopped".into())
            })?;
        timeout_at(deadline, response)
            .await
            .map_err(|_| AgentError::Timeout)?
            .map_err(|_| unavailable())?
    }

    pub async fn prepare(&self, root: PathBuf, name: String) -> Result<PreparedAgent, AgentError> {
        match self.request(root, Operation::Prepare(name)).await? {
            Output::Prepared(prepared) => Ok(*prepared),
            _ => Err(unavailable()),
        }
    }
    pub async fn identify(&self, root: PathBuf, name: String) -> Result<AgentId, AgentError> {
        match self.request(root, Operation::Identify(name)).await? {
            Output::Identified(id) => Ok(id),
            _ => Err(unavailable()),
        }
    }
    pub async fn filter(&self, root: PathBuf) -> Result<Option<ProjectId>, AgentError> {
        match self.request(root, Operation::Filter).await? {
            Output::Filtered(id) => Ok(id),
            _ => Err(unavailable()),
        }
    }
    /// The away settings of the project recorded at `root`. Read-only: it
    /// registers nothing and writes no file.
    pub async fn away_settings(&self, root: PathBuf) -> Result<AwaySettings, AgentError> {
        match self.request(root, Operation::AwaySettings).await? {
            Output::AwaySettings(settings) => Ok(settings),
            _ => Err(unavailable()),
        }
    }
}

fn unavailable() -> AgentError {
    AgentError::Unavailable("project configuration lane stopped".into())
}

fn resolve(input: Input, state: OrgState) -> Result<Resolution, AgentError> {
    if matches!(input.operation, Operation::AwaySettings) {
        // The root is a recorded one, read as it is: a project whose folder
        // is gone has no settings file, which is the default.
        let settings = read_away_settings(&input.root.join(".atm/away.toml"))?;
        return Ok((Output::AwaySettings(settings), None));
    }
    let root = project_root(&input.root)?;
    let file = root.join(".atm/project.toml");
    if matches!(input.operation, Operation::Filter) {
        return Ok((Output::Filtered(read_project(&file)?.map(|p| p.id)), None));
    }
    // Validate the entire role and its directories BEFORE publishing a project
    // or returning a preparation that can authorize Up/Respawn's pane effects.
    let role = match &input.operation {
        Operation::Prepare(name) => Some(read_role(&root, name)?),
        _ => None,
    };
    let project = match read_project(&file)? {
        Some(project) => project,
        None => {
            let id = state
                .projects()
                .find(|p| Path::new(&p.root) == root)
                .map(|p| p.id.clone())
                .unwrap_or_else(|| {
                    state.free_project_id(
                        root.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("project"),
                    )
                });
            publish(&file, &ProjectFile { id })?
        }
    };
    let move_root = match state.project(&project.id) {
        Some(old) if Path::new(&old.root) != root => {
            if !Path::new(&old.root).is_absolute() {
                return Err(refused(
                    Path::new(&old.root),
                    "recorded project root is not absolute",
                ));
            }
            if read_project(&Path::new(&old.root).join(".atm/project.toml"))?
                .is_some_and(|p| p.id == project.id)
            {
                return Err(AgentError::Conflict(format!(
                    "project {} still exists at {}",
                    project.id, old.root
                )));
            }
            true
        }
        _ => false,
    };
    let name = match input.operation {
        Operation::Prepare(name) | Operation::Identify(name) => name,
        Operation::Filter | Operation::AwaySettings => return Err(unavailable()),
    };
    let agent_id = AgentId::new(project.id.clone(), name)
        .map_err(|e| AgentError::InvalidParams(e.to_string()))?;
    let command = OrgCommand::RegisterProject {
        project_id: project.id,
        root: path_text(&root)?.to_owned(),
        move_root,
    };
    let output = match role {
        Some((role, cwd, trust_dirs)) => Output::Prepared(Box::new(PreparedAgent {
            agent_id,
            root,
            role,
            cwd,
            trust_dirs,
        })),
        None => Output::Identified(agent_id),
    };
    Ok((output, Some(command)))
}

fn path_text(path: &Path) -> Result<&str, AgentError> {
    path.to_str()
        .ok_or_else(|| AgentError::Refused(format!("path is not UTF-8: {}", path.display())))
}
fn io_error(path: &Path, error: io::Error) -> AgentError {
    AgentError::Internal(format!("{}: {error}", path.display()))
}
fn refused(path: &Path, reason: &str) -> AgentError {
    AgentError::Refused(format!("{}: {reason}", path.display()))
}
fn directory(path: &Path) -> Result<PathBuf, AgentError> {
    let canonical =
        fs::canonicalize(path).map_err(|_| refused(path, "directory cannot be resolved"))?;
    path_text(&canonical)?;
    if !canonical.is_dir() {
        return Err(refused(path, "not a directory"));
    }
    Ok(canonical)
}

fn project_root(path: &Path) -> Result<PathBuf, AgentError> {
    let root = directory(path)?;
    for ancestor in root.ancestors() {
        let git = ancestor.join(".git");
        let metadata = match fs::symlink_metadata(&git) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_error(&git, e)),
        };
        let main = if metadata.is_dir() {
            ancestor.to_path_buf()
        } else if metadata.is_file() {
            let unsupported = || refused(&git, "unsupported gitdir layout");
            let text = fs::read_to_string(&git).map_err(|_| unsupported())?;
            let target = text.strip_prefix("gitdir:").ok_or_else(unsupported)?.trim();
            let target = fs::canonicalize(ancestor.join(target)).map_err(|_| unsupported())?;
            let worktrees = target.parent().ok_or_else(unsupported)?;
            let common = worktrees.parent().ok_or_else(unsupported)?;
            if worktrees.file_name().is_none_or(|n| n != "worktrees")
                || common.file_name().is_none_or(|n| n != ".git")
            {
                return Err(unsupported());
            }
            let commondir =
                fs::read_to_string(target.join("commondir")).map_err(|_| unsupported())?;
            let backlink = fs::read_to_string(target.join("gitdir")).map_err(|_| unsupported())?;
            if fs::canonicalize(target.join(commondir.trim())).map_err(|_| unsupported())? != common
                || fs::canonicalize(target.join(backlink.trim())).map_err(|_| unsupported())? != git
            {
                return Err(unsupported());
            }
            common.parent().ok_or_else(unsupported)?.to_path_buf()
        } else {
            return Err(refused(&git, "unsupported gitdir entry"));
        };
        // The legacy resolver's fallback accepts arbitrary pointers. Only call
        // it after validation, on the main checkout (also handles relative pointers).
        let resolved = crate::project::resolve_project_root(path_text(&main)?)
            .ok_or_else(|| refused(&git, "project root cannot be resolved"))?;
        return directory(Path::new(&resolved));
    }
    Ok(root)
}

fn read_project(path: &Path) -> Result<Option<ProjectFile>, AgentError> {
    match fs::read_to_string(path) {
        Ok(text) => atm_org::parse_project(&text)
            .map(Some)
            .map_err(|_| refused(path, "invalid project configuration")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // ENOENT through a dangling link is not evidence of removal. Walk
            // to the first existing entry rather than treating every failed
            // read as permission to move an identity (or replace its config).
            for ancestor in path.ancestors() {
                match fs::symlink_metadata(ancestor) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(refused(ancestor, "unresolved project configuration link"));
                    }
                    Ok(_) => break,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(io_error(ancestor, e)),
                }
            }
            Ok(None)
        }
        Err(e) => Err(io_error(path, e)),
    }
}

fn read_away_settings(path: &Path) -> Result<AwaySettings, AgentError> {
    match fs::read_to_string(path) {
        // TOML diagnostics can quote the file's text; never include them in RPC errors.
        Ok(text) => atm_org::parse_away_settings(&text).map_err(|error| match error {
            AwaySettingsError::Toml(_) => refused(path, "invalid away settings"),
            AwaySettingsError::CapBelowOne => refused(path, "stacking_cap must be at least 1"),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(AwaySettings::default()),
        Err(e) => Err(io_error(path, e)),
    }
}

fn role_path(root: &Path, name: &str) -> PathBuf {
    root.join(".atm/roles").join(format!("{name}.md"))
}

fn read_role(root: &Path, name: &str) -> Result<(RoleSpec, PathBuf, Vec<PathBuf>), AgentError> {
    let path = role_path(root, name);
    let text = fs::read_to_string(&path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            AgentError::NotFound(format!("role file {} was not found", path.display()))
        } else {
            io_error(&path, e)
        }
    })?;
    // TOML diagnostics can quote role text; never include them in RPC errors.
    let role = atm_org::parse_role(name, &text)
        .map_err(|_| refused(&path, "invalid role configuration"))?;
    if role
        .config
        .spawn_args
        .iter()
        .chain(&role.config.command_prefix)
        .chain(role.config.model.iter())
        .any(|s| s.contains('\0'))
    {
        return Err(refused(&path, "role command contains a NUL byte"));
    }
    let cwd = directory(&root.join(&role.config.cwd))?;
    let trust_dirs = role
        .config
        .trust_dirs
        .iter()
        .map(|p| directory(&root.join(p)))
        .collect::<Result<_, _>>()?;
    Ok((role, cwd, trust_dirs))
}

fn publish(path: &Path, project: &ProjectFile) -> Result<ProjectFile, AgentError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| refused(path, "missing config directory"))?;
    fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
    let text = project
        .to_toml()
        .map_err(|e| AgentError::Internal(e.to_string()))?;
    let temp = parent.join(format!(
        ".project-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| io_error(&temp, e))?;
    let result = (|| {
        output
            .write_all(text.as_bytes())
            .and_then(|()| output.sync_all())
            .map_err(|e| io_error(&temp, e))?;
        // Same-directory link is atomic and never replaces a concurrent winner.
        match fs::hard_link(&temp, path) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(io_error(path, e)),
        }
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| io_error(parent, e))?;
        read_project(path)?
            .ok_or_else(|| refused(path, "project file disappeared during publication"))
    })();
    let cleanup = fs::remove_file(&temp).map_err(|e| io_error(&temp, e));
    let winner = result?;
    cleanup?;
    Ok(winner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn agent_project_timeout_retains_worker_and_runtime_is_responsive() {
        let cancel = CancellationToken::new();
        let dir = tempfile::tempdir().unwrap();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (entered, entry) = oneshot::channel();
        let entered = Mutex::new(Some(entered));
        let (release, gate) = std::sync::mpsc::channel();
        let gate = Mutex::new(gate);
        let (projects, task) =
            AgentProjects::spawn_with(org.clone(), cancel.clone(), move |input, state| {
                count.fetch_add(1, Ordering::SeqCst);
                if let Some(entered) = entered.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    gate.lock().unwrap().recv().unwrap();
                }
                resolve(input, state)
            });
        let first = projects.clone();
        let root = dir.path().to_path_buf();
        let first = tokio::spawn(async move { first.filter(root).await });
        tokio::time::timeout(Duration::from_secs(30), entry)
            .await
            .unwrap()
            .unwrap();
        // Only an in-memory worker gate is held: no paused filesystem or socket I/O.
        tokio::time::pause();
        assert!(matches!(first.await.unwrap(), Err(AgentError::Timeout)));
        assert!(matches!(
            projects.filter(dir.path().into()).await,
            Err(AgentError::Timeout)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::resume();
        assert_eq!(org.snapshot().await.unwrap().projects().count(), 0);
        release.send(()).unwrap();
        assert_eq!(projects.filter(dir.path().into()).await.unwrap(), None);
        // The expired queued request is not run; the retained read completes
        // before the next fresh request can start a filesystem worker.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        cancel.cancel();
        task.await.unwrap();
    }

    #[test]
    fn agent_project_concurrent_publication_never_clobbers_winner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".atm/project.toml");
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut jobs = Vec::new();
        for id in ["one", "two"] {
            let path = path.clone();
            let barrier = barrier.clone();
            jobs.push(std::thread::spawn(move || {
                barrier.wait();
                publish(
                    &path,
                    &ProjectFile {
                        id: id.parse().unwrap(),
                    },
                )
                .unwrap()
            }));
        }
        barrier.wait();
        let mut winners = jobs.into_iter().map(|job| job.join().unwrap());
        let winner = winners.next().unwrap();
        assert_eq!(winners.next().unwrap(), winner);
        assert_eq!(read_project(&path).unwrap().unwrap(), winner);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn agent_project_fifo_read_does_not_block_org_or_shutdown() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(root.join(".atm")).unwrap();
        let fifo = root.join(".atm/project.toml");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: CString supplies a valid NUL-terminated pathname for mkfifo.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        let (entered, entry) = oneshot::channel();
        let entered = Mutex::new(Some(entered));
        let (finished, finish) = oneshot::channel();
        let finished = Mutex::new(Some(finished));
        let (projects, task) =
            AgentProjects::spawn_with(org.clone(), cancel.clone(), move |input, state| {
                entered.lock().unwrap().take().unwrap().send(()).unwrap();
                let result = resolve(input, state);
                let _ = finished.lock().unwrap().take().unwrap().send(());
                result
            });
        let caller = tokio::spawn(async move { projects.filter(root).await });
        tokio::time::timeout(Duration::from_secs(30), entry)
            .await
            .unwrap()
            .unwrap();
        let (opened, open) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let mut file = OpenOptions::new().write(true).open(fifo).unwrap();
            opened.send(()).unwrap(); // Reader and writer have both opened the FIFO.
            gate.recv().unwrap();
            file.write_all(b"id='project'\n").unwrap();
        });
        tokio::time::timeout(Duration::from_secs(30), open)
            .await
            .unwrap()
            .unwrap();
        // Reader cannot finish until this test releases the writer and its EOF.
        assert_eq!(org.snapshot().await.unwrap().projects().count(), 0);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            caller.await.unwrap(),
            Err(AgentError::Unavailable(_))
        ));
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(30), finish)
            .await
            .unwrap()
            .unwrap();
        writer.join().unwrap();
    }

    // Break caught: reading the away settings on the runtime or a blocking
    // pool thread, where a FIFO at that path would hold the daemon or its exit.
    #[tokio::test]
    async fn away_settings_fifo_read_does_not_block_org_or_shutdown() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(root.join(".atm")).unwrap();
        let fifo = root.join(".atm/away.toml");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: CString supplies a valid NUL-terminated pathname for mkfifo.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        let (entered, entry) = oneshot::channel();
        let entered = Mutex::new(Some(entered));
        let (finished, finish) = oneshot::channel();
        let finished = Mutex::new(Some(finished));
        let (projects, task) =
            AgentProjects::spawn_with(org.clone(), cancel.clone(), move |input, state| {
                entered.lock().unwrap().take().unwrap().send(()).unwrap();
                let result = resolve(input, state);
                let _ = finished
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(result.map(|r| r.0));
                Err(unavailable())
            });
        let caller = tokio::spawn(async move { projects.away_settings(root).await });
        tokio::time::timeout(Duration::from_secs(30), entry)
            .await
            .unwrap()
            .unwrap();
        let (opened, open) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let mut file = OpenOptions::new().write(true).open(fifo).unwrap();
            opened.send(()).unwrap(); // Reader and writer have both opened the FIFO.
            gate.recv().unwrap();
            file.write_all(b"stacking_cap = 4\n").unwrap();
        });
        tokio::time::timeout(Duration::from_secs(30), open)
            .await
            .unwrap()
            .unwrap();
        // The reader cannot finish until this test releases the writer; the
        // organisation still answers, and the lane still stops.
        assert_eq!(org.snapshot().await.unwrap().projects().count(), 0);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            caller.await.unwrap(),
            Err(AgentError::Unavailable(_))
        ));
        release.send(()).unwrap();
        let read = tokio::time::timeout(Duration::from_secs(30), finish)
            .await
            .unwrap()
            .unwrap();
        // The held read was the settings file itself, and registered nothing.
        assert!(matches!(
            read,
            Ok(Output::AwaySettings(AwaySettings { stacking_cap: 4 }))
        ));
        writer.join().unwrap();
    }

    #[test]
    fn agent_project_shutdown_completes_despite_held_read() {
        let (release, gate) = std::sync::mpsc::channel();
        let (stopped, stop) = std::sync::mpsc::channel();
        let (exited, exit) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let dir = tempfile::tempdir().unwrap();
            runtime.block_on(async {
                let cancel = CancellationToken::new();
                let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
                let (entered, entry) = oneshot::channel();
                let entered = Mutex::new(Some(entered));
                let gate = Mutex::new(gate);
                let (projects, task) =
                    AgentProjects::spawn_with(org, cancel.clone(), move |input, state| {
                        entered.lock().unwrap().take().unwrap().send(()).unwrap();
                        gate.lock().unwrap().recv().unwrap();
                        let result = resolve(input, state);
                        exited.send(()).unwrap();
                        result
                    });
                let root = dir.path().to_path_buf();
                let caller = tokio::spawn(async move { projects.filter(root).await });
                tokio::time::timeout(Duration::from_secs(30), entry)
                    .await
                    .unwrap()
                    .unwrap();
                cancel.cancel();
                task.await.unwrap();
                assert!(matches!(
                    caller.await.unwrap(),
                    Err(AgentError::Unavailable(_))
                ));
            });
            drop(runtime);
            stopped.send(()).unwrap();
        });
        let result = stop.recv_timeout(Duration::from_secs(30));
        let _ = release.send(());
        thread.join().unwrap();
        assert!(result.is_ok(), "runtime shutdown waited for held read");
        exit.recv_timeout(Duration::from_secs(30)).unwrap();
    }
}
