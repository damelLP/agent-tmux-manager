use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use super::push::{self, Endpoint, Key};
use anyhow::{ensure, Result};

const KEY: &str = ".vapid.pem";
const SLOT: &str = ".subscription";

fn load(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.permissions().mode() & 0o777 == 0o600,
        "notification state is not private"
    );
    let mut data = Vec::new();
    file.take(4097).read_to_end(&mut data)?;
    ensure!(data.len() <= 4096, "notification state too large");
    Ok(Some(data))
}

fn save(dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(dir.join(name)).map_err(|e| e.error)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

pub(super) fn open(dir: &Path, excluded: Vec<PathBuf>) -> Result<(Key, Option<Endpoint>)> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let meta = dir.symlink_metadata()?;
    ensure!(
        meta.is_dir() && meta.permissions().mode() & 0o777 == 0o700,
        "notification directory is not private"
    );
    let real = dir.canonicalize()?;
    for root in excluded {
        match root.canonicalize() {
            Ok(root) => ensure!(!real.starts_with(root), "notification directory is served"),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let key = match load(&dir.join(KEY))? {
        Some(pem) => Key::private_key_from_pem(&pem)?,
        None => {
            let key = push::new_key()?;
            // A subscription made with the lost key cannot accept the new one.
            set(dir, None)?;
            save(dir, KEY, &key.private_key_to_pem()?)?;
            key
        }
    };
    key.check_key()?;
    ensure!(
        key.group().curve_name() == Some(openssl::nid::Nid::X9_62_PRIME256V1),
        "wrong notification key curve"
    );
    let endpoint = load(&dir.join(SLOT))?
        .map(|bytes| Endpoint::parse(std::str::from_utf8(&bytes)?))
        .transpose()?;
    Ok((key, endpoint))
}

pub(super) fn set(dir: &Path, endpoint: Option<Endpoint>) -> Result<()> {
    if let Some(ep) = endpoint {
        return save(dir, SLOT, ep.as_str().as_bytes());
    }
    match fs::remove_file(dir.join(SLOT)) {
        Ok(()) => File::open(dir)?.sync_all()?,
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
