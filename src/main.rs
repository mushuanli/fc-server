use anyhow::{ensure, Context, Result};
use firecracker_server::{
    api,
    config::{self, Config},
    manager::{Manager, Shared},
    volume,
};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use tokio::sync::{Mutex, Semaphore};

fn main() -> Result<()> {
    tracing_subscriber::fmt().json().with_writer(std::io::stderr).init();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "__supervise-fs") && args.len() > 6 {
        firecracker_server::mount_namespace::attach(
            args[3].parse()?,
            Path::new(&args[4]),
            args[5].parse()?,
        )?;
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(dispatch(args))
}

async fn dispatch(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("__supervise") if args.len() > 2 => firecracker_server::supervisor::run(args[1].parse()?, &args[2..].iter().map(std::ffi::OsString::from).collect::<Vec<_>>()).await,
        Some("__supervise-fs") if args.len() > 6 => firecracker_server::supervisor::run_backend(args[1].parse()?, Path::new(&args[2]), &args[6..].iter().map(std::ffi::OsString::from).collect::<Vec<_>>()).await,
        Some("serve") if args.len() == 2 => serve(Path::new(&args[1])).await,
        Some("connect") if args.len() == 4 => client(&args[1], &args[2], args[3].parse()?).await,
        _ => anyhow::bail!("usage: firecracker-server serve CONFIG | connect http://127.0.0.1:PORT VM_ID VSOCK_PORT; client key: FIRECRACKER_API_KEY"),
    }
}

fn initialize(path: &Path) -> Result<(Config, Vec<std::fs::File>, String)> {
    // SAFETY: umask is set before any background work creates files.
    unsafe {
        libc::umask(0o077);
    }
    ensure!(unsafe { libc::geteuid() } == 0, "serve requires root for jailer and cgroups");
    config::trusted(path)?;
    config::trusted(&std::env::current_exe()?)?;
    let config = Config::load(path)?;
    config.trusted_paths()?;
    ensure!(config.state_dir.is_dir(), "state_dir must be pre-created as root mode 0700");
    let locks = vec![
        exclusive_lock(Path::new("/run/mindos-firecracker.lock"))?,
        exclusive_lock(&config.state_dir.join("server.lock"))?,
    ];
    initialize_state(&config)?;
    let key = load_key(&config)?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .context("KVM is unavailable")?;
    Ok((config, locks, key))
}

fn exclusive_lock(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    lock.try_lock().context("another Firecracker server holds its exclusive host/state lock")?;
    Ok(lock)
}

fn initialize_state(config: &Config) -> Result<()> {
    let instance_dir =
        config.state_dir.join(config.firecracker.file_name().context("invalid executable name")?);
    std::fs::create_dir_all(&instance_dir)?;
    ensure!(std::fs::read_dir(&instance_dir)?.next().is_none(), "stale jails found: inspect cgroups, stop orphan VMMs and remove jails before restarting; volumes remain reusable");
    std::fs::create_dir_all(config.state_dir.join("volumes"))?;
    for group in [
        Path::new("/sys/fs/cgroup/mindos-firecracker"),
        Path::new(firecracker_server::virtiofs::CGROUP_ROOT),
    ] {
        if group.exists() {
            ensure!(
                std::fs::read_dir(group)?
                    .all(|entry| entry.is_ok_and(|entry| !entry.path().is_dir())),
                "stale VM cgroups found; inspect and remove empty groups before restarting"
            );
        }
    }
    Ok(())
}

fn load_key(config: &Config) -> Result<String> {
    let key = std::env::var(&config.api_key_env).context("API key environment is missing")?;
    ensure!(
        key.len() >= 32 && key.bytes().all(|b| b.is_ascii_graphic()),
        "API key must contain at least 32 printable ASCII characters"
    );
    Ok(key)
}

async fn serve(path: &Path) -> Result<()> {
    let (config, _lock, key) = initialize(path)?;
    let volumes = volume::load(&config)?;
    let exports = volumes.keys().map(|id| (id.clone(), Arc::new(Semaphore::new(1)))).collect();
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!(operation = "server_start", stage = "listening", address = %config.listen, result = "success");
    let shared = Arc::new(Mutex::new(Manager {
        config,
        vms: BTreeMap::new(),
        volumes,
        exports,
        closing: false,
        pending_vms: BTreeMap::new(),
        pending_volumes: Default::default(),
        operations: BTreeMap::new(),
    }));
    let reaper = tokio::spawn(reap(shared.clone()));
    let result = axum::serve(listener, api::router(shared.clone(), &key))
        .with_graceful_shutdown(shutdown(shared.clone()))
        .await;
    reaper.abort();
    drain(&shared).await;
    result.context("HTTP server stopped")
}

async fn reap(shared: Shared) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        firecracker_server::resource_operations::reap(&shared).await;
    }
}

async fn shutdown(shared: Shared) {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("signal handler");
    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    drain(&shared).await;
}

async fn drain(shared: &Shared) {
    firecracker_server::resource_operations::drain(shared).await;
}

async fn client(address: &str, id: &str, port: u32) -> Result<()> {
    ensure!(config::valid_id(id), "invalid VM id");
    let url = reqwest::Url::parse(address)?;
    ensure!(
        url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1")),
        "client requires a loopback HTTP address"
    );
    let key = std::env::var("FIRECRACKER_API_KEY").context("FIRECRACKER_API_KEY missing")?;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()?
        .post(format!("{}/v1/vms/{id}/connect/{port}", address.trim_end_matches('/')))
        .bearer_auth(key)
        .header("Connection", "upgrade")
        .header("Upgrade", "mindos-vsock")
        .send()
        .await
        .context("connect request failed")?;
    ensure!(
        response.status() == reqwest::StatusCode::SWITCHING_PROTOCOLS,
        "guest tunnel rejected: {}",
        response.status()
    );
    let stream = response.upgrade().await?;
    copy_stdio(stream).await
}

async fn copy_stdio(stream: reqwest::Upgraded) -> Result<()> {
    let (mut read, mut write) = tokio::io::split(stream);
    let upload = async {
        tokio::io::copy(&mut tokio::io::stdin(), &mut write).await?;
        // Firecracker treats host UDS EOF as shutdown of both vsock directions.
        // Keep the write half alive until the guest closes its protocol stream.
        tokio::io::AsyncWriteExt::flush(&mut write).await
    };
    let download = async {
        let mut output = tokio::io::stdout();
        tokio::io::copy(&mut read, &mut output).await?;
        tokio::io::AsyncWriteExt::flush(&mut output).await
    };
    tokio::pin!(upload, download);
    tokio::select! {
        result = &mut upload => { result?; download.await?; }
        result = &mut download => { result?; }
    }
    Ok(())
}
