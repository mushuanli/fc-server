use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub api_key_env: String,
    pub jailer: PathBuf,
    pub firecracker: PathBuf,
    pub virtiofsd: Option<PathBuf>,
    pub port_publisher: Option<PathBuf>,
    pub state_dir: PathBuf,
    pub uid_base: u32,
    pub max_vms: usize,
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub bindings: BTreeMap<String, Binding>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    #[serde(default)]
    pub rootfs_mode: RootfsMode,
    pub initrd: Option<PathBuf>,
    #[serde(default = "default_upper_size")]
    pub upper_size_mib: u32,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub max_lifetime_secs: u64,
    pub startup_timeout_secs: u64,
    pub max_connections: usize,
    pub ports: Vec<u32>,
    #[serde(default)]
    pub bindings: Vec<String>,
    #[serde(default)]
    pub network_slots: Vec<NetworkSlot>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootfsMode {
    #[default]
    Private,
    Shared,
}

fn default_upper_size() -> u32 {
    2048
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSlot {
    pub tap: String,
    pub netns: PathBuf,
    pub guest_mac: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub source: PathBuf,
    pub target: String,
    pub read_only: bool,
    #[serde(default)]
    pub backend: Backend,
    #[serde(default)]
    pub size_mib: u32,
    #[serde(skip)]
    pub source_fd: Option<std::sync::Arc<std::fs::File>>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    #[default]
    Block,
    Virtiofs,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 48 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        config.validate()?;
        for binding in config.bindings.values_mut() {
            binding.source_fd = Some(std::sync::Arc::new(open_directory(&binding.source)?));
        }
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.listen.ip().is_loopback(),
            "listen must be loopback; use an authenticated tunnel for remote access"
        );
        ensure!((1..=128).contains(&self.max_vms), "max_vms must be 1..128");
        ensure!(
            self.uid_base >= 65536 && self.uid_base.checked_add(self.max_vms as u32).is_some(),
            "invalid dedicated uid range"
        );
        ensure!(
            !self.api_key_env.is_empty() && !self.profiles.is_empty(),
            "key environment and profiles required"
        );
        self.validate_profiles()?;
        for (name, binding) in &self.bindings {
            binding.validate(name)?;
            ensure!(
                binding.backend != Backend::Virtiofs || self.virtiofsd.is_some(),
                "virtiofs bindings require virtiofsd"
            );
        }
        Ok(())
    }
    fn validate_profiles(&self) -> Result<()> {
        for (name, profile) in &self.profiles {
            ensure!(valid_id(name), "invalid profile id");
            profile.validate()?;
            ensure!(
                profile.network_slots.is_empty() || profile.network_slots.len() == self.max_vms,
                "network_slots must cover every global VM slot"
            );
            ensure!(profile.bindings.len() <= 8, "at most 8 directory bindings per profile");
            ensure!(
                profile.bindings.iter().all(|b| self.bindings.contains_key(b)),
                "unknown profile binding"
            );
            self.validate_targets(profile)?;
        }
        Ok(())
    }
    fn validate_targets(&self, profile: &Profile) -> Result<()> {
        for (index, name) in profile.bindings.iter().enumerate() {
            let target = Path::new(&self.bindings[name].target);
            ensure!(
                profile.bindings[index + 1..].iter().all(|other| {
                    let other = Path::new(&self.bindings[other].target);
                    !target.starts_with(other) && !other.starts_with(target)
                }),
                "profile binding targets must not overlap"
            );
        }
        Ok(())
    }
    pub fn trusted_paths(&self) -> Result<()> {
        for path in [&self.jailer, &self.firecracker, &self.state_dir] {
            trusted(path)?;
        }
        if let Some(daemon) = &self.virtiofsd {
            trusted(daemon)?;
        }
        if let Some(helper) = &self.port_publisher {
            trusted(helper)?;
        }
        for profile in self.profiles.values() {
            trusted(&profile.kernel)?;
            trusted(&profile.rootfs)?;
            if let Some(initrd) = &profile.initrd {
                trusted(initrd)?;
            }
            if profile.rootfs_mode == RootfsMode::Shared {
                use std::os::unix::fs::MetadataExt;
                let metadata = std::fs::metadata(&profile.rootfs)?;
                ensure!(metadata.mode() & 0o777 == 0o444, "shared rootfs must have mode 0444");
            }
            for slot in &profile.network_slots {
                trusted(&slot.netns)?;
            }
        }
        Ok(())
    }
}

impl Binding {
    fn validate(&self, name: &str) -> Result<()> {
        ensure!(name != "rootfs", "rootfs binding id is reserved");
        ensure!(
            valid_id(name) && name.len() <= 12,
            "binding id must be 1..12 ASCII letters, digits or hyphens"
        );
        match self.backend {
            Backend::Block => {
                ensure!((64..=65536).contains(&self.size_mib), "binding size must be 64..65536 MiB")
            }
            Backend::Virtiofs => {
                ensure!(self.size_mib == 0, "virtiofs uses host files; omit size_mib")
            }
        }
        self.validate_target()?;
        ensure!(
            self.source.is_absolute() && self.source.is_dir(),
            "binding source must be an existing absolute directory"
        );
        ensure!(self.source.canonicalize()? == self.source, "binding source must be canonical");
        Ok(())
    }
    fn validate_target(&self) -> Result<()> {
        let target = Path::new(&self.target);
        ensure!(
            self.target.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b)),
            "unsafe guest target"
        );
        ensure!(
            target.is_absolute()
                && target.components().all(|p| !matches!(
                    p,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )),
            "invalid guest target"
        );
        ensure!(
            ["/workspace/", "/home/"].iter().any(|p| self.target.starts_with(p)),
            "guest targets must be below /workspace or /home"
        );
        Ok(())
    }
}

impl Profile {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.rootfs_mode != RootfsMode::Shared || self.initrd.is_some(),
            "shared rootfs requires the overlay initrd"
        );
        ensure!((64..=65536).contains(&self.upper_size_mib), "upper size must be 64..65536 MiB");
        ensure!(
            (1..=32).contains(&self.vcpus) && (self.vcpus == 1 || self.vcpus.is_multiple_of(2)),
            "vcpus must be 1 or even, at most 32"
        );
        ensure!((128..=65536).contains(&self.memory_mib), "memory_mib must be 128..65536");
        ensure!((1..=86400).contains(&self.max_lifetime_secs), "invalid lifetime");
        ensure!((1..=300).contains(&self.startup_timeout_secs), "invalid startup timeout");
        ensure!((1..=64).contains(&self.max_connections), "invalid connection limit");
        ensure!(!self.ports.is_empty() && self.ports.len() <= 16, "1..16 ports required");
        ensure!(self.ports.iter().all(|p| (1024..1073741824).contains(p)), "invalid vsock port");
        self.validate_network()
    }
    fn validate_network(&self) -> Result<()> {
        for slot in &self.network_slots {
            ensure!(
                !slot.tap.is_empty()
                    && slot.tap.len() <= 15
                    && slot.tap.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "invalid tap name"
            );
            ensure!(slot.netns.is_absolute(), "netns must be absolute");
            ensure!(
                slot.guest_mac.len() == 17
                    && slot.guest_mac.split(':').count() == 6
                    && slot
                        .guest_mac
                        .split(':')
                        .all(|b| b.len() == 2 && u8::from_str_radix(b, 16).is_ok()),
                "invalid guest MAC"
            );
        }
        Ok(())
    }
}

pub fn trusted(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    ensure!(path.is_absolute(), "trusted path must be absolute");
    ensure!(
        path.canonicalize().context("trusted path unavailable")? == path,
        "trusted path must be canonical without symlinks"
    );
    for ancestor in path.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor)?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!(
                "trusted path and parents must be root-owned and not group/world-writable: {}",
                ancestor.display()
            );
        }
    }
    Ok(())
}

pub fn open_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{openat2, Mode, OFlags, ResolveFlags, CWD};
    Ok(openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )?
    .into())
}
