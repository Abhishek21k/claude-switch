use anyhow::Result;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Replace one file only after its complete contents have reached the filesystem.
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
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
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
