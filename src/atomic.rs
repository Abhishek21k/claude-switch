use anyhow::Result;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Replace one file only after its complete contents have reached the filesystem.
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    write_inner(path, bytes, true).map(|_| ())
}

/// Publish a complete new file, leaving an existing file untouched even in a race.
pub fn write_once(path: &Path, bytes: &[u8]) -> Result<bool> {
    write_inner(path, bytes, false)
}

/// Create a private temporary file before any bytes are written to it.
pub fn create_private_temp(dir: &Path) -> Result<(PathBuf, fs::File)> {
    fs::create_dir_all(dir)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0.. {
        let path = dir.join(format!(
            ".cswitch.{}.{}.{}.tmp",
            std::process::id(),
            stamp,
            index
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!()
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().expect("file has a parent");
    let (temporary, mut file) = create_private_temp(parent)?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn write_inner(path: &Path, bytes: &[u8], replace: bool) -> Result<bool> {
    let parent = path.parent().expect("file has a parent");
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let mut index = 0;
    let (temporary, mut file) = loop {
        let candidate = parent.join(format!(
            ".{}.{}.{}.{}.tmp",
            path.file_name().unwrap().to_string_lossy(),
            std::process::id(),
            stamp,
            index
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => index += 1,
            Err(error) => return Err(error.into()),
        }
    };
    let result = (|| -> Result<bool> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if replace {
            fs::rename(&temporary, path)?;
            Ok(true)
        } else {
            match fs::hard_link(&temporary, path) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(error) => Err(error.into()),
            }
        }
    })();
    let _ = fs::remove_file(temporary);
    result
}
