use crate::{config::Binding, error::Error};
use std::{fs::File, os::unix::fs::MetadataExt};

pub fn overlap(a: &Binding, b: &Binding) -> Result<bool, Error> {
    let a = a.source_fd.as_ref().ok_or_else(|| Error::unavailable("source_not_pinned"))?;
    let b = b.source_fd.as_ref().ok_or_else(|| Error::unavailable("source_not_pinned"))?;
    let a_identity = identity(a)?;
    let b_identity = identity(b)?;
    if a_identity.0 != b_identity.0 {
        return Ok(false);
    }
    // Separate bind mounts can hide a common ancestor. Serialize these exports
    // conservatively; ordinary disjoint directories on one mount can coexist.
    if mount_id(a)? != mount_id(b)? {
        return Ok(true);
    }
    Ok(ancestor(a, b)? || ancestor(b, a)?)
}

fn mount_id(file: &File) -> Result<u64, Error> {
    use std::os::fd::AsRawFd;
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))
        .map_err(|e| Error::io("directory_mount_id", e))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:")?.trim().parse().ok())
        .ok_or_else(|| Error::unavailable("directory_mount_id_missing"))
}

// Inode ancestry also detects bind-mount aliases of a previously exported subtree.
fn ancestor(parent: &File, child: &File) -> Result<bool, Error> {
    use rustix::fs::{openat, Mode, OFlags};
    let target = identity(parent)?;
    let mut cursor = child.try_clone().map_err(|e| Error::io("directory_duplicate", e))?;
    for _ in 0..512 {
        let current = identity(&cursor)?;
        if current == target {
            return Ok(true);
        }
        let next: File = openat(
            &cursor,
            "..",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| Error::io("directory_ancestor", e.into()))?
        .into();
        if identity(&next)? == current {
            return Ok(false);
        }
        cursor = next;
    }
    Err(Error::unavailable("directory_ancestry_limit"))
}

fn identity(file: &File) -> Result<(u64, u64), Error> {
    let stat = file.metadata().map_err(|e| Error::io("directory_identity", e))?;
    Ok((stat.dev(), stat.ino()))
}
