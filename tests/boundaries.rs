use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use firecracker_server::{
    api,
    config::{valid_id, Config},
    launcher,
    manager::Manager,
    transport,
    volume::{self, Volume},
};
use http_body_util::BodyExt;
use std::{collections::BTreeMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};
use tower::ServiceExt;

const KEY: &str = "test-key-with-at-least-thirty-two-characters";

#[test]
fn import_errors_keep_distinct_non_sensitive_categories() {
    use firecracker_server::diagnostics::import_category;
    assert_eq!(import_category(b"bwrap: namespace creation failed"), "IMPORT_SANDBOX_REJECTED");
    assert_eq!(import_category(b"Could not allocate block in ext2 filesystem"), "VOLUME_CAPACITY");
    assert_eq!(import_category(b"Permission denied: private-name"), "SOURCE_PERMISSION");
    assert_eq!(import_category(b"No such file or directory: private-name"), "SOURCE_DISAPPEARED");
    assert_eq!(
        import_category(b"unrecognized failure with private-name"),
        "DIRECTORY_IMPORT_FAILED"
    );
}

#[test]
fn direct_bindings_require_a_daemon_and_reject_block_capacity() {
    use firecracker_server::config::Backend;
    let dir = tempfile::tempdir().unwrap();
    let mut c = config(dir.path());
    c.bindings.get_mut("project").unwrap().backend = Backend::Virtiofs;
    assert!(c.validate().is_err());
    c.bindings.get_mut("project").unwrap().size_mib = 0;
    assert!(c.validate().is_err());
    c.virtiofsd = Some("/usr/libexec/virtiofsd".into());
    c.validate().unwrap();
}

#[tokio::test]
async fn direct_exports_require_authorized_ids_and_cannot_be_imported_as_blocks() {
    use firecracker_server::{config::Backend, virtiofs};
    let dir = tempfile::tempdir().unwrap();
    let mut c = config(dir.path());
    let profile = c.profiles["test"].clone();
    assert!(virtiofs::select(&c, &profile, &["project".into()]).is_err());
    c.bindings.get_mut("project").unwrap().backend = Backend::Virtiofs;
    virtiofs::select(&c, &profile, &["project".into()]).unwrap();
    assert!(virtiofs::select(&c, &profile, &["/etc".into()]).is_err());
    assert!(virtiofs::select(&c, &profile, &["project".into(), "project".into()]).is_err());
    let error = volume::create(&c, &Volume { id: "direct".into(), binding: "project".into() })
        .await
        .unwrap_err();
    assert_eq!(error.code, "BLOCK_BINDING_REQUIRED");
    assert!(!dir.path().join("volumes/direct").exists());
}

#[tokio::test]
async fn source_aliases_cannot_be_exported_to_concurrent_vms() {
    use firecracker_server::{config::Backend, manager::CreateVm};
    let dir = tempfile::tempdir().unwrap();
    let mut c = config(dir.path());
    c.bindings.get_mut("project").unwrap().backend = Backend::Virtiofs;
    c.bindings.get_mut("project").unwrap().source_fd =
        Some(Arc::new(firecracker_server::config::open_directory(dir.path()).unwrap()));
    c.bindings.insert("alias".into(), c.bindings["project"].clone());
    c.profiles.get_mut("test").unwrap().bindings.push("alias".into());
    let shared = tunnel_manager(c);
    let mut manager = shared.lock().await;
    manager.vms.get_mut("test-vm").unwrap().spec.directories = vec!["project".into()];
    let spec = CreateVm {
        id: "other".into(),
        profile: "test".into(),
        volumes: vec![],
        directories: vec!["alias".into()],
    };
    assert_eq!(manager.reserve_vm(&spec).err().unwrap().code, "DIRECTORY_ATTACHED");
}

#[test]
fn backend_diagnostics_never_expose_raw_messages() {
    use firecracker_server::diagnostics::backend_category;
    assert_eq!(
        backend_category(b"unexpected argument '--readonly' private text"),
        "VIRTIOFSD_VERSION_UNSUPPORTED"
    );
    assert_eq!(
        backend_category(b"bwrap: permission denied for private source"),
        "BACKEND_SANDBOX_REJECTED"
    );
    assert_eq!(backend_category(b"permission denied for token-file"), "BACKEND_PERMISSION");
    assert_eq!(backend_category(b"seccomp failed for private-name"), "BACKEND_SECCOMP");
    assert_eq!(backend_category(b"some unrecognized private data"), "BACKEND_EXITED");
}

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

fn router(config: Config) -> axum::Router {
    api::router(
        Arc::new(Mutex::new(Manager {
            config,
            vms: BTreeMap::new(),
            volumes: BTreeMap::new(),
            exports: BTreeMap::new(),
            closing: false,
            pending_vms: BTreeMap::new(),
            pending_volumes: Default::default(),
            operations: BTreeMap::new(),
        })),
        KEY,
    )
}

fn tunnel_manager(config: Config) -> firecracker_server::manager::Shared {
    use firecracker_server::manager::{CreateVm, Vm};
    let child =
        tokio::process::Command::new("/bin/sleep").arg("60").kill_on_drop(true).spawn().unwrap();
    let vm = Vm {
        spec: CreateVm {
            id: "test-vm".into(),
            profile: "test".into(),
            volumes: vec![],
            directories: vec![],
        },
        uid: 200000,
        child,
        backend: None,
        born: std::time::Instant::now(),
        stop: tokio_util::sync::CancellationToken::new(),
        connections: Arc::new(tokio::sync::Semaphore::new(1)),
    };
    Arc::new(Mutex::new(Manager {
        config,
        vms: BTreeMap::from([("test-vm".into(), vm)]),
        volumes: BTreeMap::new(),
        exports: BTreeMap::new(),
        closing: false,
        pending_vms: BTreeMap::new(),
        pending_volumes: Default::default(),
        operations: BTreeMap::new(),
    }))
}

async fn eof_sensitive_guest(listener: tokio::net::UnixListener) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut connect = [0; 13];
    stream.read_exact(&mut connect).await.unwrap();
    assert_eq!(&connect, b"CONNECT 7000\n");
    stream.write_all(b"OK 12345\n").await.unwrap();
    let mut message = [0; 6];
    stream.read_exact(&mut message).await.unwrap();
    assert_eq!(&message, b"hello\n");
    // Emulate Firecracker closing both vsock directions on host UDS EOF.
    let mut byte = [0; 1];
    if let Ok(Ok(0)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), stream.read(&mut byte)).await
    {
        return;
    }
    stream.write_all(b"final reply\n").await.unwrap();
}

#[tokio::test]
async fn client_preserves_final_reply_after_stdin_eof() {
    use std::process::Stdio;
    let dir = tempfile::tempdir().unwrap();
    let c = config(dir.path());
    let root = launcher::jail(&c, "test-vm").join("root");
    std::fs::create_dir_all(&root).unwrap();
    let guest = tokio::net::UnixListener::bind(root.join("vsock.sock")).unwrap();
    let worker = tokio::spawn(eof_sensitive_guest(guest));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let app = api::router(tunnel_manager(c), KEY);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_firecracker-server"))
        .env_clear()
        .env("FIRECRACKER_API_KEY", KEY)
        .args(["connect", &address, "test-vm", "7000"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hello\n").await.unwrap();
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"final reply\n");
    worker.await.unwrap();
    server.abort();
}

#[test]
fn rejects_paths_and_policy_injection() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = config(dir.path());
    c.validate().unwrap();
    for id in ["", "../escape", "-x/y", "a b", "$(id)", "é"] {
        assert!(!valid_id(id));
    }
    c.listen = "0.0.0.0:8790".parse().unwrap();
    assert!(c.validate().is_err());
    c.listen = "127.0.0.1:8790".parse().unwrap();
    for target in ["/etc", "/workspace/../../etc", "/home/x:rw", "/workspace/a b", "/proc/x"] {
        c.bindings.get_mut("project").unwrap().target = target.into();
        assert!(c.validate().is_err(), "{target}");
    }
}

#[tokio::test]
async fn authentication_and_origin_checks_cover_every_route() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(config(dir.path()));
    for path in ["/v1/health", "/v1/profiles", "/v1/vms", "/v1/volumes"] {
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        assert_eq!(app.clone().oneshot(request).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let request = Request::builder()
            .uri(path)
            .header("Authorization", format!("Bearer {KEY}"))
            .header("Origin", "http://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.clone().oneshot(request).await.unwrap().status(), StatusCode::FORBIDDEN);
    }
    let request = Request::builder()
        .uri("/v1/profiles")
        .header("Authorization", format!("Bearer {KEY}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(!text.contains(dir.path().to_str().unwrap()));
    assert!(!text.contains(KEY));
}

#[tokio::test]
async fn clients_cannot_supply_paths_commands_or_machine_policy() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(config(dir.path()));
    for body in [
        r#"{"id":"x","profile":"test","rootfs":"/etc/shadow"}"#,
        r#"{"id":"x","profile":"test","command":"/bin/sh"}"#,
        r#"{"id":"x","profile":"test","network":true}"#,
    ] {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/vms")
            .header("Authorization", format!("Bearer {KEY}"))
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}

#[test]
fn read_only_is_enforced_by_device_and_guest_mount() {
    let dir = tempfile::tempdir().unwrap();
    let c = config(dir.path());
    let volume = Volume { id: "v1".into(), binding: "project".into() };
    let machine = launcher::machine(&c, &c.profiles["test"], &[volume], &[]);
    assert_eq!(machine["drives"][1]["is_read_only"], true);
    assert!(machine["boot-source"]["boot_args"]
        .as_str()
        .unwrap()
        .contains("project:/workspace/project:ro"));
    assert!(machine.get("network-interfaces").is_none());
}

#[tokio::test]
async fn vsock_handshake_is_bounded_and_preserves_first_payload_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vsock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let worker = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 13];
        stream.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"CONNECT 7000\n");
        stream.write_all(b"OK 1073741824\npayload").await.unwrap();
    });
    let mut stream = transport::connect(&path, 7000).await.unwrap();
    let mut payload = String::new();
    stream.read_to_string(&mut payload).await.unwrap();
    assert_eq!(payload, "payload");
    worker.await.unwrap();
}

#[tokio::test]
async fn invalid_vsock_ack_is_rejected_without_logging_payload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vsock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 13];
        stream.read_exact(&mut request).await.unwrap();
        stream.write_all(&[b'x'; 64]).await.unwrap();
    });
    let error = transport::connect(&path, 7000).await.unwrap_err();
    assert_eq!(error.stage, "invalid_vsock_ack");
}

#[tokio::test]
async fn volume_import_is_persistent_and_rejects_unknown_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("sample"), "persistent fixture").unwrap();
    let mut c = config(dir.path());
    c.bindings.get_mut("project").unwrap().source = source.path().to_owned();
    std::fs::create_dir(dir.path().join("volumes")).unwrap();
    let volume = Volume { id: "persistent".into(), binding: "project".into() };
    volume::create(&c, &volume).await.unwrap();
    assert_eq!(volume::load(&c).unwrap()["persistent"].binding, "project");
    let result = volume::create(&c, &Volume { id: "bad".into(), binding: "unknown".into() }).await;
    assert_eq!(result.unwrap_err().code, "UNKNOWN_BINDING");
}

#[tokio::test]
async fn source_directory_replacement_cannot_redirect_import() {
    let state = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let original = source.path().join("allowed");
    let replacement = source.path().join("other");
    std::fs::create_dir(&original).unwrap();
    std::fs::create_dir(&replacement).unwrap();
    std::fs::write(original.join("marker"), "authorized fixture").unwrap();
    std::fs::write(replacement.join("marker"), "outside fixture").unwrap();
    let mut c = config(state.path());
    let binding = c.bindings.get_mut("project").unwrap();
    binding.source = original.clone();
    binding.source_fd =
        Some(Arc::new(firecracker_server::config::open_directory(&original).unwrap()));
    std::fs::rename(&original, source.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(&replacement, &original).unwrap();
    assert!(firecracker_server::config::open_directory(&original).is_err());
    std::fs::create_dir(state.path().join("volumes")).unwrap();
    volume::create(&c, &Volume { id: "pinned".into(), binding: "project".into() }).await.unwrap();
    let output = std::process::Command::new("/usr/sbin/debugfs")
        .args(["-R", "cat /marker"])
        .arg(state.path().join("volumes/pinned/disk.ext4"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "authorized fixture");
}
