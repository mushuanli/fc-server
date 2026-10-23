use crate::{
    config::{valid_id, Backend, Binding, Config},
    error::Error,
};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    pub id: String,
    pub binding: String,
}

pub fn directory(config: &Config, id: &str) -> PathBuf {
    config.state_dir.join("volumes").join(id)
}

pub fn load(config: &Config) -> anyhow::Result<BTreeMap<String, Volume>> {
    let mut volumes = BTreeMap::new();
    for entry in std::fs::read_dir(config.state_dir.join("volumes"))? {
        let entry = entry?;
        anyhow::ensure!(entry.file_type()?.is_dir(), "unexpected volume entry");
        let volume: Volume =
            serde_json::from_slice(&std::fs::read(entry.path().join("manifest.json"))?)?;
        anyhow::ensure!(
            valid_id(&volume.id) && entry.file_name() == volume.id.as_str(),
            "invalid volume manifest"
        );
        anyhow::ensure!(
            config.bindings.get(&volume.binding).is_some_and(|b| b.backend == Backend::Block),
            "persisted volume requires its original binding"
        );
        anyhow::ensure!(entry.path().join("disk.ext4").is_file(), "volume disk missing");
        volumes.insert(volume.id.clone(), volume);
    }
    Ok(volumes)
}

pub async fn create(config: &Config, volume: &Volume) -> Result<(), Error> {
    if !valid_id(&volume.id) {
        return Err(Error::new(StatusCode::BAD_REQUEST, "INVALID_ID", "volume_validate"));
    }
    let binding = config
        .bindings
        .get(&volume.binding)
        .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST, "UNKNOWN_BINDING", "volume_validate"))?;
    if binding.backend != Backend::Block {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "BLOCK_BINDING_REQUIRED",
            "volume_validate",
        ));
    }
    let dir = directory(config, &volume.id);
    tokio::fs::create_dir(&dir).await.map_err(|e| Error::io("volume_reserve", e))?;
    let result = populate(&dir, binding, volume).await;
    if result.is_err() {
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
    result
}

async fn populate(dir: &Path, binding: &Binding, volume: &Volume) -> Result<(), Error> {
    let disk = dir.join("disk.ext4");
    let file = tokio::fs::File::create(&disk).await.map_err(|e| Error::io("volume_create", e))?;
    file.set_len(u64::from(binding.size_mib) * 1024 * 1024)
        .await
        .map_err(|e| Error::io("volume_size", e))?;
    import(dir, binding, &volume.binding).await?;
    let bytes = serde_json::to_vec(volume).expect("serializable volume");
    tokio::fs::write(dir.join("manifest.json"), bytes)
        .await
        .map_err(|e| Error::io("volume_manifest", e))
}

async fn import(dir: &Path, binding: &Binding, name: &str) -> Result<(), Error> {
    let source = match &binding.source_fd {
        Some(source) => source.clone(),
        None => std::sync::Arc::new(
            crate::config::open_directory(&binding.source)
                .map_err(|e| Error::io("source_open", e))?,
        ),
    };
    let mut command = importer(dir, source.as_ref(), name)?;
    command.stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| Error::io("volume_import_spawn", e))?;
    let stderr = child.stderr.take().expect("piped import stderr");
    let (status, stderr) = tokio::time::timeout(std::time::Duration::from_secs(300), async {
        tokio::try_join!(child.wait(), crate::diagnostics::prefix(stderr))
    })
    .await
    .map_err(|_| Error::unavailable("volume_import_timeout"))?
    .map_err(|e| Error::io("volume_import_wait", e))?;
    if !status.success() {
        return Err(Error::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            crate::diagnostics::import_category(&stderr),
            "volume_import",
        ));
    }
    Ok(())
}

fn importer(dir: &Path, source: &std::fs::File, name: &str) -> Result<Command, Error> {
    use std::os::fd::AsRawFd;
    let fd = source.as_raw_fd();
    let mut command = import_sandbox();
    command
        .args(["--ro-bind-fd", &fd.to_string(), "/source", "--bind"])
        .arg(dir)
        .arg("/output")
        .args(["--", "/usr/sbin/mkfs.ext4", "-q", "-F", "-E"])
        .arg(root_options(source)?)
        .arg("-L")
        .arg(format!("fc-{name}"))
        .args(["-d", "/source", "/output/disk.ext4"]);
    // SAFETY: fcntl is async-signal-safe; source stays open until spawn completes.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

fn root_options(source: &std::fs::File) -> Result<String, Error> {
    use std::os::unix::fs::MetadataExt;
    let metadata = source.metadata().map_err(|e| Error::io("source_stat", e))?;
    Ok(format!(
        "root_owner={}:{},root_perms={:o}",
        metadata.uid(),
        metadata.gid(),
        metadata.mode() & 0o777
    ))
}

const IMPORT_NAMESPACE: &[&str] = &[
    "--die-with-parent",
    "--ro-bind",
    "/usr",
    "/usr",
    "--symlink",
    "usr/bin",
    "/bin",
    "--symlink",
    "usr/lib",
    "/lib",
    "--symlink",
    "usr/lib64",
    "/lib64",
    "--proc",
    "/proc",
    "--dev",
    "/dev",
    "--tmpfs",
    "/tmp",
];

fn import_sandbox() -> Command {
    let mut command = Command::new("/usr/bin/bwrap");
    // Root imports must preserve source UID mappings. Only DAC_OVERRIDE remains;
    // DAC_READ_SEARCH would allow open_by_handle_at to escape path confinement.
    let root = unsafe { libc::geteuid() } == 0;
    if root {
        command.args([
            "--unshare-pid",
            "--unshare-net",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup",
            "--cap-drop",
            "ALL",
            "--cap-add",
            "CAP_DAC_OVERRIDE",
        ]);
    } else {
        command.args(["--unshare-all", "--cap-drop", "ALL"]);
    }
    command
        .env_clear()
        .args(IMPORT_NAMESPACE)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}
