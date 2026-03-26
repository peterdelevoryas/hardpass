use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;

pub(crate) struct FileLock {
    file: std::fs::File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub(crate) async fn lock_file(path: impl AsRef<Path>) -> Result<FileLock> {
    let path = path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || lock_file_blocking(&path)).await?
}

pub(crate) fn sibling_lock_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| {
            let mut name = OsString::from(name);
            name.push(".lock");
            name
        })
        .unwrap_or_else(|| OsString::from(".lock"));
    match path.parent() {
        Some(parent) => parent.join(file_name),
        None => PathBuf::from(file_name),
    }
}

fn lock_file_blocking(path: &Path) -> Result<FileLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create lock dir {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open lock file {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(FileLock { file })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Stdio;
    use std::time::Duration;

    use tempfile::tempdir;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;
    use tokio::time::timeout;

    use super::{lock_file, sibling_lock_path};

    #[test]
    fn sibling_lock_path_adds_lock_suffix() {
        assert_eq!(
            sibling_lock_path(Path::new("/tmp/dev")),
            Path::new("/tmp/dev.lock")
        );
    }

    #[tokio::test]
    async fn exclusive_lock_blocks_other_processes() {
        let dir = tempdir().expect("tempdir");
        let lock_path = dir.path().join("instance.lock");
        let lock = lock_file(&lock_path).await.expect("parent lock");

        let mut child = Command::new("python3")
            .arg("-c")
            .arg(
                r#"
import fcntl
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
path.parent.mkdir(parents=True, exist_ok=True)
with open(path, "a+") as fh:
    fcntl.flock(fh.fileno(), fcntl.LOCK_EX)
    print("locked", flush=True)
"#,
            )
            .arg(&lock_path)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn child");

        let stdout = child.stdout.take().expect("child stdout");
        let mut lines = BufReader::new(stdout).lines();

        assert!(
            timeout(Duration::from_millis(200), lines.next_line())
                .await
                .is_err(),
            "child acquired lock before parent released it"
        );

        drop(lock);

        let line = timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("child lock wait timed out")
            .expect("read child stdout")
            .expect("child line");
        assert_eq!(line, "locked");

        let status = timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("child exit wait timed out")
            .expect("wait for child");
        assert!(status.success());
    }
}
