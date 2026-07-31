//! Crash-safe filesystem persistence shared by Medusa state stores.
//!
//! Writes land in a same-directory temporary file, are flushed to disk, and
//! are atomically renamed over the destination. Keeping the temporary file in
//! the destination directory avoids cross-filesystem rename failures.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use color_eyre::eyre::{Result, WrapErr};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically replace `path`, preserving existing file permissions.
pub fn atomic_write(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    atomic_write_inner(path, contents.as_ref(), false)
}

/// Atomically replace private Medusa state and restrict new files to the owner.
pub fn atomic_write_private(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    atomic_write_inner(path, contents.as_ref(), true)
}

/// Create a private state directory. Existing directories are tightened on
/// Unix because transcripts and approval grants can contain sensitive data.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .wrap_err_with(|| format!("failed to create private directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .wrap_err_with(|| format!("failed to secure directory {}", path.display()))?;
    }
    Ok(())
}

fn atomic_write_inner(path: &Path, contents: &[u8], private: bool) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if private {
        ensure_private_dir(parent)?;
    } else {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create directory {}", parent.display()))?;
    }

    let temp = temporary_path(path);
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options
            .open(&temp)
            .wrap_err_with(|| format!("failed to create temporary file {}", temp.display()))?;

        apply_permissions(path, &file, private)?;
        file.write_all(contents)
            .wrap_err_with(|| format!("failed to write temporary file {}", temp.display()))?;
        file.flush()
            .wrap_err_with(|| format!("failed to flush temporary file {}", temp.display()))?;
        file.sync_all()
            .wrap_err_with(|| format!("failed to sync temporary file {}", temp.display()))?;
        drop(file);

        fs::rename(&temp, path).wrap_err_with(|| {
            format!(
                "failed to atomically replace {} with {}",
                path.display(),
                temp.display()
            )
        })?;

        // A directory fsync makes the rename durable on Unix filesystems.
        // Some filesystems reject directory sync, so the data-file sync above
        // remains the hard requirement and directory sync is best effort.
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("medusa-state"))
        .to_string_lossy();
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{file_name}.tmp-{}-{nonce}", std::process::id()))
}

fn apply_permissions(path: &Path, file: &File, private: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if private {
            0o600
        } else {
            fs::metadata(path)
                .map(|metadata| metadata.permissions().mode())
                .unwrap_or(0o666)
        };
        file.set_permissions(fs::Permissions::from_mode(mode))
            .wrap_err_with(|| format!("failed to set permissions on {}", path.display()))?;
    }

    #[cfg(not(unix))]
    {
        let _ = (path, file, private);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "medusa-persistence-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn atomic_write_replaces_content_without_leaving_temp_files() {
        let dir = temp_dir("replace");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        fs::write(&path, "old").unwrap();

        atomic_write(&path, b"new").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn private_writes_restrict_file_and_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("private").join("nested");
        let path = dir.join("session.json");
        atomic_write_private(&path, b"secret").unwrap();

        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn regular_writes_preserve_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("permissions");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("script.sh");
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        atomic_write(&path, b"#!/bin/sh\necho ok\n").unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
