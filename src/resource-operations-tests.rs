use super::*;
use crate::{
    config::Config,
    manager::{Manager, Vm},
};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::{oneshot, Notify};

fn config(root: &std::path::Path) -> Config {
    let text = format!(
        r#"
listen = "127.0.0.1:8790"
api_key_env = "TEST_KEY"
jailer = "/usr/local/bin/jailer"
firecracker = "/usr/local/bin/firecracker"
state_dir = "{}"
uid_base = 200000
max_vms = 2
[profiles.test]
kernel = "/kernel"
rootfs = "/rootfs"
vcpus = 2
memory_mib = 128
max_lifetime_secs = 30
startup_timeout_secs = 1
max_connections = 1
ports = [7000]
bindings = ["project"]
[bindings.project]
source = "{}"
target = "/workspace/project"
read_only = true
size_mib = 64
"#,
        root.display(),
        root.display()
    );
    toml::from_str(&text).unwrap()
}

fn shared(root: &std::path::Path) -> Shared {
    Arc::new(Mutex::new(Manager {
        config: config(root),
        vms: BTreeMap::new(),
        volumes: BTreeMap::new(),
        exports: BTreeMap::new(),
        closing: false,
        pending_vms: BTreeMap::new(),
        pending_volumes: Default::default(),
        operations: BTreeMap::new(),
    }))
}

#[tokio::test]
async fn slow_import_does_not_block_queries_or_another_vm_stop() {
    let root = tempfile::tempdir().unwrap();
    let shared = shared(root.path());
    let mut child = tokio::process::Command::new("/bin/true").spawn().unwrap();
    child.wait().await.unwrap();
    let spec = CreateVm {
        id: "independent".into(),
        profile: "test".into(),
        volumes: vec![],
        directories: vec![],
    };
    shared.lock().await.vms.insert(spec.id.clone(), Vm::new(spec, 200000, child, None, 1));
    let (entered, started) = oneshot::channel();
    let release = Arc::new(Notify::new());
    let signal = release.clone();
    let worker = tokio::spawn(create_volume_using(
        shared.clone(),
        Volume { id: "slow".into(), binding: "project".into() },
        |_, _| async move {
            let _ = entered.send(());
            signal.notified().await;
            Ok(())
        },
    ));
    started.await.unwrap();
    let app = crate::api::router(shared.clone(), "key");
    use tower::ServiceExt;
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        app.oneshot(
            axum::http::Request::builder()
                .uri("/v1/volumes")
                .header("Authorization", "Bearer key")
                .body(axum::body::Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert!(shared.lock().await.busy("slow"));
    let stopped =
        tokio::time::timeout(Duration::from_secs(1), stop_vm(shared.clone(), "independent".into()))
            .await
            .unwrap();
    // Restricted CI may deny cgroup inspection; failure must restore ownership and release only its reservation.
    let manager = shared.lock().await;
    assert!(manager.pending_vms.is_empty());
    assert_eq!(manager.vms.contains_key("independent"), stopped.is_err());
    drop(manager);
    release.notify_one();
    worker.await.unwrap().unwrap();
    assert!(!shared.lock().await.busy("slow"));
}

#[tokio::test]
async fn same_resource_retries_serialize_and_failures_release_reservations() {
    let root = tempfile::tempdir().unwrap();
    let shared = shared(root.path());
    let held = resource_lock(&shared, "volume", "retry").await;
    let volume = Volume { id: "retry".into(), binding: "project".into() };
    let worker = tokio::spawn(create_volume_using(shared.clone(), volume.clone(), |_, _| async {
        Err(Error::unavailable("fixture"))
    }));
    tokio::task::yield_now().await;
    assert!(!worker.is_finished());
    drop(held);
    assert!(worker.await.unwrap().is_err());
    assert!(!shared.lock().await.busy("retry"));
    create_volume_using(shared.clone(), volume.clone(), |_, _| async { Ok(()) }).await.unwrap();
    create_volume_using(shared.clone(), volume, |_, _| async {
        panic!("idempotent retry cannot import again")
    })
    .await
    .unwrap();
    assert_eq!(shared.lock().await.volumes.len(), 1);
}

#[tokio::test]
async fn launch_reservations_keep_uids_and_volumes_exclusive() {
    let root = tempfile::tempdir().unwrap();
    let shared = shared(root.path());
    let mut manager = shared.lock().await;
    manager.volumes.insert("disk".into(), Volume { id: "disk".into(), binding: "project".into() });
    let spec = CreateVm {
        id: "first".into(),
        profile: "test".into(),
        volumes: vec!["disk".into()],
        directories: vec![],
    };
    let (_, _, first_uid) = manager.reserve_vm(&spec).unwrap();
    let mut second = spec.clone();
    second.id = "second".into();
    assert_eq!(manager.reserve_vm(&second).err().unwrap().code, "VOLUME_ATTACHED");
    second.volumes.clear();
    let (_, _, second_uid) = manager.reserve_vm(&second).unwrap();
    assert_ne!(first_uid, second_uid);
    assert_eq!(manager.view("first")["state"], "starting");
}

#[tokio::test]
async fn shutdown_waits_for_inflight_import_without_blocking_the_catalog() {
    let root = tempfile::tempdir().unwrap();
    let shared = shared(root.path());
    let (entered, started) = oneshot::channel();
    let release = Arc::new(Notify::new());
    let signal = release.clone();
    let worker = tokio::spawn(create_volume_using(
        shared.clone(),
        Volume { id: "slow".into(), binding: "project".into() },
        |_, _| async move {
            let _ = entered.send(());
            signal.notified().await;
            Ok(())
        },
    ));
    started.await.unwrap();
    let draining = tokio::spawn({
        let shared = shared.clone();
        async move { drain(&shared).await }
    });
    tokio::task::yield_now().await;
    assert!(shared.lock().await.closing);
    assert!(!draining.is_finished());
    release.notify_one();
    worker.await.unwrap().unwrap();
    draining.await.unwrap();
    assert!(shared.lock().await.pending_volumes.is_empty());
}
