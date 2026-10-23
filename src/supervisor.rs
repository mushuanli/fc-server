use anyhow::{ensure, Result};
use std::{ffi::OsString, path::Path, process::Stdio};
use tokio::process::{Child, Command};

// This root supervisor retains PDEATHSIG when jailer drops its own credentials.
#[tracing::instrument(skip_all, err, fields(operation="vmm_supervisor", operation_id=%uuid::Uuid::new_v4(), vm_id=?args.get(2)))]
pub async fn run(parent: u32, args: &[OsString]) -> Result<()> {
    ensure!(!args.is_empty(), "missing jailer command");
    let mut signal = watch_parent(parent)?;
    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let status = tokio::select! {
        status = child.wait() => Some(status?),
        _ = signal.recv() => None,
    };
    match status {
        Some(status) => ensure!(status.success(), "jailer exited with status {status}"),
        None => {
            child.kill().await?;
        }
    }
    Ok(())
}

fn watch_parent(parent: u32) -> Result<tokio::signal::unix::Signal> {
    let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // SAFETY: prctl and getppid use integer arguments and no borrowed pointers.
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } == 0,
        "parent death signal unavailable"
    );
    ensure!(unsafe { libc::getppid() } as u32 == parent, "parent exited before supervisor started");
    Ok(signal)
}

#[tracing::instrument(skip_all, err, fields(operation="backend_supervisor", operation_id=%uuid::Uuid::new_v4(), vm_id=?group.file_name()))]
pub async fn run_backend(parent: u32, group: &Path, args: &[OsString]) -> Result<()> {
    ensure!(!args.is_empty(), "missing backend command");
    let mut signal = watch_parent(parent)?;
    let mut child = spawn_backend(group, args)?;
    let stderr = tokio::spawn(crate::diagnostics::prefix(child.stderr.take().unwrap()));
    let status = tokio::select! {
        status = child.wait() => Some(status?),
        _ = signal.recv() => None,
    };
    // Always stop the bwrap monitor as well, even if cgroup.kill failed.
    let killed = tokio::fs::write(group.join("cgroup.kill"), b"1").await;
    let reaped = child.kill().await;
    killed?;
    reaped?;
    if let Some(status) = status.filter(|s| !s.success()) {
        let bytes = stderr.await??;
        tracing::error!(operation="virtiofs", stage="backend_exit", category=crate::diagnostics::backend_category(&bytes), exit_code=?status.code(), result="failed");
        anyhow::bail!("virtiofs backend exited unsuccessfully");
    }
    let _ = stderr.await;
    Ok(())
}

fn spawn_backend(group: &Path, args: &[OsString]) -> Result<Child> {
    use std::os::fd::AsRawFd;
    let entry = std::fs::OpenOptions::new().write(true).open(group.join("cgroup.procs"))?;
    let fd = entry.as_raw_fd();
    let mut command = Command::new(&args[0]);
    command
        .args(&args[1..])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // SAFETY: write is async-signal-safe; the entry FD lives through spawn.
    unsafe {
        command.pre_exec(move || {
            if libc::write(fd, b"0".as_ptr().cast(), 1) != 1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command.spawn()?)
}

pub async fn stop(child: &mut Child, group: &Path) -> std::io::Result<()> {
    use std::time::Duration;
    let signalled = tokio::time::timeout(Duration::from_secs(5), terminate(child)).await;
    // A reaped supervisor does not prove that its descendants have exited.
    let tree = kill_tree(group).await;
    let reaped = if matches!(signalled, Ok(Ok(()))) {
        Ok(())
    } else {
        tokio::time::timeout(Duration::from_secs(5), child.kill()).await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "supervisor reap timeout")
        })?
    };
    tree?;
    reaped
}

pub fn populated(group: &Path) -> std::io::Result<bool> {
    match std::fs::read_to_string(group.join("cgroup.events")) {
        Ok(events) => Ok(events.lines().any(|line| line == "populated 1")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

async fn kill_tree(group: &Path) -> std::io::Result<()> {
    if !populated(group)? {
        return Ok(());
    }
    tracing::warn!(
        operation = "supervisor_stop",
        stage = "remaining_descendants",
        category = "PROCESS_TREE_RECLAIM",
        result = "forced"
    );
    tokio::fs::write(group.join("cgroup.kill"), b"1").await?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while populated(group)? {
        if tokio::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "process tree remains populated",
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    Ok(())
}

pub async fn terminate(child: &mut Child) -> Result<(), std::io::Error> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    if let Some(pid) = child.id() {
        // SAFETY: the owned unreaped child PID cannot have been reused.
        if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    child.wait().await?;
    Ok(())
}
