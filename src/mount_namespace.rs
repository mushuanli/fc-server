use anyhow::{Context, Result};
use std::{ffi::CString, os::fd::FromRawFd, os::unix::ffi::OsStrExt, path::Path};

// Run before starting any runtime threads. The mount exists only in this helper's namespace.
pub fn attach(fd: i32, target: &Path, read_only: bool) -> Result<()> {
    // SAFETY: this private CLI receives a live owned mount FD from its parent.
    let _tree = unsafe { std::fs::File::from_raw_fd(fd) };
    let target = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: unshare has no pointer arguments; mount receives valid static strings.
    check(unsafe { libc::unshare(libc::CLONE_NEWNS) } as i64).context("isolate mount namespace")?;
    private_root()?;
    attributes(fd, read_only)?;
    // SAFETY: fd references the detached tree; the target is a live NUL-terminated string.
    check(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            fd,
            c"".as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            0x4u32,
        )
    })
    .context("attach private source mount")?;
    Ok(())
}

fn private_root() -> Result<()> {
    // SAFETY: mount receives either null pointers or a valid static path.
    check(unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_PRIVATE | libc::MS_REC,
            std::ptr::null(),
        )
    } as i64)
    .context("disable mount propagation")
}

fn attributes(fd: i32, read_only: bool) -> Result<()> {
    let attributes = libc::mount_attr {
        attr_set: libc::MOUNT_ATTR_NODEV
            | libc::MOUNT_ATTR_NOSUID
            | if read_only { libc::MOUNT_ATTR_RDONLY } else { 0 },
        attr_clr: 0,
        propagation: libc::MS_PRIVATE,
        userns_fd: 0,
    };
    // SAFETY: the kernel only reads this live mount_attr; AT_EMPTY_PATH addresses fd directly.
    check(unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            &attributes,
            std::mem::size_of::<libc::mount_attr>(),
        )
    })
    .context("restrict source mount")
}

fn check(result: i64) -> std::io::Result<()> {
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
