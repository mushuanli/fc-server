use crate::{
    config::{Config, Profile, RootfsMode},
    error::Error,
    volume::Volume,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::{Child, Command};

pub fn jail(config: &Config, id: &str) -> PathBuf {
    config.state_dir.join(config.firecracker.file_name().expect("validated executable")).join(id)
}

pub fn machine(
    config: &Config,
    profile: &Profile,
    volumes: &[Volume],
    directories: &[String],
) -> Value {
    let shared = profile.rootfs_mode == RootfsMode::Shared;
    let mut drives = vec![
        json!({"drive_id":"rootfs", "path_on_host":"/rootfs.ext4", "is_root_device":!shared, "is_read_only":false}),
    ];
    for volume in volumes {
        let binding = &config.bindings[&volume.binding];
        drives.push(json!({"drive_id":volume.binding, "path_on_host":format!("/{}.ext4", volume.binding), "is_root_device":false, "is_read_only":binding.read_only}));
    }
    let mounts: Vec<_> = volumes.iter().map(|v| mount_token(config, &v.binding)).collect();
    let fs_mounts: Vec<_> = directories.iter().map(|name| mount_token(config, name)).collect();
    let mut machine = json!({
        "boot-source":{"kernel_image_path":"/vmlinux", "boot_args":format!("console=ttyS0 reboot=k panic=1 pci=off acpi=off i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd root=/dev/vda rw init=/sbin/init mindos.mounts={} mindos.virtiofs={}", mounts.join(","), fs_mounts.join(","))},
        "drives":drives, "machine-config":{"vcpu_count":profile.vcpus, "mem_size_mib":profile.memory_mib, "smt":false},
        "vsock":{"guest_cid":3, "uds_path":"/vsock.sock"}
    });
    if let Some(name) = directories.first() {
        machine["fs"] = json!([{"fs_id":name,"tag":name,"socket":"/backend/fs.sock"}]);
    }
    if shared {
        machine["boot-source"]["initrd_path"] = json!("/initrd");
        machine["pmem"] = json!([{"id":"base", "path_on_host":"/base.ext4", "root_device":false, "read_only":true}]);
    }
    machine
}

fn mount_token(config: &Config, name: &str) -> String {
    let binding = &config.bindings[name];
    format!("{}:{}:{}", name, binding.target, if binding.read_only { "ro" } else { "rw" })
}

pub async fn prepare(
    config: &Config,
    id: &str,
    profile: &Profile,
    volumes: &[Volume],
    directories: &[String],
    uid: u32,
    deadline: std::time::Instant,
) -> Result<PathBuf, Error> {
    let root = jail(config, id).join("root");
    tokio::fs::create_dir_all(&root).await.map_err(|e| Error::io("jail_create", e))?;
    copy(&profile.kernel, &root.join("vmlinux"), deadline).await?;
    readonly(&root.join("vmlinux"))?;
    prepare_root(profile, &root, uid, deadline).await?;
    attach(config, &root, volumes, uid).await?;
    let mut machine = machine(config, profile, volumes, directories);
    if let Some(slot) = network(config, profile, uid)? {
        machine["network-interfaces"] =
            json!([{"iface_id":"eth0","host_dev_name":slot.tap,"guest_mac":slot.guest_mac}]);
    }
    tokio::fs::write(root.join("machine.json"), serde_json::to_vec(&machine).unwrap())
        .await
        .map_err(|e| Error::io("machine_config_write", e))?;
    readonly(&root.join("machine.json"))?;
    Ok(root)
}

async fn prepare_root(
    profile: &Profile,
    root: &Path,
    uid: u32,
    deadline: std::time::Instant,
) -> Result<(), Error> {
    if profile.rootfs_mode == RootfsMode::Shared {
        prepare_shared_root(profile, root, deadline).await?;
    } else {
        copy(&profile.rootfs, &root.join("rootfs.ext4"), deadline).await?;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.join("rootfs.ext4"), std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::io("rootfs_permissions", e))?;
    chown(&root.join("rootfs.ext4"), uid)
}

async fn prepare_shared_root(
    profile: &Profile,
    root: &Path,
    deadline: std::time::Instant,
) -> Result<(), Error> {
    let initrd = profile.initrd.as_ref().expect("validated initrd");
    copy(initrd, &root.join("initrd"), deadline).await?;
    readonly(&root.join("initrd"))?;
    tokio::fs::hard_link(&profile.rootfs, root.join("base.ext4"))
        .await
        .map_err(|e| Error::io("shared_base_link_same_filesystem", e))?;
    create_upper(&root.join("rootfs.ext4"), profile.upper_size_mib, deadline).await?;
    tracing::info!(
        stage = "rootfs_prepare",
        mode = "shared",
        upper_size_mib = profile.upper_size_mib,
        result = "success"
    );
    Ok(())
}

async fn create_upper(path: &Path, size: u32, deadline: std::time::Instant) -> Result<(), Error> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::io("upper_create", e))?;
    file.set_len(u64::from(size) * 1024 * 1024).map_err(|e| Error::io("upper_size", e))?;
    let mut child = Command::new("/usr/sbin/mkfs.ext4")
        .env_clear()
        .args(["-q", "-F", "-m", "0", "-L", "fc-upper"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::io("upper_format", e))?;
    let status = tokio::time::timeout_at(deadline.into(), child.wait())
        .await
        .map_err(|_| Error::unavailable("upper_format_timeout"))?
        .map_err(|e| Error::io("upper_format_wait", e))?;
    if !status.success() {
        return Err(Error::unavailable("upper_format"));
    }
    Ok(())
}

async fn attach(config: &Config, root: &Path, volumes: &[Volume], uid: u32) -> Result<(), Error> {
    for volume in volumes {
        let source = crate::volume::directory(config, &volume.id).join("disk.ext4");
        let target = root.join(format!("{}.ext4", volume.binding));
        tokio::fs::hard_link(source, &target).await.map_err(|e| Error::io("volume_attach", e))?;
        chown(&target, uid)?;
    }
    Ok(())
}

fn readonly(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))
        .map_err(|e| Error::io("asset_permissions", e))
}

async fn copy(source: &Path, target: &Path, deadline: std::time::Instant) -> Result<(), Error> {
    let mut child = Command::new("/usr/bin/cp")
        .env_clear()
        .args(["--reflink=auto", "--sparse=always", "--"])
        .arg(source)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::io("image_copy", e))?;
    let status = match tokio::time::timeout_at(deadline.into(), child.wait()).await {
        Ok(status) => status.map_err(|e| Error::io("image_copy_wait", e))?,
        Err(_) => {
            child.kill().await.map_err(|e| Error::io("image_copy_stop", e))?;
            return Err(Error::unavailable("image_copy_timeout"));
        }
    };
    if !status.success() {
        return Err(Error::unavailable("image_copy"));
    }
    Ok(())
}

pub fn chown(path: &Path, uid: u32) -> Result<(), Error> {
    std::os::unix::fs::chown(path, Some(uid), Some(uid)).map_err(|e| Error::io("disk_owner", e))
}

pub fn spawn(config: &Config, id: &str, profile: &Profile, uid: u32) -> Result<Child, Error> {
    let command = jailer_command(config, id, profile, uid)?;
    let executable = std::env::current_exe().map_err(|e| Error::io("supervisor_executable", e))?;
    Command::new(executable)
        .env_clear()
        .arg("__supervise")
        .arg(std::process::id().to_string())
        .arg(command.as_std().get_program())
        .args(command.as_std().get_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| Error::io("supervisor_spawn", e))
}

fn jailer_command(
    config: &Config,
    id: &str,
    profile: &Profile,
    uid: u32,
) -> Result<Command, Error> {
    let mut command = Command::new(&config.jailer);
    command
        .args(["--id", id, "--exec-file"])
        .arg(&config.firecracker)
        .arg("--uid")
        .arg(uid.to_string())
        .arg("--gid")
        .arg(uid.to_string())
        .arg("--chroot-base-dir")
        .arg(&config.state_dir);
    cgroup_limits(&mut command, profile);
    if let Some(slot) = network(config, profile, uid)? {
        command.arg("--netns").arg(&slot.netns);
    }
    firecracker_arguments(&mut command);
    Ok(command)
}

fn firecracker_arguments(command: &mut Command) {
    command.args([
        "--resource-limit",
        "no-file=1024",
        "--",
        "--api-sock",
        "/api.sock",
        "--config-file",
        "/machine.json",
    ]);
}

fn cgroup_limits(command: &mut Command, profile: &Profile) {
    let shared_cache = if profile.rootfs_mode == RootfsMode::Shared {
        std::fs::metadata(&profile.rootfs).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    command
        .args(["--cgroup-version", "2", "--parent-cgroup", "mindos-firecracker", "--cgroup"])
        .arg(format!(
            "memory.max={}",
            (u64::from(profile.memory_mib) + 256) * 1024 * 1024 + shared_cache
        ))
        .args(["--cgroup", "pids.max=128", "--cgroup"])
        .arg(format!("cpu.max={} 100000", u32::from(profile.vcpus) * 100000));
}

fn network<'a>(
    config: &Config,
    profile: &'a Profile,
    uid: u32,
) -> Result<Option<&'a crate::config::NetworkSlot>, Error> {
    if profile.network_slots.is_empty() {
        return Ok(None);
    }
    profile
        .network_slots
        .get((uid - config.uid_base) as usize)
        .map(Some)
        .ok_or_else(|| Error::unavailable("network_slot_missing"))
}

pub async fn ready(
    child: &mut Child,
    root: &Path,
    deadline: std::time::Instant,
) -> Result<(), Error> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .unix_socket(root.join("api.sock"))
        .timeout(Duration::from_millis(300))
        .build()
        .map_err(|_| Error::unavailable("api_client"))?;
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(|e| Error::io("jailer_wait", e))? {
            tracing::error!(stage = "jailer_exit", exit_code = ?status.code(), result = "failed");
            return Err(Error::unavailable("jailer_exit"));
        }
        if let Ok(response) = client.get("http://localhost/").send().await {
            if let Ok(value) = response.json::<Value>().await {
                if value["state"] == "Running" {
                    return Ok(());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(Error::unavailable("vmm_start_timeout"))
}

pub fn cgroup(id: &str) -> PathBuf {
    PathBuf::from("/sys/fs/cgroup/mindos-firecracker").join(id)
}

pub async fn poweroff(config: &Config, id: &str, child: &mut Child) -> Result<bool, Error> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .unix_socket(jail(config, id).join("root/api.sock"))
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|_| Error::unavailable("poweroff_client"))?;
    let response = client
        .put("http://localhost/actions")
        .json(&json!({"action_type":"SendCtrlAltDel"}))
        .send()
        .await;
    if matches!(response, Ok(r) if r.status().is_success())
        && matches!(tokio::time::timeout(Duration::from_secs(15), child.wait()).await, Ok(Ok(status)) if status.success())
    {
        tracing::info!(operation = "vm_stop", stage = "guest_poweroff", result = "success");
        return Ok(true);
    }
    tracing::warn!(
        operation = "vm_stop",
        stage = "forced_stop",
        category = "GUEST_SHUTDOWN_UNCONFIRMED",
        result = "forced"
    );
    crate::supervisor::stop(child, &cgroup(id))
        .await
        .map_err(|e| Error::io("supervisor_stop", e))?;
    Ok(false)
}

pub async fn clean(config: &Config, id: &str) -> Result<(), Error> {
    crate::virtiofs::clean(id).await?;
    let group = cgroup(id);
    if group.exists() {
        tokio::fs::remove_dir(&group).await.map_err(|e| Error::io("cgroup_remove", e))?;
    }
    let dir = jail(config, id);
    if dir.exists() {
        tokio::fs::remove_dir_all(dir).await.map_err(|e| Error::io("jail_remove", e))?;
    }
    Ok(())
}

pub fn selected_volumes(
    config: &Config,
    profile: &Profile,
    ids: &[String],
    all: &BTreeMap<String, Volume>,
) -> Result<Vec<Volume>, Error> {
    let mut selected = BTreeMap::new();
    for id in ids {
        let volume = all.get(id).ok_or_else(|| {
            Error::new(axum::http::StatusCode::NOT_FOUND, "VOLUME_NOT_FOUND", "volume_select")
        })?;
        if !profile.bindings.contains(&volume.binding)
            || !config.bindings.contains_key(&volume.binding)
            || selected.contains_key(&volume.binding)
        {
            return Err(Error::new(
                axum::http::StatusCode::BAD_REQUEST,
                "INVALID_BINDING_SET",
                "volume_select",
            ));
        }
        selected.insert(volume.binding.clone(), volume.clone());
    }
    Ok(selected.into_values().collect())
}
