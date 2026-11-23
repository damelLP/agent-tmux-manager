//! Fresh, bounded reads from the configured department directory. Its own
//! retained slot keeps a stuck filesystem out of the reporting/authority lane.

use super::agent_context::detached_read;
use atm_org::department::{parse_department, Department, DepartmentError};
use atm_protocol::error_code;
use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use tokio::sync::Semaphore;

const MAX_ENTRIES: usize = 256;
const MAX_FILES: usize = 32;
const FILE_BYTES: usize = 16 * 1024;
const DIAGNOSTIC_BYTES: usize = 256;

#[derive(Debug, Default)]
pub(super) struct DepartmentDirectory {
    pub departments: Vec<Department>,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DepartmentFiles {
    path: Option<PathBuf>,
    pub(super) slot: Arc<Semaphore>,
}

impl DepartmentFiles {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            slot: Arc::new(Semaphore::new(1)),
        }
    }

    pub(super) fn is_configured(&self) -> bool {
        self.path.is_some()
    }

    pub(super) async fn read(&self) -> DepartmentDirectory {
        let Some(path) = self.path.clone() else {
            return DepartmentDirectory::default();
        };
        detached_read(
            self.slot.clone(),
            Path::new("department directory"),
            move || Ok(read_directory(&path)),
        )
        .await
        .unwrap_or_else(|error| {
            directory_problem(if error.0.code == error_code::TIMEOUT {
                "reader timed out or is busy"
            } else {
                "reader unavailable"
            })
        })
    }
}

fn directory_problem(reason: impl std::fmt::Display) -> DepartmentDirectory {
    DepartmentDirectory {
        problems: vec![format!("departments: {reason}")],
        ..Default::default()
    }
}

fn read_directory(path: &Path) -> DepartmentDirectory {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return DepartmentDirectory::default(),
        Err(err) => return directory_problem(format_args!("cannot list directory: {err}")),
    };
    let mut result = DepartmentDirectory::default();
    let mut paths = Vec::new();
    for (i, entry) in entries.enumerate() {
        if i == MAX_ENTRIES {
            return directory_problem(format_args!("scan limit of {MAX_ENTRIES} entries exceeded"));
        }
        match entry {
            Ok(entry) if entry.file_name().as_encoded_bytes().ends_with(b".toml") => {
                paths.push(entry.path())
            }
            Ok(_) => {}
            Err(err) => result
                .problems
                .push(format!("departments: cannot list entry: {err}")),
        }
    }
    paths.sort_unstable();
    if paths.len() > MAX_FILES {
        result.problems.push(format!(
            "departments: file limit of {MAX_FILES} exceeded; reading first {MAX_FILES} by name"
        ));
    }
    for path in paths.into_iter().take(MAX_FILES) {
        let name = path.file_name().unwrap_or_default();
        match read_file(&path, name) {
            Ok(department) => result.departments.push(department),
            Err(error) => result.problems.push(file_problem(name, &error)),
        }
    }
    result
}

#[derive(Debug, Error)]
enum FileError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("id: filename stem is not UTF-8")]
    InvalidStem,
    #[error("not a regular file")]
    NotRegular,
    #[error("file exceeds {FILE_BYTES} bytes")]
    TooLarge,
    #[error("file is not UTF-8")]
    Encoding(#[from] std::str::Utf8Error),
    #[error(transparent)]
    Config(#[from] DepartmentError),
}

fn read_file(path: &Path, name: &OsStr) -> Result<Department, FileError> {
    let id = name
        .to_str()
        .and_then(|name| name.strip_suffix(".toml"))
        .ok_or(FileError::InvalidStem)?;
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(FileError::NotRegular);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(FileError::NotRegular);
    }
    let mut bytes = Vec::new();
    file.take(FILE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > FILE_BYTES {
        return Err(FileError::TooLarge);
    }
    Ok(parse_department(id, std::str::from_utf8(&bytes)?)?)
}

fn file_problem(name: &OsStr, error: &FileError) -> String {
    let name = name.to_string_lossy();
    let detail = error.to_string();
    let mut end = DIAGNOSTIC_BYTES.min(detail.len());
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    let detail = detail.get(..end).unwrap_or_default();
    match error {
        FileError::Config(DepartmentError::Toml {
            line: Some(line), ..
        }) => format!("{name} line {line}: {detail}"),
        FileError::Config(DepartmentError::Toml { line: None, .. }) => format!("{name}: {detail}"),
        FileError::Config(_) | FileError::InvalidStem => format!("{name}:{detail}"),
        _ => format!("{name}: {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::{ffi::OsStringExt, fs::symlink},
    };

    const APPS: &str = "title='Apps'\nprojects=['alpha']\nroster=['alpha/lead']\n";
    const TOOLS: &str =
        "title='Tools'\nprojects=['alpha']\n[[roster]]\nseats='alpha/build-*'\nmax=9\n";

    // Break caught: a bad file hides valid neighbors, or the first read is cached.
    #[tokio::test]
    async fn department_directory_loads_neighbors_and_reads_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("tools.toml"), TOOLS).unwrap();
        fs::write(root.join("apps.toml"), APPS).unwrap();
        fs::write(
            root.join("bad.toml"),
            "title='Bad'\nprojects=['alpha']\nkey = !\n",
        )
        .unwrap();
        fs::write(root.join("notes.txt"), "not TOML").unwrap();
        let files = DepartmentFiles::new(Some(root.into()));
        let read = files.read().await;
        assert_eq!(
            read.departments
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            ["apps", "tools"]
        );
        assert_eq!(
            read.departments[0].roster[0].seats.to_string(),
            "alpha/lead"
        );
        assert_eq!(read.departments[1].roster[0].max.unwrap().get(), 9);
        assert_eq!(read.problems.len(), 1);
        assert!(
            read.problems[0].starts_with("bad.toml line 3:"),
            "{:?}",
            read.problems
        );

        fs::write(root.join("apps.toml"), APPS.replace("Apps", "Updated")).unwrap();
        fs::remove_file(root.join("tools.toml")).unwrap();
        fs::remove_file(root.join("bad.toml")).unwrap();
        let read = files.read().await;
        assert_eq!(read.departments.len(), 1);
        assert_eq!(read.departments[0].title, "Updated");
        assert!(read.problems.is_empty());
    }

    // Break caught: treating a non-directory read error as though nothing was configured.
    #[tokio::test]
    async fn department_directory_missing_is_not_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("departments");
        let files = DepartmentFiles::new(Some(path.clone()));
        let read = files.read().await;
        assert!(read.departments.is_empty() && read.problems.is_empty());
        fs::write(path, APPS).unwrap();
        let read = files.read().await;
        assert!(read.departments.is_empty());
        assert_eq!(read.problems.len(), 1);
        assert!(read.problems[0].starts_with("departments: cannot list directory"));
    }

    // Break caught: dropping error names or accepting oversized, nonregular, and non-UTF-8 inputs.
    #[tokio::test]
    async fn department_directory_bounds_and_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let exact = format!("{APPS}#{}", " ".repeat(16 * 1024 - APPS.len() - 1));
        fs::write(root.join("ok.toml"), &exact).unwrap();
        fs::write(root.join("oversize.toml"), format!("{exact} ")).unwrap();
        fs::write(root.join("encoding.toml"), [255]).unwrap();
        fs::write(
            root.join("bad.toml"),
            format!("{APPS}typo=1 # PRIVATE-SENTINEL\n"),
        )
        .unwrap();
        fs::write(root.join("Dot.toml"), APPS).unwrap();
        fs::write(
            root.join(std::ffi::OsString::from_vec(b"stem\xff.toml".to_vec())),
            APPS,
        )
        .unwrap();
        fs::create_dir(root.join("folder.toml")).unwrap();
        symlink(root.join("ok.toml"), root.join("alias.toml")).unwrap();
        let read = DepartmentFiles::new(Some(root.into())).read().await;
        assert_eq!(
            read.departments
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            ["ok"]
        );
        let problems = read.problems.join("\n");
        assert_eq!(read.problems.len(), 7, "{problems}");
        for name in [
            "oversize.toml",
            "encoding.toml",
            "folder.toml",
            "alias.toml",
            "Dot.toml",
            "stem�.toml",
        ] {
            assert!(problems.contains(name), "{problems}");
        }
        assert!(problems.contains("bad.toml line 4: unknown field `typo`"));
        assert!(!problems.contains("PRIVATE-SENTINEL"));
        assert!(!problems.contains(root.to_str().unwrap()));
    }

    // Break caught: unbounded enumeration or nondeterministic truncated results.
    #[tokio::test]
    async fn department_directory_caps_files_and_entries() {
        let dir = tempfile::tempdir().unwrap();
        for n in (0..33).rev() {
            fs::write(dir.path().join(format!("dept-{n:02}.toml")), APPS).unwrap();
        }
        let files = DepartmentFiles::new(Some(dir.path().into()));
        let read = files.read().await;
        assert_eq!(read.departments.len(), 32);
        assert_eq!(read.departments.first().unwrap().id.as_str(), "dept-00");
        assert_eq!(read.departments.last().unwrap().id.as_str(), "dept-31");
        assert_eq!(read.problems.len(), 1);
        assert!(read.problems[0].contains("32"));
        for n in 33..257 {
            fs::write(dir.path().join(format!("ignored-{n}.txt")), "").unwrap();
        }
        let read = files.read().await;
        assert!(read.departments.is_empty());
        assert_eq!(read.problems.len(), 1);
        assert!(read.problems[0].contains("256"));
    }
}
