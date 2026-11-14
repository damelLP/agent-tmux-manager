//! Procfs ownership checks shared by a rig and its external cleanup guardian.
use std::path::{Path, PathBuf};

pub fn daemon_identity(root: &Path, pid: u32) -> Option<String> {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    if std::fs::read_to_string(proc.join("comm")).ok()?.trim() != "atmd" {
        return None;
    }
    let env = std::fs::read(proc.join("environ")).ok()?;
    let owned = env
        .split(|b| *b == 0)
        .filter_map(|entry| {
            let (_, value) = std::str::from_utf8(entry).ok()?.split_once('=')?;
            Some(Path::new(value).starts_with(root))
        })
        .any(|owned| owned);
    if !owned {
        return None;
    }
    let stat = std::fs::read_to_string(proc.join("stat")).ok()?;
    Some(stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.into())
}

pub fn tmux_identity(root: &Path, pid: u32) -> Option<String> {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    if std::fs::read_to_string(proc.join("comm")).ok()?.trim() != "tmux: server" {
        return None;
    }
    let root = root.join("tmux").canonicalize().ok()?;
    let env = std::fs::read(proc.join("environ")).ok()?;
    let owns = env
        .split(|b| *b == 0)
        .filter_map(|entry| entry.strip_prefix(b"TMUX_TMPDIR="))
        .any(|path| {
            std::str::from_utf8(path)
                .ok()
                .and_then(|path| Path::new(path).canonicalize().ok())
                .as_ref()
                == Some(&root)
        });
    if !owns {
        return None;
    }
    let stat = std::fs::read_to_string(proc.join("stat")).ok()?;
    Some(stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.into())
}
