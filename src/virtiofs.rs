use crate::{
    config::{Backend, Binding, Config, Profile},
    error::{audit, Error},
    launcher, supervisor,
};
use axum::http::StatusCode;
use std::{
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::process::{Child, Command};

pub const CGROUP_ROOT: &str = "/sys/fs/cgroup/mindos-firecracker-fs";
const NAMESPACE: &[&str] = &[
    "--die-with-parent",
    "--unshare-pid",
    "--unshare-net",
    "--unshare-ipc",
    "--unshare-uts",
    "--unshare-cgroup",
    "--cap-add",
    "ALL",
    "--cap-drop",
    "CAP_DAC_READ_SEARCH",
];
const RUNTIME_MOUNTS: &[&str] = &[
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
const DAEMON_OPTIONS: &[&str] = &[
    "--",
    "/virtiofsd",
    "--shared-dir",
    "/source",
    "--socket-path",
    "/socket/fs.sock",
    "--sandbox",
    "namespace",
    "--seccomp",
    "kill",
    "--cache",
    "never",
    "--inode-file-handles=never",
    "--rlimit-nofile",
    "65536",
    "--thread-pool-size",
    "4",
    "--modcaps=-mknod",
    "--log-level",
    "warn",
];

pub fn group(id: &str) -> PathBuf {
    Path::new(CGROUP_ROOT).join(id)
}

pub fn select(config: &Config, profile: &Profile, ids: &[String]) -> Result<(), Error> {
    if ids.len() > 1 {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "ONE_DIRECTORY_SUPPORTED",
            "directory_select",
        ));
    }
    for id in ids {
        if !profile.bindings.contains(id)
            || !config.bindings.get(id).is_some_and(|b| b.backend == Backend::Virtiofs)
        {
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                "VIRTIOFS_BINDING_REQUIRED",
                "directory_select",
            ));
        }
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(vm_id=id))]
pub async fn start(
    config: &Config,
    id: &str,
    profile: &Profile,
    binding: &Binding,
    uid: u32,
    deadline: Instant,
) -> Result<Child, Error> {
    let operation = uuid::Uuid::new_v4().to_string();
    let started = Instant::now();
    let socket_dir = launcher::jail(config, id).join("root/backend");
    std::fs::create_dir(&socket_dir).map_err(|e| Error::io("backend_socket_dir", e))?;
    std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o711))
        .map_err(|e| Error::io("backend_socket_permissions", e))?;
    create_group(id, profile)?;
    let source = isolated_tree(binding)?;
    let stage = launcher::jail(config, id).join("source");
    std::fs::create_dir(&stage).map_err(|e| Error::io("source_mount_dir", e))?;
    let command = sandbox(config, &socket_dir, binding, &stage)?;
    let mut child = spawn(&command, id, source.as_raw_fd(), &stage, binding.read_only)?;
    let result = ready(&mut child, &socket_dir.join("fs.sock"), uid, deadline).await;
    audit(&operation, "virtiofs_start", "backend_listening", started, result.is_ok());
    if let Err(error) = result {
        stop(id, &mut child).await?;
        return Err(error);
    }
    Ok(child)
}

// A non-recursive detached mount excludes existing submounts and pins the inode.
fn isolated_tree(binding: &Binding) -> Result<std::fs::File, Error> {
    let source =
        binding.source_fd.as_ref().ok_or_else(|| Error::unavailable("source_not_pinned"))?;
    // SAFETY: source FD is valid; the NUL string is static; a successful syscall returns an owned FD.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            source.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH as u32 | 1u32 | libc::O_CLOEXEC as u32,
        )
    };
    if fd < 0 {
        return Err(Error::io("source_mount_clone", std::io::Error::last_os_error()));
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd as i32) })
}

fn sandbox(
    config: &Config,
    socket_dir: &Path,
    binding: &Binding,
    stage: &Path,
) -> Result<Command, Error> {
    let executable =
        config.virtiofsd.as_ref().ok_or_else(|| Error::unavailable("virtiofsd_missing"))?;
    let mut command = Command::new("/usr/bin/bwrap");
    command.args(NAMESPACE).args(RUNTIME_MOUNTS).arg("--ro-bind").arg(executable).arg("/virtiofsd");
    command.arg(if binding.read_only { "--ro-bind" } else { "--bind" }).arg(stage).arg("/source");
    command.arg("--bind").arg(socket_dir).arg("/socket");
    command.args(DAEMON_OPTIONS);
    if binding.read_only {
        command.arg("--readonly");
    }
    Ok(command)
}

fn spawn(
    command: &Command,
    id: &str,
    fd: i32,
    stage: &Path,
    read_only: bool,
) -> Result<Child, Error> {
    let executable = std::env::current_exe().map_err(|e| Error::io("supervisor_executable", e))?;
    let mut child = Command::new(executable);
    child
        .env_clear()
        .arg("__supervise-fs")
        .arg(std::process::id().to_string())
        .arg(group(id))
        .arg(fd.to_string())
        .arg(stage)
        .arg(read_only.to_string())
        .arg(command.as_std().get_program())
        .args(command.as_std().get_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    inherit_mount(&mut child, fd);
    child.spawn().map_err(|e| Error::io("backend_spawn", e))
}

fn inherit_mount(child: &mut Command, fd: i32) {
    // SAFETY: fcntl is async-signal-safe; caller keeps the mount FD alive through spawn.
    unsafe {
        child.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

async fn ready(child: &mut Child, socket: &Path, uid: u32, deadline: Instant) -> Result<(), Error> {
    while Instant::now() < deadline {
        if child.try_wait().map_err(|e| Error::io("backend_wait", e))?.is_some() {
            return Err(Error::unavailable("backend_exit"));
        }
        if std::fs::symlink_metadata(socket).is_ok_and(|m| m.file_type().is_socket()) {
            launcher::chown(socket, uid)?;
            std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| Error::io("backend_socket_permissions", e))?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(Error::unavailable("backend_start_timeout"))
}

fn create_group(id: &str, profile: &Profile) -> Result<(), Error> {
    let parent = Path::new(CGROUP_ROOT);
    for root in [Path::new("/sys/fs/cgroup"), parent] {
        std::fs::create_dir_all(root).map_err(|e| Error::io("backend_cgroup_parent", e))?;
        std::fs::write(root.join("cgroup.subtree_control"), b"+cpu +memory +pids")
            .map_err(|e| Error::io("backend_controllers", e))?;
    }
    let group = group(id);
    std::fs::create_dir(&group).map_err(|e| Error::io("backend_cgroup_create", e))?;
    let values = [
        ("memory.max", ((u64::from(profile.memory_mib) + 128) * 1024 * 1024).to_string()),
        ("pids.max", "64".into()),
        ("cpu.max", format!("{} 100000", u32::from(profile.vcpus) * 100000)),
    ];
    for (name, value) in values {
        std::fs::write(group.join(name), value)
            .map_err(|e| Error::io("backend_resource_limits", e))?;
    }
    if !group.join("cgroup.kill").exists() {
        return Err(Error::unavailable("cgroup_kill_required"));
    }
    Ok(())
}

pub async fn stop(id: &str, child: &mut Child) -> Result<(), Error> {
    let killed = if group(id).exists() {
        tokio::fs::write(group(id).join("cgroup.kill"), b"1").await
    } else {
        Ok(())
    };
    let reaped = supervisor::stop(child, &group(id)).await;
    killed.map_err(|e| Error::io("backend_tree_kill", e))?;
    reaped.map_err(|e| Error::io("backend_reap", e))?;
    Ok(())
}

pub async fn clean(id: &str) -> Result<(), Error> {
    let path = group(id);
    if path.exists() {
        tokio::fs::remove_dir(path).await.map_err(|e| Error::io("backend_cgroup_remove", e))?;
    }
    Ok(())
}
