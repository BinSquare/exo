use super::snapshot_lease::SnapshotLease;
use anyhow::{Context, Result, bail, ensure};
use serde::de::DeserializeOwned;
use std::collections::HashSet;
use std::fs::{self, File, TryLockError};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(super) fn budget_lock(path: &Path) -> Result<SnapshotLease> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    Ok(SnapshotLease::exclusive(file)?)
}

pub(super) fn staging_directory(
    parent: &Path,
    budget_file: &Path,
) -> Result<(tempfile::TempDir, SnapshotLease)> {
    fs::create_dir_all(parent)?;
    let _budget = budget_lock(budget_file)?;
    let directory = tempfile::Builder::new()
        .prefix(".capture-")
        .tempdir_in(parent)?;
    let file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.path().join("lease"))?;
    Ok((directory, SnapshotLease::exclusive(file)?))
}

pub(super) fn read_lease(directory: &Path) -> Result<SnapshotLease> {
    Ok(SnapshotLease::shared(
        File::open(directory.join("lease"))
            .context("filesystem capture is missing on this host")?,
    )?)
}

pub(super) fn read_manifest<M: DeserializeOwned>(directory: &Path) -> Result<M> {
    let file = File::open(directory.join("manifest.json"))?;
    ensure!(
        file.metadata()?.len() <= 1_048_576,
        "capture manifest exceeds its limit"
    );
    Ok(serde_json::from_reader(file)?)
}

pub(super) fn directories(root: &Path) -> Result<impl Iterator<Item = Result<PathBuf>>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok(entries.into_iter().flatten().filter_map(|entry| {
        let directory = (|| {
            let entry = entry?;
            Ok::<_, std::io::Error>(entry.file_type()?.is_dir().then(|| entry.path()))
        })();
        match directory {
            Ok(directory) => directory.map(Ok),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(Err(error.into())),
        }
    }))
}

pub(super) fn delete(directory: &Path) -> Result<()> {
    ensure!(
        try_delete(directory)? || !directory.try_exists()?,
        "filesystem capture is in use"
    );
    Ok(())
}

fn try_delete(directory: &Path) -> Result<bool> {
    let file = match File::open(directory.join("lease")) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let _lease = match file.map(SnapshotLease::try_exclusive).transpose() {
        Ok(lease) => lease,
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    };
    match fs::remove_dir_all(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    File::open(directory.parent().context("missing capture parent")?)?.sync_all()?;
    Ok(true)
}

pub(super) fn collect(
    root: &Path,
    keep: &HashSet<PathBuf>,
    directory_for_manifest: impl Fn(&Path) -> Result<PathBuf>,
) -> Result<usize> {
    let mut removed = 0;
    for directory in directories(root)? {
        let directory = directory?;
        if directory
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".capture-"))
        {
            removed += usize::from(try_delete(&directory)?);
            continue;
        }
        let valid = directory_for_manifest(&directory).and_then(|expected| {
            ensure!(
                expected == directory,
                "capture directory differs from manifest"
            );
            Ok(())
        });
        match valid {
            Ok(()) if !keep.contains(&directory) => removed += usize::from(try_delete(&directory)?),
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(path = %directory.display(), %error, "skipping invalid filesystem capture")
            }
        }
    }
    Ok(removed)
}

pub(super) fn allocated(root: &Path) -> Result<u64> {
    walk_allocated(root, &mut HashSet::new(), &mut 0, false)
}

#[cfg(feature = "firecracker")]
pub(super) fn allocated_files(root: &Path, inodes: &mut HashSet<(u64, u64)>) -> Result<u64> {
    walk_allocated(root, inodes, &mut 0, true)
}

fn walk_allocated(
    root: &Path,
    inodes: &mut HashSet<(u64, u64)>,
    count: &mut usize,
    files_only: bool,
) -> Result<u64> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if !files_only {
        *count += 1;
        ensure!(*count <= 262_144, "capture file count exceeds its limit");
    }
    if files_only && !metadata.is_dir() && !metadata.is_file() {
        bail!("unexpected file in snapshot storage");
    }
    let mut bytes =
        if (!files_only || metadata.is_file()) && inodes.insert((metadata.dev(), metadata.ino())) {
            metadata
                .blocks()
                .checked_mul(512)
                .context("snapshot size overflow")?
        } else {
            0
        };
    if metadata.is_dir() {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            bytes = bytes
                .checked_add(walk_allocated(&entry.path(), inodes, count, files_only)?)
                .context("snapshot size overflow")?;
        }
    }
    Ok(bytes)
}

pub(super) fn sync_tree(root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(root)? {
            sync_tree(&entry?.path())?;
        }
    }
    if !metadata.file_type().is_symlink() {
        File::open(root)?.sync_all()?;
    }
    Ok(())
}

pub(super) fn publish(
    temporary: &Path,
    destination: &Path,
    budget_file: &Path,
    check_budget: impl FnOnce() -> Result<()>,
) -> Result<()> {
    File::open(temporary)?.sync_all()?;
    {
        let _budget = budget_lock(budget_file)?;
        check_budget()?;
        fs::rename(temporary, destination)
            .with_context(|| format!("publishing capture {}", destination.display()))?;
    }
    let parent = destination.parent().context("missing capture parent")?;
    File::open(parent)?.sync_all()?;
    if let Some(namespace) = parent.parent().filter(|path| !path.as_os_str().is_empty()) {
        File::open(namespace)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collection_skips_busy_and_corrupt_captures_and_reclaims_abandoned_staging() -> Result<()> {
        let root = tempfile::tempdir()?;
        for key in ["kept", "busy", "stale"] {
            let directory = root.path().join(key);
            fs::create_dir(&directory)?;
            File::create(directory.join("lease"))?;
            fs::write(directory.join("manifest.json"), serde_json::to_vec(key)?)?;
        }
        let kept = root.path().join("kept");
        let busy = root.path().join("busy");
        let stale = root.path().join("stale");
        let busy_lease = read_lease(&busy)?;
        let (active, active_lease) =
            staging_directory(root.path(), &root.path().join("budget.lock"))?;
        assert_eq!(
            fs::metadata(active.path().join("lease"))?.mode() & 0o777,
            0o600
        );
        let incomplete = root.path().join(".capture-incomplete");
        fs::create_dir(&incomplete)?;
        fs::write(incomplete.join("data"), b"incomplete")?;
        let unpublished = root.path().join(".capture-unpublished");
        fs::create_dir(&unpublished)?;
        fs::write(unpublished.join("manifest.json"), b"\"unpublished\"")?;
        let corrupt = root.path().join("corrupt");
        fs::create_dir(&corrupt)?;
        fs::write(corrupt.join("manifest.json"), b"invalid JSON")?;
        let keep = HashSet::from([kept.clone()]);
        let directory_for =
            |directory: &Path| Ok(root.path().join(read_manifest::<String>(directory)?));
        assert!(delete(&busy).is_err());
        assert_eq!(collect(root.path(), &keep, directory_for)?, 3);
        assert!(busy.exists());
        assert!(active.path().exists());
        assert!(corrupt.exists());
        assert!(!stale.exists());
        assert!(!incomplete.exists());
        assert!(!unpublished.exists());
        drop(busy_lease);
        drop(active_lease);
        assert_eq!(collect(root.path(), &keep, directory_for)?, 2);
        assert!(read_lease(&kept).is_ok());
        delete(&kept)?;
        delete(&kept)?;
        Ok(())
    }

    #[test]
    fn allocated_walk_tolerates_concurrent_directory_removal() -> Result<()> {
        let root = tempfile::tempdir()?;
        for round in 0..8 {
            let capture = root.path().join("capture");
            fs::create_dir(&capture)?;
            for index in 0..128 {
                let directory = capture.join(index.to_string());
                fs::create_dir(&directory)?;
                fs::write(directory.join("disk"), b"data")?;
            }
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                let delete = scope.spawn(|| {
                    barrier.wait();
                    fs::remove_dir_all(&capture)
                });
                barrier.wait();
                let walked = if round % 2 == 0 {
                    allocated(root.path())
                } else {
                    walk_allocated(root.path(), &mut HashSet::new(), &mut 0, true)
                };
                delete.join().expect("capture deletion panicked")?;
                walked?;
                Ok::<_, anyhow::Error>(())
            })?;
        }
        Ok(())
    }
}
