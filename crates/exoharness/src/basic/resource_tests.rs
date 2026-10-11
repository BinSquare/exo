use super::*;
use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxRequest, SnapshotFormat, SnapshotPayload,
};
use std::time::Duration;
use tokio::task::JoinHandle;

use crate::resources::{
    MaterializeResourcesRequest, PreparedResource, ResourceDefinition, ResourceSource,
};
use crate::test_support::local_test_config;
use crate::{EnvironmentDefinition, FileSystemMountMode};
use tokio::sync::Semaphore;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(5);

struct ResourceBackend {
    entered: mpsc::UnboundedSender<(ConversationId, bool)>,
    release: Semaphore,
    materialized: Mutex<HashSet<ConversationId>>,
    deleted_snapshots: Mutex<Vec<bytes::Bytes>>,
    fail_snapshot_deletion: std::sync::atomic::AtomicBool,
    capture_gate: Mutex<Option<Arc<CaptureGate>>>,
}

#[async_trait]
impl ManagedSandboxBackend for ResourceBackend {
    async fn materialize_resources(
        &self,
        request: MaterializeResourcesRequest,
    ) -> Result<Vec<FileSystemMount>> {
        self.entered.send((request.thread, request.resume))?;
        self.release.acquire().await?.forget();
        self.materialized.lock().unwrap().insert(request.thread);
        Ok(Vec::new())
    }

    async fn remove_thread_resources(&self, _agent: AgentId, thread: ConversationId) -> Result<()> {
        assert!(self.materialized.lock().unwrap().remove(&thread));
        Ok(())
    }

    fn is_local(&self) -> bool {
        false
    }

    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        static FORMATS: [SnapshotFormat; 1] = [SnapshotFormat::FirecrackerFilesystemRef];
        &FORMATS
    }

    async fn delete_snapshot(&self, payload: SnapshotPayload) -> Result<()> {
        if self
            .fail_snapshot_deletion
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            bail!("capture is busy");
        }
        self.deleted_snapshots.lock().unwrap().push(payload.bytes);
        Ok(())
    }

    async fn acquire(&self, _request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        Ok(Arc::new(CaptureHandle {
            gate: self.capture_gate.lock().unwrap().take(),
        }))
    }

    async fn terminate(&self, _request: SandboxRequest) -> Result<()> {
        Ok(())
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("resource test does not attach VMs")
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        assert_eq!(payload.format, SnapshotFormat::FirecrackerFilesystemRef);
        assert_eq!(payload.bytes, bytes::Bytes::from_static(b"capture"));
        Ok(Arc::new(CaptureHandle { gate: None }))
    }
}

struct CaptureGate {
    entered: tokio::sync::Notify,
    release: Semaphore,
}

struct CaptureHandle {
    gate: Option<Arc<CaptureGate>>,
}

#[async_trait]
impl ManagedSandboxHandle for CaptureHandle {
    fn id(&self) -> &str {
        "capture-test"
    }

    async fn exec(&self, _command: &crate::SandboxCommand) -> Result<crate::SandboxCommandOutput> {
        bail!("capture test does not execute commands")
    }

    async fn start_process(
        &self,
        _command: &crate::SandboxCommand,
    ) -> Result<crate::SandboxProcessParts> {
        bail!("capture test does not start processes")
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("capture test does not attach VMs")
    }

    async fn snapshot(&self, _kind: crate::SnapshotKind) -> Result<SnapshotPayload> {
        if let Some(gate) = &self.gate {
            gate.entered.notify_one();
            gate.release.acquire().await?.forget();
        }
        Ok(SnapshotPayload {
            format: SnapshotFormat::FirecrackerFilesystemRef,
            bytes: bytes::Bytes::from_static(b"capture"),
        })
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    harness: BasicExoHarness,
    agent: Arc<dyn AgentHandle>,
    backend: Arc<ResourceBackend>,
    entered: mpsc::UnboundedReceiver<(ConversationId, bool)>,
}

// Unblock detached materializers even if an assertion fails.
impl Drop for Fixture {
    fn drop(&mut self) {
        self.backend.release.close();
    }
}

impl Fixture {
    async fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let (entered, receiver) = mpsc::unbounded_channel();
        let backend = Arc::new(ResourceBackend {
            entered,
            release: Semaphore::new(0),
            materialized: Mutex::new(HashSet::new()),
            deleted_snapshots: Mutex::new(Vec::new()),
            fail_snapshot_deletion: std::sync::atomic::AtomicBool::new(false),
            capture_gate: Mutex::new(None),
        });
        let mut config = local_test_config(directory.path());
        config
            .sandbox_backends
            .push(SandboxBackendRegistration::from_backend(
                SandboxProvider::Firecracker,
                backend.clone(),
            ));
        let harness = BasicExoHarness::new(config).await?;
        let agent = harness
            .new_agent(NewAgentRequest {
                slug: "resources".into(),
                name: "Resources".into(),
                vaults: vec![],
            })
            .await?;
        Ok(Self {
            directory,
            harness,
            agent,
            backend,
            entered: receiver,
        })
    }

    async fn thread(&self) -> Result<Arc<dyn ConversationHandle>> {
        timeout(DEADLINE, self.agent.new_conversation(Default::default())).await?
    }

    async fn sandbox(&self) -> Result<SandboxId> {
        self.agent.create_sandbox(capture_spec()).await
    }

    fn resources(&self) -> Vec<PreparedResource> {
        // The test backend gates materialization without cloning a repo or disk.
        vec![PreparedResource {
            definition: ResourceDefinition {
                name: "workspace".into(),
                mount_path: "/workspace".into(),
                mode: FileSystemMountMode::ReadWrite,
                source: ResourceSource::Directory {
                    path: self.directory.path().to_owned(),
                },
            },
            snapshot: None,
        }]
    }

    fn prepare(
        &self,
        thread: Arc<dyn ConversationHandle>,
    ) -> JoinHandle<Result<Vec<FileSystemMount>>> {
        let resources = self.resources();
        tokio::spawn(async move {
            thread
                .materialize_resources(resources, SandboxProvider::Firecracker)
                .await
        })
    }

    async fn next(&mut self) -> Result<(ConversationId, bool)> {
        timeout(DEADLINE, self.entered.recv())
            .await?
            .context("materializer did not start")
    }

    fn resources_exist(&self, thread: ConversationId) -> bool {
        self.directory
            .path()
            .join("resources/threads")
            .join(self.agent.record().id.to_string())
            .join(thread.to_string())
            .exists()
    }
}

async fn write_metadata(thread: &dyn ConversationHandle) -> Result<()> {
    timeout(
        DEADLINE,
        thread.write_artifact(WriteArtifactRequest {
            path: "config/test.json".into(),
            contents: b"{}".to_vec(),
        }),
    )
    .await??;
    Ok(())
}

async fn assert_pending<T>(task: &mut JoinHandle<T>) {
    assert!(
        timeout(Duration::from_millis(50), task).await.is_err(),
        "operation raced resource preparation"
    );
}

#[tokio::test]
async fn resource_preparation_allows_metadata_writes_and_other_threads() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let first = fixture.thread().await?;
    let preparation = fixture.prepare(first.clone());
    assert_eq!(fixture.next().await?, (first.record().id, false));

    write_metadata(first.as_ref()).await?;
    let second = fixture.thread().await?;
    let other_preparation = fixture.prepare(second.clone());
    assert_eq!(fixture.next().await?, (second.record().id, false));

    fixture.backend.release.add_permits(2);
    timeout(DEADLINE, preparation).await???;
    timeout(DEADLINE, other_preparation).await???;
    Ok(())
}

#[tokio::test]
async fn resource_preparation_serializes_the_same_thread_and_rechecks_resume() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let thread = fixture.thread().await?;
    let preparation = fixture.prepare(thread.clone());
    assert_eq!(fixture.next().await?, (thread.record().id, false));
    let mut resumed = fixture.prepare(thread.clone());
    assert_pending(&mut resumed).await;
    assert!(fixture.entered.try_recv().is_err());
    write_metadata(thread.as_ref()).await?;

    fixture.backend.release.add_permits(1);
    timeout(DEADLINE, preparation).await???;
    assert_eq!(fixture.next().await?, (thread.record().id, true));
    fixture.backend.release.add_permits(1);
    timeout(DEADLINE, resumed).await???;
    Ok(())
}

#[tokio::test]
async fn thread_deletion_waits_for_cancelled_preparation_and_removes_resources() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let thread = fixture.thread().await?;
    let id = thread.record().id;
    let preparation = fixture.prepare(thread.clone());
    assert_eq!(fixture.next().await?, (id, false));
    preparation.abort();
    assert!(preparation.await.unwrap_err().is_cancelled());

    let agent = fixture.agent.clone();
    let mut deletion = tokio::spawn(async move { agent.delete_conversation(&id).await });
    assert_pending(&mut deletion).await;
    let other = fixture.thread().await?;
    write_metadata(other.as_ref()).await?;

    fixture.backend.release.add_permits(1);
    assert!(timeout(DEADLINE, deletion).await???);
    assert!(fixture.agent.get_conversation(&id).await?.is_none());
    assert!(!fixture.resources_exist(id));
    assert!(fixture.backend.materialized.lock().unwrap().is_empty());

    assert!(
        thread
            .materialize_resources(fixture.resources(), SandboxProvider::Firecracker)
            .await
            .is_err()
    );
    assert!(!fixture.resources_exist(id));
    assert!(fixture.entered.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn agent_deletion_waits_for_all_preparation_without_blocking_other_agents() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let first = fixture.thread().await?;
    let second = fixture.thread().await?;
    let mut first_preparation = fixture.prepare(first.clone());
    assert_eq!(fixture.next().await?, (first.record().id, false));
    let mut second_preparation = fixture.prepare(second.clone());
    assert_eq!(fixture.next().await?, (second.record().id, false));

    let harness = fixture.harness.clone();
    let id = fixture.agent.record().id;
    let mut deletion = tokio::spawn(async move { harness.delete_agent(&id).await });
    assert_pending(&mut deletion).await;
    timeout(
        DEADLINE,
        fixture.harness.new_agent(NewAgentRequest {
            slug: "other".into(),
            name: "Other".into(),
            vaults: vec![],
        }),
    )
    .await??;

    fixture.backend.release.add_permits(1);
    let first_completed = timeout(DEADLINE, async {
        tokio::select! {
            result = &mut first_preparation => { result??; Ok::<_, anyhow::Error>(true) }
            result = &mut second_preparation => { result??; Ok(false) }
        }
    })
    .await??;
    assert_pending(&mut deletion).await;
    fixture.backend.release.add_permits(1);
    if first_completed {
        timeout(DEADLINE, second_preparation).await???;
    } else {
        timeout(DEADLINE, first_preparation).await???;
    }
    assert!(timeout(DEADLINE, deletion).await???);
    assert!(fixture.harness.get_agent(&id).await?.is_none());
    assert!(!fixture.resources_exist(first.record().id));
    assert!(!fixture.resources_exist(second.record().id));
    assert!(fixture.backend.materialized.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn environment_changes_and_forks_wait_for_resource_preparation() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let thread = fixture.thread().await?;
    let preparation = fixture.prepare(thread.clone());
    assert_eq!(fixture.next().await?, (thread.record().id, false));

    let changing = thread.clone();
    let mut update = tokio::spawn(async move {
        changing
            .update_environment(EnvironmentDefinition {
                name: "local".into(),
                config: CreateSandboxRequest {
                    provider: SandboxProvider::LocalProcess,
                    image: "test".into(),
                    tcp_ports: vec![],
                    name: None,
                    resources: None,
                    default_workdir: None,
                    file_system_mounts: None,
                    durable_file_systems: None,
                    policy: None,
                    enable_networking: Some(true),
                    idle_seconds: None,
                },
            })
            .await
    });
    let forking = thread.clone();
    let mut fork = tokio::spawn(async move { forking.fork(Default::default()).await });
    assert_pending(&mut update).await;
    assert_pending(&mut fork).await;
    write_metadata(thread.as_ref()).await?;

    fixture.backend.release.add_permits(1);
    timeout(DEADLINE, preparation).await???;
    assert!(timeout(DEADLINE, update).await??.is_err());
    assert!(timeout(DEADLINE, fork).await??.is_err());
    Ok(())
}

fn capture_spec() -> CreateSandboxRequest {
    CreateSandboxRequest {
        name: None,
        provider: SandboxProvider::Firecracker,
        image: "test".into(),
        resources: None,
        default_workdir: None,
        file_system_mounts: None,
        durable_file_systems: None,
        policy: None,
        enable_networking: None,
        idle_seconds: None,
        tcp_ports: Vec::new(),
    }
}

async fn seed_snapshot(
    storage: &BasicObjectStore,
    owner_dir: &Path,
    contents: Option<&[u8]>,
) -> Result<PathBuf> {
    let snapshot_id = Uuid7::now();
    let directory = owner_dir.join("snapshots").join(snapshot_id.to_string());
    storage
        .put_json(
            directory.join("manifest.json"),
            &StoredSnapshotManifest {
                snapshot_id,
                sandbox_id: "previously-terminated".into(),
                format: SnapshotFormat::FirecrackerFilesystemRef,
                provider: Some(SandboxProvider::Firecracker),
                created_at: chrono::Utc::now(),
                payload_size_bytes: contents.map_or(1, |bytes| bytes.len() as u64),
                payload: None,
            },
        )
        .await?;
    let payload = directory.join("payload.bin");
    if let Some(contents) = contents {
        storage.put_bytes(&payload, contents.to_vec()).await?;
    }
    Ok(payload)
}

#[tokio::test]
async fn owner_deletion_releases_snapshots_and_preserves_references_on_failure() -> Result<()> {
    let fixture = Fixture::new().await?;
    let harness = &fixture.harness;
    let agent = &fixture.agent;
    let backend = &fixture.backend;
    let storage = &harness.inner.storage;
    let first = fixture.thread().await?;
    let second = fixture.thread().await?;
    let mut snapshot_paths = Vec::new();
    for (owner, contents) in [
        (
            ResourceScope::Agent {
                agent_id: agent.record().id,
            },
            b"agent".as_slice(),
        ),
        (
            ResourceScope::Thread {
                agent_id: agent.record().id,
                thread_id: first.record().id,
            },
            b"first".as_slice(),
        ),
        (
            ResourceScope::Thread {
                agent_id: agent.record().id,
                thread_id: second.record().id,
            },
            b"second".as_slice(),
        ),
    ] {
        let directory = harness.owner_dir(owner);
        snapshot_paths.push(seed_snapshot(storage, &directory, Some(contents)).await?);
        seed_snapshot(storage, &directory, None).await?;
    }
    backend
        .fail_snapshot_deletion
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(agent.delete_conversation(&first.record().id).await.is_err());
    assert!(storage.get_bytes(&snapshot_paths[1]).await.is_ok());
    assert!(agent.get_conversation(&first.record().id).await?.is_some());
    backend
        .fail_snapshot_deletion
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(agent.delete_conversation(&first.record().id).await?);
    assert_eq!(
        *backend.deleted_snapshots.lock().unwrap(),
        vec![bytes::Bytes::from_static(b"first")]
    );
    assert!(storage.get_bytes(&snapshot_paths[0]).await.is_ok());
    assert!(storage.get_bytes(&snapshot_paths[2]).await.is_ok());
    assert!(harness.delete_agent(&agent.record().id).await?);
    let mut deleted = backend.deleted_snapshots.lock().unwrap().clone();
    deleted.sort();
    assert_eq!(
        deleted,
        vec![
            bytes::Bytes::from_static(b"agent"),
            bytes::Bytes::from_static(b"first"),
            bytes::Bytes::from_static(b"second")
        ]
    );
    Ok(())
}

#[tokio::test]
async fn snapshot_publication_failure_cleans_capture_or_retains_cleanup_reference() -> Result<()> {
    for (fail_after, fail_cleanup) in [(0, false), (1, false), (0, true), (1, true)] {
        let fixture = Fixture::new().await?;
        let harness = &fixture.harness;
        let agent = &fixture.agent;
        let backend = &fixture.backend;
        let storage = &harness.inner.storage;
        let sandbox_id = fixture.sandbox().await?;
        backend
            .fail_snapshot_deletion
            .store(fail_cleanup, std::sync::atomic::Ordering::Relaxed);
        storage.fail_json_put_after(fail_after);
        let error = agent
            .snapshot_sandbox(sandbox_id, crate::SnapshotKind::Filesystem)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("injected JSON object write failure"));
        let owner_dir = harness.owner_dir(ResourceScope::Agent {
            agent_id: agent.record().id,
        });
        let keys = storage.list_keys(owner_dir.join("snapshots")).await?;
        assert_eq!(keys.len(), usize::from(fail_cleanup || fail_after == 1));
        backend
            .fail_snapshot_deletion
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(harness.delete_agent(&agent.record().id).await?);
        assert_eq!(
            *backend.deleted_snapshots.lock().unwrap(),
            vec![bytes::Bytes::from_static(b"capture")]
        );
    }
    Ok(())
}

#[tokio::test]
async fn capture_reference_is_published_atomically_and_loads_without_payload_blob() -> Result<()> {
    let fixture = Fixture::new().await?;
    let harness = &fixture.harness;
    let agent = &fixture.agent;
    let backend = &fixture.backend;
    let storage = &harness.inner.storage;
    let sandbox_id = fixture.sandbox().await?;
    let snapshot_id = agent
        .snapshot_sandbox(sandbox_id, crate::SnapshotKind::Filesystem)
        .await?;
    let owner_dir = harness.owner_dir(ResourceScope::Agent {
        agent_id: agent.record().id,
    });
    let snapshot_dir = owner_dir.join("snapshots").join(snapshot_id.to_string());
    assert_eq!(storage.list_keys(&snapshot_dir).await?.len(), 1);
    assert!(
        storage
            .get_bytes_if_exists(snapshot_dir.join("payload.bin"))
            .await?
            .is_none()
    );
    let payload = load_snapshot_payload(harness, &owner_dir, snapshot_id).await?;
    assert_eq!(payload.format, SnapshotFormat::FirecrackerFilesystemRef);
    assert_eq!(payload.bytes, bytes::Bytes::from_static(b"capture"));
    assert!(harness.delete_agent(&agent.record().id).await?);
    assert_eq!(
        *backend.deleted_snapshots.lock().unwrap(),
        vec![payload.bytes]
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_snapshot_publishes_a_visible_event_with_its_capture() -> Result<()> {
    for (on_turn, finish_turn, advance_turn) in [
        (false, false, false),
        (true, false, false),
        (true, false, true),
        (true, true, false),
        (true, true, true),
    ] {
        let fixture = Fixture::new().await?;
        let thread = fixture.thread().await?;
        let harness = &fixture.harness;
        let agent = &fixture.agent;
        let backend = &fixture.backend;
        let gate = Arc::new(CaptureGate {
            entered: tokio::sync::Notify::new(),
            release: Semaphore::new(0),
        });
        *backend.capture_gate.lock().unwrap() = Some(gate.clone());
        let sandbox = thread.create_sandbox(capture_spec()).await?;
        let (snapshotting, original_turn) = if on_turn {
            let turn = crate::test_support::begin_test_turn(thread.as_ref()).await?;
            let capturing = turn.clone();
            (
                tokio::spawn(async move {
                    capturing
                        .snapshot_sandbox(sandbox, crate::SnapshotKind::Filesystem)
                        .await
                }),
                Some(turn),
            )
        } else {
            let thread = thread.clone();
            (
                tokio::spawn(async move {
                    thread
                        .snapshot_sandbox(sandbox, crate::SnapshotKind::Filesystem)
                        .await
                }),
                None,
            )
        };
        timeout(DEADLINE, gate.entered.notified()).await?;
        snapshotting.abort();
        assert!(snapshotting.await.unwrap_err().is_cancelled());
        let turn_record = original_turn.as_ref().map(|turn| turn.record().clone());
        let finished_turn = original_turn.filter(|_| finish_turn);
        let finished_event = match &finished_turn {
            Some(turn) => Some(turn.finish().await?),
            None => None,
        };
        let next_turn = if advance_turn {
            Some(crate::test_support::begin_test_turn(thread.as_ref()).await?)
        } else {
            None
        };
        gate.release.add_permits(1);
        let event = timeout(DEADLINE, async {
            loop {
                if let Some(event) = thread
                    .get_events(None)
                    .await?
                    .events
                    .into_iter()
                    .find(|event| matches!(event.data, EventData::SandboxSnapshotted { .. }))
                {
                    return Ok::<_, anyhow::Error>(event);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        let event_turn = turn_record
            .as_ref()
            .filter(|_| !finish_turn && !advance_turn);
        assert_eq!(event.session_id, event_turn.map(|record| record.session_id));
        assert_eq!(event.turn_id, event_turn.map(|record| record.id));
        let EventData::SandboxSnapshotted {
            snapshot_id,
            sandbox_id,
        } = event.data
        else {
            unreachable!()
        };
        let owner_dir = harness.owner_dir(ResourceScope::Thread {
            agent_id: agent.record().id,
            thread_id: thread.record().id,
        });
        assert_eq!(
            load_snapshot_payload(harness, &owner_dir, snapshot_id)
                .await?
                .bytes,
            bytes::Bytes::from_static(b"capture")
        );
        if let Some(turn) = next_turn {
            for snapshot_id in [None, Some(snapshot_id)] {
                turn.start_sandbox(crate::StartSandboxRequest {
                    id: sandbox_id.clone(),
                    snapshot_id,
                    idle_seconds: None,
                    provider: None,
                })
                .await?;
            }
            let started = thread
                .get_events(None)
                .await?
                .events
                .into_iter()
                .filter(|event| {
                    event.turn_id == Some(turn.record().id)
                        && matches!(event.data, EventData::SandboxStarted { .. })
                })
                .collect::<Vec<_>>();
            assert_eq!(started.len(), 2);
            for event in &started {
                assert_eq!(event.session_id, Some(turn.record().session_id));
                assert_eq!(event.turn_id, Some(turn.record().id));
            }
            assert!(
                matches!(&started[0].data, EventData::SandboxStarted { sandbox_id: id, snapshot_id: None } if id == &sandbox_id)
            );
            assert!(
                matches!(&started[1].data, EventData::SandboxStarted { sandbox_id: id, snapshot_id: Some(snapshot) } if id == &sandbox_id && *snapshot == snapshot_id)
            );
            turn.finish().await?;
        }
        if let Some(turn) = finished_turn {
            assert_eq!(Some(turn.finish().await?), finished_event);
            let rebuilt = thread.turn_handle(turn.record().clone()).await?;
            assert_eq!(Some(rebuilt.finish().await?), finished_event);
        }
        assert!(harness.delete_agent(&agent.record().id).await?);
        assert_eq!(backend.deleted_snapshots.lock().unwrap().len(), 1);
    }
    Ok(())
}
