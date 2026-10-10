use super::super::capture_store::{self, allocated};
use super::super::snapshot_lease::SnapshotLease;
use super::*;
use std::fs::{self, File};
#[cfg(test)]
use std::os::unix::fs::MetadataExt;

const MAX_ALLOCATED: u64 = 64 * 1024 * 1024 * 1024;
const MAX_CAPTURE_MOUNTS: usize = 21;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Manifest {
    version: u32,
    key: String,
    workdir: String,
    volumes: Vec<Volume>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Volume {
    guest_path: String,
    access: SandboxMountAccess,
    apfs: bool,
}

impl Manifest {
    pub(super) fn parse(payload: &SnapshotPayload) -> Result<Self> {
        ensure!(
            payload.format == SnapshotFormat::SmolvmMachinePack,
            "not a smolvm filesystem capture"
        );
        ensure!(
            payload.bytes.len() <= 1_048_576,
            "capture manifest exceeds its limit"
        );
        serde_json::from_slice::<Self>(&payload.bytes)?.validate()
    }

    fn validate(self) -> Result<Self> {
        ensure!(
            self.version == 1
                && self.key.len() == 32
                && self.key.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid smolvm capture identity"
        );
        ensure!(
            self.volumes.len() <= MAX_CAPTURE_MOUNTS,
            "too many captured volumes"
        );
        Ok(self)
    }
}

pub(super) struct Lease {
    pub directory: PathBuf,
    _lease: SnapshotLease,
}

pub(super) fn staging_directory(root: &Path) -> Result<(tempfile::TempDir, SnapshotLease)> {
    capture_store::staging_directory(root, &root.join("budget.lock"))
}

pub(super) fn open(root: &Path, manifest: &Manifest) -> Result<Lease> {
    let directory = root.join(&manifest.key);
    let lease = capture_store::read_lease(&directory)?;
    let saved: Manifest = capture_store::read_manifest(&directory)?;
    ensure!(
        saved == *manifest,
        "capture manifest differs from published state"
    );
    ensure!(
        directory.join("machine.smolmachine").is_file(),
        "filesystem pack is missing"
    );
    for index in 0..manifest.volumes.len() {
        ensure!(
            directory.join(format!("volume-{index}")).is_dir(),
            "captured mount is missing"
        );
    }
    Ok(Lease {
        directory,
        _lease: lease,
    })
}

fn clone_directory(source: &Path, target: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let source = CString::new(source.as_os_str().as_bytes())?;
        let target = CString::new(target.as_os_str().as_bytes())?;
        ensure!(
            unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } == 0,
            "mounted directory copies require APFS: {}",
            std::io::Error::last_os_error()
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        let output = std::process::Command::new("cp")
            .args(["-a", "--reflink=always", "--"])
            .arg(source)
            .arg(target)
            .output()?;
        ensure!(
            output.status.success(),
            "mounted directory copies require reflinks: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn volume_root(path: &Path) -> Option<&Path> {
    let root = path.parent()?.parent()?;
    (path.ends_with("mount/workspace") && root.join("volume.sparseimage").is_file()).then_some(root)
}

fn enforce_budget(root: &Path) -> Result<()> {
    ensure!(
        allocated(root)? <= MAX_ALLOCATED,
        "retained filesystem capture budget exhausted"
    );
    Ok(())
}

pub(super) fn publish(
    root: &Path,
    staging: tempfile::TempDir,
    request: &SandboxRequest,
) -> Result<SnapshotPayload> {
    enforce_budget(root)?;
    let mut volumes = Vec::new();
    ensure!(
        request.spec.mounts.len() <= MAX_CAPTURE_MOUNTS,
        "too many mounted volumes"
    );
    for (index, mount) in request.spec.mounts.iter().enumerate() {
        let target = staging.path().join(format!("volume-{index}"));
        let apfs = volume_root(&mount.host_path);
        if let Some(source) = apfs {
            fs::create_dir(&target)?;
            crate::local_volume::unmount_volume(source)?;
            let cloned = crate::local_volume::clone_volume(source, &target);
            crate::local_volume::mount_volume(source)?;
            cloned?;
        } else {
            clone_directory(&mount.host_path, &target)?;
        }
        volumes.push(Volume {
            guest_path: mount.guest_path.clone(),
            access: mount.access,
            apfs: apfs.is_some(),
        });
    }
    let manifest = Manifest {
        version: 1,
        key: uuid::Uuid::new_v4().simple().to_string(),
        workdir: request.spec.default_workdir.clone(),
        volumes,
    };
    let mut file = File::create(staging.path().join("manifest.json"))?;
    serde_json::to_writer(&mut file, &manifest)?;
    drop(file);
    for entry in fs::read_dir(staging.path())? {
        capture_store::sync_tree(&entry?.path())?;
    }
    capture_store::publish(
        staging.path(),
        &root.join(&manifest.key),
        &root.join("budget.lock"),
        || enforce_budget(root),
    )?;
    Ok(SnapshotPayload {
        format: SnapshotFormat::SmolvmMachinePack,
        bytes: serde_json::to_vec(&manifest)?.into(),
    })
}

pub(super) struct RestoredMounts {
    directories: Vec<(PathBuf, bool)>,
}

impl RestoredMounts {
    pub(super) fn keep(mut self) {
        self.directories.clear();
    }
}

impl Drop for RestoredMounts {
    fn drop(&mut self) {
        for (directory, apfs) in self.directories.iter().rev() {
            let cleanup = (|| {
                if *apfs {
                    crate::local_volume::unmount_volume(directory)?;
                    let image = directory.join("volume.sparseimage");
                    if image.try_exists()? {
                        fs::remove_file(image)?;
                    }
                    let mount = directory.join("mount");
                    if mount.try_exists()? {
                        fs::remove_dir(mount)?;
                    }
                } else if directory.try_exists()? {
                    fs::remove_dir_all(directory)?;
                }
                Ok::<_, anyhow::Error>(())
            })();
            if let Err(error) = cleanup {
                tracing::warn!(path = %directory.display(), %error, "failed to remove restored mount");
            }
        }
    }
}

pub(super) fn restore_mounts(
    directory: &Path,
    manifest: &Manifest,
    request: &mut SandboxRequest,
) -> Result<RestoredMounts> {
    ensure!(
        manifest.workdir == request.spec.default_workdir
            && manifest.volumes.len() == request.spec.mounts.len(),
        "filesystem capture layout changed"
    );
    let mut restored = RestoredMounts {
        directories: Vec::new(),
    };
    for (index, (volume, mount)) in manifest
        .volumes
        .iter()
        .zip(&mut request.spec.mounts)
        .enumerate()
    {
        ensure!(
            volume.guest_path == mount.guest_path && volume.access == mount.access,
            "mounted volume layout changed"
        );
        let source = directory.join(format!("volume-{index}"));
        fs::create_dir_all(&mount.host_path)?;
        let target = if volume.apfs {
            mount.host_path.clone()
        } else {
            mount.host_path.join("captured")
        };
        let owned_path = if volume.apfs {
            target.join("volume.sparseimage")
        } else {
            target.clone()
        };
        ensure!(
            !owned_path.try_exists()?,
            "restored mount target already exists"
        );
        restored.directories.push((target.clone(), volume.apfs));
        if volume.apfs {
            crate::local_volume::clone_volume(&source, &target)?;
            mount.host_path = crate::local_volume::mount_volume(&target)?;
        } else {
            clone_directory(&source, &target)?;
            mount.host_path = target;
        }
    }
    Ok(restored)
}

pub(super) fn delete(root: &Path, payload: &SnapshotPayload) -> Result<()> {
    capture_store::delete(&root.join(Manifest::parse(payload)?.key))
}

pub(super) fn collect(root: &Path, keep: &[SnapshotPayload]) -> Result<usize> {
    let keep = keep
        .iter()
        .map(|payload| Ok(root.join(Manifest::parse(payload)?.key)))
        .collect::<Result<_>>()?;
    if !root.try_exists()? {
        return Ok(0);
    }
    let _budget = capture_store::budget_lock(&root.join("budget.lock"))?;
    capture_store::collect(root, &keep, |directory| {
        let manifest: Manifest = capture_store::read_manifest(directory)?;
        Ok(root.join(manifest.validate()?.key))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires APFS disk images"]
    fn managed_apfs_volume_capture_remounts_source_and_restores_fresh_volume() -> Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        fs::create_dir(&source)?;
        crate::local_volume::create_sized_volume(&source, 1)?;
        let workspace = crate::local_volume::mount_volume(&source)?;
        fs::write(workspace.join("marker"), b"captured")?;
        let mut request = super::super::tests::test_request(Some(Duration::from_secs(60)));
        request.spec.mounts = vec![crate::SandboxMount {
            host_path: workspace.clone(),
            guest_path: "/repo".into(),
            access: SandboxMountAccess::ReadWrite,
            internal: false,
        }];
        let (staging, staging_lease) = staging_directory(root.path())?;
        fs::write(
            staging.path().join("machine.smolmachine"),
            b"unit-test pack",
        )?;
        let payload = publish(root.path(), staging, &request)?;
        drop(staging_lease);
        fs::write(workspace.join("marker"), b"later")?;
        let manifest = Manifest::parse(&payload)?;
        let lease = open(root.path(), &manifest)?;
        let target = root.path().join("target");
        request.spec.mounts[0].host_path = target.clone();
        let result = restore_mounts(&lease.directory, &manifest, &mut request);
        let captured = result.and_then(|mounts| {
            let bytes = fs::read(request.spec.mounts[0].host_path.join("marker"))?;
            mounts.keep();
            Ok(bytes)
        });
        let failed = root.path().join("failed");
        fs::create_dir(&failed)?;
        fs::write(failed.join("existing"), b"keep")?;
        request.spec.mounts[0].host_path = failed.clone();
        request.spec.mounts.push(crate::SandboxMount {
            host_path: root.path().join("missing"),
            guest_path: "/missing".into(),
            access: SandboxMountAccess::ReadWrite,
            internal: false,
        });
        let mut partial = manifest.clone();
        let mut missing = manifest.volumes[0].clone();
        missing.guest_path = "/missing".into();
        partial.volumes.push(missing);
        assert!(restore_mounts(&lease.directory, &partial, &mut request).is_err());
        assert!(!failed.join("volume.sparseimage").exists());
        assert!(!failed.join("mount").exists());
        assert_eq!(fs::read(failed.join("existing"))?, b"keep");
        crate::local_volume::unmount_volume(&source)?;
        crate::local_volume::unmount_volume(&target)?;
        assert_eq!(captured?, b"captured");
        drop(lease);
        delete(root.path(), &payload)?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn directory_captures_restore_independently_and_hold_deletion_leases() -> Result<()> {
        let root = tempfile::tempdir()?;
        let workspace = root.path().join("source");
        fs::create_dir(&workspace)?;
        fs::write(workspace.join("marker"), b"original")?;
        let (staging, staging_lease) = staging_directory(root.path())?;
        fs::write(
            staging.path().join("machine.smolmachine"),
            b"unit-test pack",
        )?;
        let mut request = super::super::tests::test_request(Some(Duration::from_secs(60)));
        request.spec.mounts = vec![crate::SandboxMount {
            host_path: workspace.clone(),
            guest_path: "/repo".into(),
            access: SandboxMountAccess::ReadWrite,
            internal: false,
        }];
        let payload = publish(root.path(), staging, &request)?;
        drop(staging_lease);
        let manifest = Manifest::parse(&payload)?;
        let lease = open(root.path(), &manifest)?;
        fs::write(workspace.join("marker"), b"later")?;
        let mut first = request.clone();
        first.spec.mounts[0].host_path = root.path().join("first");
        restore_mounts(&lease.directory, &manifest, &mut first)?.keep();
        fs::write(first.spec.mounts[0].host_path.join("marker"), b"edited")?;
        let mut second = request;
        second.spec.mounts[0].host_path = root.path().join("second");
        restore_mounts(&lease.directory, &manifest, &mut second)?.keep();
        assert_eq!(
            fs::read(second.spec.mounts[0].host_path.join("marker"))?,
            b"original"
        );
        assert!(delete(root.path(), &payload).is_err());
        drop(lease);
        delete(root.path(), &payload)?;
        delete(root.path(), &payload)?;
        Ok(())
    }

    fn capture_fixture(root: &Path) -> Result<SnapshotPayload> {
        let request = super::super::tests::test_request(Some(Duration::from_secs(60)));
        let (staging, _lease) = staging_directory(root)?;
        fs::write(
            staging.path().join("machine.smolmachine"),
            b"unit-test pack",
        )?;
        publish(root, staging, &request)
    }

    #[tokio::test]
    async fn filesystem_snapshot_allocated_bytes_counts_blocks_and_rejects_missing_captures()
    -> Result<()> {
        let root = tempfile::tempdir()?;
        let backend = SmolvmSandboxBackend {
            snapshot_root: root.path().into(),
            ..SmolvmSandboxBackend::new()
        };
        let payload = capture_fixture(root.path())?;
        let lease = open(root.path(), &Manifest::parse(&payload)?)?;
        let pack = File::options()
            .write(true)
            .open(lease.directory.join("machine.smolmachine"))?;
        pack.set_len(1024 * 1024 * 1024)?;
        pack.sync_all()?;
        let mut expected = fs::metadata(&lease.directory)?.blocks() * 512;
        for entry in fs::read_dir(&lease.directory)? {
            expected += entry?.metadata()?.blocks() * 512;
        }
        let bytes = backend
            .filesystem_snapshot_allocated_bytes(payload.clone())
            .await?;
        assert_eq!(bytes, expected);
        assert!(bytes > 0 && bytes < 1024 * 1024);
        drop(lease);
        backend.delete_snapshot(payload.clone()).await?;
        assert!(
            backend
                .filesystem_snapshot_allocated_bytes(payload)
                .await
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn collection_validates_manifest_keys_and_preserves_cataloged_captures() -> Result<()> {
        let root = tempfile::tempdir()?;
        let kept = capture_fixture(root.path())?;
        let invalid = capture_fixture(root.path())?;
        let mut manifest = Manifest::parse(&invalid)?;
        let directory = root.path().join(&manifest.key);
        manifest.key = "../outside".into();
        fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec(&manifest)?,
        )?;
        assert_eq!(collect(root.path(), std::slice::from_ref(&kept))?, 0);
        assert!(directory.exists());
        assert!(open(root.path(), &Manifest::parse(&kept)?).is_ok());
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn capture_preparation_does_not_wait_for_the_store_lock() -> Result<()> {
        let root = tempfile::tempdir()?;
        let captures = root.path().join("captures");
        let source = root.path().join("source");
        fs::create_dir(&source)?;
        fs::write(source.join("marker"), b"captured")?;
        let mut request = super::super::tests::test_request(Some(Duration::from_secs(60)));
        request.spec.mounts = vec![crate::SandboxMount {
            host_path: source,
            guest_path: "/repo".into(),
            access: SandboxMountAccess::ReadWrite,
            internal: false,
        }];
        let (staging, staging_lease) = staging_directory(&captures)?;
        fs::write(
            staging.path().join("machine.smolmachine"),
            b"unit-test pack",
        )?;
        let directory = staging.path().to_owned();
        let budget = capture_store::budget_lock(&captures.join("budget.lock"))?;
        let root_for_publish = captures.clone();
        let publisher = std::thread::spawn(move || {
            let _lease = staging_lease;
            publish(&root_for_publish, staging, &request)
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !directory.join("manifest.json").is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let prepared = directory.join("manifest.json").is_file();
        let copied = fs::read(directory.join("volume-0/marker"));
        drop(budget);
        let payload = publisher.join().expect("capture publisher panicked")?;
        assert!(prepared, "capture preparation waited for the store lock");
        assert_eq!(copied?, b"captured");
        assert!(open(&captures, &Manifest::parse(&payload)?).is_ok());
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn partial_mount_restore_removes_its_copies_and_preserves_existing_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join("volume-0"))?;
        fs::write(root.path().join("volume-0/marker"), b"captured")?;
        let mut request = super::super::tests::test_request(Some(Duration::from_secs(60)));
        let first = root.path().join("first");
        fs::create_dir(&first)?;
        fs::write(first.join("existing"), b"keep")?;
        request.spec.mounts = vec![
            crate::SandboxMount {
                host_path: first.clone(),
                guest_path: "/first".into(),
                access: SandboxMountAccess::ReadWrite,
                internal: false,
            },
            crate::SandboxMount {
                host_path: root.path().join("second"),
                guest_path: "/second".into(),
                access: SandboxMountAccess::ReadWrite,
                internal: false,
            },
        ];
        let manifest = Manifest {
            version: 1,
            key: "a".repeat(32),
            workdir: request.spec.default_workdir.clone(),
            volumes: vec![
                Volume {
                    guest_path: "/first".into(),
                    access: SandboxMountAccess::ReadWrite,
                    apfs: false,
                },
                Volume {
                    guest_path: "/second".into(),
                    access: SandboxMountAccess::ReadWrite,
                    apfs: false,
                },
            ],
        };
        assert!(restore_mounts(root.path(), &manifest, &mut request).is_err());
        assert!(!first.join("captured").exists());
        assert_eq!(fs::read(first.join("existing"))?, b"keep");
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn failed_vm_restore_removes_mount_copies_after_create_or_start_failure() -> Result<()> {
        for (failure, cleanup_fails) in [
            ("create", false),
            ("start", false),
            ("create", true),
            ("start", true),
        ] {
            let root = tempfile::tempdir()?;
            let delete = if cleanup_fails {
                "printf 'cleanup failure\\n' >&2; exit 24"
            } else {
                "exit 0"
            };
            let binary = root.path().join("smolvm");
            super::super::tests::write_test_binary(
                &binary,
                &format!(
                    r#"
case "$1 $2" in
  '--version ') printf 'smolvm 1.20.0\n';;
  'machine status') printf 'machine not found\n' >&2; exit 1;;
  'machine create')
    printf '%s\n' "$@" > "$(dirname "$0")/create-args"
    [ "{failure}" != create ] || {{ printf 'injected failure\n' >&2; exit 23; }};;
  'machine start')
    [ "{failure}" != start ] || {{ printf 'injected failure\n' >&2; exit 23; }};;
  'machine delete') {delete};;
  *) exit 0;;
esac"#
                ),
            );
            let backend = SmolvmSandboxBackend {
                snapshot_root: root.path().join("captures"),
                ..SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
                    binary: Some(binary),
                    mode: SmolvmExecutionMode::Warm,
                    ..Default::default()
                })
            };
            let (staging, _lease) = staging_directory(&backend.snapshot_root)?;
            fs::write(
                staging.path().join("machine.smolmachine"),
                b"unit-test pack",
            )?;
            let source = root.path().join("source");
            fs::create_dir(&source)?;
            fs::write(source.join("marker"), b"captured")?;
            let mut request = super::super::tests::test_request(Some(Duration::from_secs(60)));
            request.spec.mounts = vec![crate::SandboxMount {
                host_path: source,
                guest_path: "/repo".into(),
                access: SandboxMountAccess::ReadWrite,
                internal: false,
            }];
            let payload = publish(&backend.snapshot_root, staging, &request)?;
            drop(_lease);
            let target = root.path().join("target");
            fs::create_dir(&target)?;
            fs::write(target.join("existing"), b"keep")?;
            request.spec.mounts[0].host_path = target.clone();
            let error = backend
                .acquire_from_snapshot(request, payload)
                .await
                .err()
                .context("restore should fail")?;
            let args = fs::read_to_string(root.path().join("create-args"))?;
            assert!(args.contains("--from\n"), "{args}");
            assert!(!args.contains("--image\n"), "{args}");
            assert!(
                args.contains("--\n/bin/sh\n-c\ntrap 'kill -TERM -1; wait; exit 0' INT TERM;"),
                "{args}"
            );
            let message = format!("{error:#}");
            assert!(message.contains("injected failure"), "{message}");
            if cleanup_fails {
                assert!(message.contains("cleanup failure"), "{message}");
            }
            assert!(!target.join("captured").exists());
            assert_eq!(fs::read(target.join("existing"))?, b"keep");
        }
        Ok(())
    }
}
