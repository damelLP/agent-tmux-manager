use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    future::poll_fn,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc, Mutex,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::body::{Body, Bytes, HttpBody};
use serde::Serialize;
use tokio::{
    sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore},
    time::timeout,
};

use crate::relay::IO;

const MAX_FILE: u64 = 32 * 1024 * 1024;
const MAX_THUMB: u64 = 1024 * 1024;
const STORE_QUOTA: u64 = 2 * 1024 * 1024 * 1024;
const DOWNLOAD: &str = "application/octet-stream";

#[derive(Debug)]
pub(crate) enum Failure {
    Busy,
    Full,
    Size,
    Path,
    Gone,
    Store,
}
#[derive(Debug, Serialize)]
pub(crate) struct Upload {
    path: PathBuf,
    size: u64,
}
pub(crate) struct Found {
    pub(crate) data: Bytes,
    pub(crate) size: u64,
    pub(crate) mime: &'static str,
    pub(crate) download_name: Option<String>,
    pub(crate) mutable: bool,
}

#[derive(Debug)]
pub(crate) struct Store {
    root: PathBuf,
    serve_roots: Vec<PathBuf>,
    serial: AtomicU64,
    used: Mutex<u64>,
    quota: u64,
    uploads: Arc<Semaphore>,
    reads: Arc<Semaphore>,
}

type Job<T> = oneshot::Receiver<Result<T, Failure>>;
type WriteJob = (mpsc::Sender<Option<Bytes>>, Job<Upload>);

fn safe_name(name: &str) -> String {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = basename
        .trim_start_matches('.')
        .chars()
        .take(100)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "file".into()
    } else {
        clean
    }
}

fn upload_key(time: SystemTime, n: u64) -> String {
    let ms = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{ms:020}-{n:020}")
}

fn under_root(root: &Path, path: &Path) -> bool {
    path.is_absolute()
        && path
            .as_os_str()
            .as_bytes()
            .strip_prefix(root.as_os_str().as_bytes())
            .and_then(|p| p.strip_prefix(b"/"))
            .is_some_and(|p| {
                !p.is_empty() && p.split(|b| *b == b'/').all(|part| !part.starts_with(b"."))
            })
}

fn select_root<'a>(
    store: &'a Path,
    serve_roots: &'a [PathBuf],
    path: &Path,
) -> Option<(&'a Path, bool)> {
    std::iter::once((store, false))
        .chain(serve_roots.iter().map(|root| (root.as_path(), true)))
        .find(|(root, _)| under_root(root, path))
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn bad_serve_root(root: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("serve root {root:?} must be a canonical directory disjoint from other roots and the files store"),
    )
}

fn validate_roots(store: &Path, roots: &[PathBuf]) -> io::Result<()> {
    for (i, root) in roots.iter().enumerate() {
        let explain =
            |err: io::Error| io::Error::new(err.kind(), format!("serve root {root:?}: {err}"));
        if !root.is_absolute()
            || root.parent().is_none()
            || root.to_str().is_none()
            || root.canonicalize().map_err(explain)?.as_os_str() != root.as_os_str()
            || !root.metadata().map_err(explain)?.is_dir()
            || overlaps(root, store)
            || roots.iter().take(i).any(|prior| overlaps(root, prior))
        {
            return Err(bad_serve_root(root));
        }
    }
    Ok(())
}

fn media_type(path: &Path, mutable: bool) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpeg" | "jpg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "md" | "txt" | "log" | "json" | "toml" | "yaml" | "yml" | "csv" | "diff" | "patch"
            if mutable =>
        {
            "text/plain; charset=utf-8"
        }
        _ => DOWNLOAD,
    }
}

// Only this store's root and its per-upload directories; never follow links.
fn used_bytes(dir: &Path, descend: bool) -> io::Result<u64> {
    fs::read_dir(dir)?.try_fold(0u64, |sum, entry| {
        let entry = entry?;
        let meta = entry.metadata()?;
        let size = if meta.is_file() {
            meta.len()
        } else if descend && meta.is_dir() {
            // Pin the directory for cleanup so a replaced entry cannot redirect removal.
            let dir = entry.path();
            let handle = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&dir)?;
            let pinned = PathBuf::from(format!("/proc/self/fd/{}", handle.as_raw_fd()));
            match fs::remove_file(pinned.join(".partial")) {
                Ok(()) => match fs::remove_dir(dir) {
                    Ok(()) => return Ok(sum),
                    Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => (),
                    Err(e) => return Err(e),
                },
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
            used_bytes(&pinned, false)?
        } else {
            0
        };
        Ok(sum.saturating_add(size))
    })
}

pub(crate) fn detached<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> io::Result<oneshot::Receiver<T>> {
    let (tx, rx) = oneshot::channel();
    thread::Builder::new().spawn(move || {
        let _ = tx.send(work());
    })?;
    Ok(rx)
}

async fn complete<T>(job: Job<T>, error: Failure) -> Result<T, Failure> {
    match timeout(IO, job).await {
        Ok(Ok(result)) => result,
        _ => Err(error),
    }
}

impl Store {
    pub(crate) async fn prepare(root: PathBuf, serve_roots: Vec<PathBuf>) -> io::Result<Self> {
        let job = detached(move || Self::open(root, serve_roots))?;
        timeout(IO, job)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?
    }

    fn open(root: PathBuf, serve_roots: Vec<PathBuf>) -> io::Result<Self> {
        validate_roots(&root, &serve_roots)?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
        let root = root.canonicalize()?;
        if root.parent().is_none() || root.to_str().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid files directory",
            ));
        }
        // The store path may contain a symlink. Recheck before touching staging files.
        if let Some(conflict) = serve_roots.iter().find(|r| overlaps(r, &root)) {
            return Err(bad_serve_root(conflict));
        }
        let used = Mutex::new(used_bytes(&root, true)?);
        Ok(Self {
            root,
            serve_roots,
            serial: 0.into(),
            used,
            quota: STORE_QUOTA,
            uploads: Semaphore::new(2).into(),
            reads: Semaphore::new(4).into(),
        })
    }

    fn begin(self: &Arc<Self>, name: String) -> Result<WriteJob, Failure> {
        let slot = self
            .uploads
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::Busy)?;
        let (send, recv) = mpsc::channel(1);
        let store = self.clone();
        let job = detached(move || {
            let _slot = slot;
            store.write(&safe_name(&name), recv)
        })
        .map_err(|_| Failure::Store)?;
        Ok((send, job))
    }

    pub(crate) async fn upload(
        self: &Arc<Self>,
        name: String,
        mut body: Body,
    ) -> Result<Upload, Failure> {
        let (send, mut job) = self.begin(name)?;
        // EOF is an explicit commit marker. Disconnect, cancellation and body errors just close the pipe.
        let transfer = async move {
            while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
                let Ok(frame) = frame else {
                    return;
                };
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                if !matches!(timeout(IO, send.send(Some(data))).await, Ok(Ok(()))) {
                    return;
                }
            }
            let _ = timeout(IO, send.send(None)).await;
        };
        tokio::select! {
            result = &mut job => result.map_err(|_| Failure::Store)?,
            () = transfer => complete(job, Failure::Store).await,
        }
    }

    fn write(
        &self,
        name: &str,
        mut chunks: mpsc::Receiver<Option<Bytes>>,
    ) -> Result<Upload, Failure> {
        if *self.used.lock().map_err(|_| Failure::Store)? >= self.quota {
            return Err(Failure::Full);
        }
        let dir = loop {
            let id = upload_key(SystemTime::now(), self.serial.fetch_add(1, Relaxed));
            let dir = self.root.join(id);
            match DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(Failure::Store),
            }
        };
        let mut staging = Staging {
            store: self,
            dir,
            reserved: 0,
            committed: false,
        };
        let temp = staging.dir.join(".partial");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|_| Failure::Store)?;
        while let Some(data) = chunks.blocking_recv().ok_or(Failure::Store)? {
            let size = data.len() as u64;
            if staging.reserved + size > MAX_FILE {
                return Err(Failure::Size);
            }
            {
                let mut used = self.used.lock().map_err(|_| Failure::Store)?;
                if size > self.quota.saturating_sub(*used) {
                    return Err(Failure::Full);
                }
                *used += size;
                staging.reserved += size;
            }
            file.write_all(&data).map_err(|_| Failure::Store)?;
        }
        let path = staging.dir.join(name);
        fs::rename(temp, &path).map_err(|_| Failure::Store)?;
        staging.committed = true;
        Ok(Upload {
            path,
            size: staging.reserved,
        })
    }

    pub(crate) async fn read(
        &self,
        path: PathBuf,
        head: bool,
        thumb: bool,
    ) -> Result<Found, Failure> {
        let (root, mutable) =
            select_root(&self.root, &self.serve_roots, &path).ok_or(Failure::Path)?;
        let slot = timeout(IO, self.reads.clone().acquire_owned())
            .await
            .map_err(|_| Failure::Busy)?
            .map_err(|_| Failure::Busy)?;
        let root = root.to_path_buf();
        let job = detached(move || Self::read_file(&root, &path, head, thumb, mutable, slot))
            .map_err(|_| Failure::Busy)?;
        complete(job, Failure::Busy).await
    }

    fn read_file(
        root: &Path,
        path: &Path,
        head: bool,
        thumb: bool,
        mutable: bool,
        slot: OwnedSemaphorePermit,
    ) -> Result<Found, Failure> {
        // Pin the object without opening a FIFO/device for I/O. On Linux the fd link
        // then reopens this very object, not the caller's (possibly replaced) path.
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NONBLOCK)
            .open(path)
            .map_err(|err| {
                if err.kind() == io::ErrorKind::NotFound {
                    Failure::Gone
                } else {
                    Failure::Path
                }
            })?;
        let meta = handle.metadata().map_err(|_| Failure::Path)?;
        let fd = format!("/proc/self/fd/{}", handle.as_raw_fd());
        let actual = fs::read_link(&fd).map_err(|_| Failure::Path)?;
        let size = meta.len();
        if !meta.is_file() || size > MAX_FILE || !under_root(root, &actual) {
            return Err(Failure::Path);
        }
        let mime = media_type(&actual, mutable);
        let cap = if thumb && mutable {
            MAX_THUMB
        } else {
            MAX_FILE
        };
        if thumb && (!mime.starts_with("image/") || size > cap) {
            return Err(Failure::Gone);
        }
        let data = if head {
            Bytes::new()
        } else {
            let mut bytes = Vec::with_capacity(size as usize);
            File::open(&fd)
                .map_err(|_| Failure::Path)?
                .take(cap + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Failure::Path)?;
            if bytes.len() as u64 != size {
                return Err(Failure::Path);
            }
            // Leases also survive slow clients: at most four bounded bodies can be buffered.
            Bytes::from_owner(LeasedBytes { bytes, _slot: slot })
        };
        let download_name = (mime == DOWNLOAD).then(|| {
            safe_name(
                actual
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default(),
            )
        });
        Ok(Found {
            data,
            size,
            mime,
            download_name,
            mutable,
        })
    }
}

struct Staging<'a> {
    store: &'a Store,
    dir: PathBuf,
    reserved: u64,
    committed: bool,
}
impl Drop for Staging<'_> {
    fn drop(&mut self) {
        if !self.committed && fs::remove_dir_all(&self.dir).is_ok() {
            if let Ok(mut used) = self.store.used.lock() {
                *used -= self.reserved;
            }
        }
    }
}

struct LeasedBytes {
    bytes: Vec<u8>,
    _slot: OwnedSemaphorePermit,
}
impl AsRef<[u8]> for LeasedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod unit {
    use super::*;
    use std::{
        fs,
        time::{Duration, UNIX_EPOCH},
    };
    use tempfile::tempdir;

    #[test]
    fn upload_names_are_safe_basenames() {
        for (given, expected) in [
            ("outside/../.summer 01.JPG", "summer_01.JPG"),
            (r"camera\photo-é+_.png", "photo-___.png"),
            ("../...", "file"),
            ("root/", "file"),
        ] {
            assert_eq!(safe_name(given), expected);
        }
        assert_eq!(safe_name(&format!(".{}", "f".repeat(120))), "f".repeat(100));
    }

    #[test]
    fn upload_keys_depend_on_utc_time_and_counter() {
        let time = UNIX_EPOCH + Duration::from_millis(2020);
        assert_eq!(
            upload_key(time, 7),
            "00000000000000002020-00000000000000000007"
        );
        assert!(upload_key(time, 7) < upload_key(time, 8));
        assert!(upload_key(time, 8) < upload_key(time + Duration::from_millis(1), 1));
    }

    #[test]
    fn path_is_under_store_only_without_dot_components() {
        let root = Path::new("/fixture/.data/files");
        assert!(under_root(root, &root.join("123/photo.jpg")));
        for path in [
            "123/photo.jpg",
            "/fixture/.data/files",
            "/fixture/.data/files-sibling/photo.jpg",
            "/fixture/.data/files/../photo.jpg",
            "/fixture/.data/files/.hidden/photo.jpg",
            "/fixture/.data/files/./photo.jpg",
        ] {
            assert!(!under_root(root, Path::new(path)), "{path}");
        }
    }

    #[test]
    fn read_root_rule() {
        let store = Path::new("/fixture/files");
        let roots = [
            PathBuf::from("/fixture/project/docs/plans"),
            PathBuf::from("/fixture/second/docs/plans"),
        ];
        for (path, want) in [
            ("/fixture/files/123/upload.txt", Some((store, false))),
            (
                "/fixture/project/docs/plans/nested/report.md",
                Some((roots[0].as_path(), true)),
            ),
            (
                "/fixture/second/docs/plans/shot.png",
                Some((roots[1].as_path(), true)),
            ),
            ("project/docs/plans/report.md", None),
            ("/fixture/project/docs/plans", None),
            ("/fixture/project/docs/plans-old/report.md", None),
            ("/fixture/project/docs/plans/../report.md", None),
            ("/fixture/project/docs/plans/./report.md", None),
            ("/fixture/project/docs/plans/.drafts/report.md", None),
            ("/fixture/project/docs/plans/.report.md", None),
        ] {
            assert_eq!(select_root(store, &roots, Path::new(path)), want, "{path}");
        }
    }

    #[test]
    fn root_configuration_is_canonical_existing_and_disjoint() {
        let dir = tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let store = base.join("store");
        let docs = base.join("docs/plans");
        let nested = docs.join("chunk");
        fs::create_dir_all(&nested).unwrap();
        let staging = nested.join(".partial");
        fs::write(&staging, b"leave this untouched").unwrap();
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&docs, &alias).unwrap();
        let missing = base.join("missing");
        for roots in [
            vec![PathBuf::from("relative")],
            vec![PathBuf::from("/")],
            vec![alias.clone()],
            vec![staging.clone()],
            vec![missing.clone()],
            vec![docs.clone(), nested],
        ] {
            assert!(
                Store::open(store.clone(), roots.clone()).is_err(),
                "{roots:?}"
            );
        }
        assert!(!missing.exists());
        assert!(Store::open(docs.clone(), vec![docs.clone()]).is_err());
        assert!(Store::open(alias, vec![docs.clone()]).is_err());
        let misplaced = docs.join("not-a-store");
        assert!(Store::open(misplaced.clone(), vec![docs.clone()]).is_err());
        assert!(
            !misplaced.exists(),
            "reject the overlap before creating a store"
        );
        assert_eq!(fs::read(&staging).unwrap(), b"leave this untouched");
        assert!(Store::open(store, vec![docs]).is_ok());
    }

    #[test]
    fn root_startup_error_names_the_invalid_argument() {
        let dir = tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let valid = base.join("reports");
        fs::create_dir(&valid).unwrap();
        let file = base.join("not-a-directory");
        fs::write(&file, b"fixture").unwrap();
        for (bad, kind) in [
            (base.join("missing-reports"), io::ErrorKind::NotFound),
            (file, io::ErrorKind::InvalidInput),
        ] {
            let error =
                Store::open(base.join("store"), vec![valid.clone(), bad.clone()]).unwrap_err();
            assert_eq!(error.kind(), kind);
            assert!(error.to_string().contains(bad.to_str().unwrap()), "{error}");
        }
    }

    #[test]
    fn report_extensions_are_plain_text_only_under_agent_roots() {
        for extension in [
            "MD", "txt", "log", "json", "toml", "yaml", "yml", "csv", "diff", "patch",
        ] {
            let path = PathBuf::from(format!("report.{extension}"));
            assert_eq!(media_type(&path, true), "text/plain; charset=utf-8");
            assert_eq!(media_type(&path, false), "application/octet-stream");
        }
        for extension in ["html", "svg", "pdf", "bin"] {
            let path = PathBuf::from(format!("report.{extension}"));
            assert_eq!(media_type(&path, true), "application/octet-stream");
        }
    }

    #[test]
    fn only_allowlisted_image_extensions_are_inline() {
        for (ext, mime) in [
            ("jpg", "image/jpeg"),
            ("JPEG", "image/jpeg"),
            ("PNG", "image/png"),
            ("gif", "image/gif"),
            ("webp", "image/webp"),
            ("svg", "application/octet-stream"),
            ("html", "application/octet-stream"),
            ("txt", "application/octet-stream"),
            ("", "application/octet-stream"),
        ] {
            assert_eq!(
                media_type(Path::new(&format!("fixture.{ext}")), false),
                mime
            );
        }
    }

    #[tokio::test]
    async fn uploads_hold_slots_while_waiting_and_cannot_overbook_quota() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(dir.path().join("store"), Vec::new()).unwrap();
        store.quota = 3;
        let store = Arc::new(store);
        let (a, a_done) = store.begin("a.jpg".into()).unwrap();
        let (b, b_done) = store.begin("b.jpg".into()).unwrap();
        assert!(matches!(
            store.begin("extra.jpg".into()),
            Err(Failure::Busy)
        ));
        for sender in [a, b] {
            // A full write worker may have already returned its refusal.
            let _ = sender.send(Some(Bytes::from_static(b"abc"))).await;
            let _ = sender.send(None).await;
        }
        let (a, b) = tokio::join!(a_done, b_done);
        assert!(matches!(
            (a.unwrap(), b.unwrap()),
            (Ok(_), Err(Failure::Full)) | (Err(Failure::Full), Ok(_))
        ));
        let root = dir.path().join("store");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        let mut restart = Store::open(root, Vec::new()).unwrap();
        restart.quota = 3;
        let (tx, done) = Arc::new(restart).begin("empty.jpg".into()).unwrap();
        let _ = tx.send(None).await;
        assert!(matches!(done.await.unwrap(), Err(Failure::Full)));
    }

    #[tokio::test]
    async fn restart_discards_partial_uploads_before_counting_only_retained_bytes() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("store");
        let unfinished = root.join("unfinished");
        let retained = root.join("retained");
        let outside = dir.path().join("outside");
        for subdir in [&unfinished, &retained, &outside] {
            fs::create_dir_all(subdir).unwrap();
            fs::write(subdir.join(".partial"), b"old staging bytes").unwrap();
        }
        fs::write(retained.join("photo.jpg"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        let mut store = Store::open(root, Vec::new()).unwrap();
        assert!(!unfinished.exists());
        assert!(!retained.join(".partial").exists());
        assert_eq!(fs::read(retained.join("photo.jpg")).unwrap(), b"keep");
        assert_eq!(
            fs::read(outside.join(".partial")).unwrap(),
            b"old staging bytes"
        );
        store.quota = 7;
        let (tx, done) = Arc::new(store).begin("another.jpg".into()).unwrap();
        tx.send(Some(Bytes::from_static(b"123"))).await.unwrap();
        tx.send(None).await.unwrap();
        assert!(done.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn abandoning_a_started_upload_cleans_partial_and_releases_quota() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("store");
        let mut store = Store::open(root.clone(), Vec::new()).unwrap();
        store.quota = 4;
        let store = Arc::new(store);
        let (tx, done) = store.begin("aborted.jpg".into()).unwrap();
        tx.send(Some(Bytes::from_static(b"abc"))).await.unwrap();
        drop(tx); // Previous data is consumed; no explicit EOF/commit was sent.
        assert!(matches!(done.await.unwrap(), Err(Failure::Store)));
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        let (tx, done) = store.begin("complete.jpg".into()).unwrap();
        tx.send(Some(Bytes::from_static(b"1234"))).await.unwrap();
        tx.send(None).await.unwrap();
        assert!(done.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn read_slots_remain_held_until_the_response_bytes_are_dropped() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("store");
        let store = Arc::new(Store::open(root.clone(), Vec::new()).unwrap());
        let path = root.join("file.jpg");
        fs::write(&path, b"image").unwrap();
        let mut responses = Vec::new();
        for _ in 0..4 {
            responses.push(store.read(path.clone(), false, false).await.unwrap());
        }
        let burst =
            futures_util::future::join_all((0..4).map(|_| store.read(path.clone(), false, false)));
        tokio::pin!(burst);
        assert!(futures_util::poll!(&mut burst).is_pending());
        drop(responses);
        for read in burst.await {
            assert_eq!(&read.unwrap().data[..], b"image");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_waiting_for_a_slot_returns_busy_at_the_io_bound() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("store"), Vec::new()).unwrap());
        let _held = store.reads.clone().acquire_many_owned(4).await.unwrap();
        let read = store.read(store.root.join("file.jpg"), true, false);
        tokio::pin!(read);
        assert!(futures_util::poll!(&mut read).is_pending());
        tokio::time::advance(IO / 2).await;
        assert!(futures_util::poll!(&mut read).is_pending());
        tokio::time::advance(IO / 2).await;
        assert!(matches!(read.await, Err(Failure::Busy)));
    }
}
