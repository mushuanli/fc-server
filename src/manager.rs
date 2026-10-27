use crate::{
    config::{valid_id, Config},
    error::Error,
    launcher, virtiofs,
    volume::Volume,
};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::{
    process::Child,
    sync::{Mutex, Semaphore},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateVm {
    pub id: String,
    pub profile: String,
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub directories: Vec<String>,
}

pub struct Vm {
    pub spec: CreateVm,
    pub uid: u32,
    pub child: Child,
    pub backend: Option<Child>,
    pub born: Instant,
    pub stop: CancellationToken,
    pub connections: Arc<Semaphore>,
    pub instance: String,
    pub publications: BTreeMap<String, crate::publication::Published>,
}

pub struct PendingVm {
    pub spec: CreateVm,
    pub uid: u32,
    pub state: &'static str,
}

pub struct Manager {
    pub config: Config,
    pub vms: BTreeMap<String, Vm>,
    pub volumes: BTreeMap<String, Volume>,
    pub closing: bool,
    pub exports: BTreeMap<String, Arc<Semaphore>>,
    pub pending_vms: BTreeMap<String, PendingVm>,
    pub pending_volumes: std::collections::BTreeSet<String>,
    pub operations: BTreeMap<String, Weak<Mutex<()>>>,
}
pub type Shared = Arc<Mutex<Manager>>;

#[derive(Serialize)]
pub struct StopResult {
    pub id: String,
    pub stopped: bool,
    pub mode: &'static str,
}

impl StopResult {
    pub(crate) fn new(id: &str, mode: &'static str) -> Self {
        Self { id: id.into(), stopped: true, mode }
    }
}

impl Manager {
    pub fn reserve_vm(&mut self, spec: &CreateVm) -> Result<(Config, Vec<Volume>, u32), Error> {
        self.validate_create(spec)?;
        let profile = &self.config.profiles[&spec.profile];
        virtiofs::select(&self.config, profile, &spec.directories)?;
        self.reserve_directories(&spec.directories)?;
        let volumes =
            launcher::selected_volumes(&self.config, profile, &spec.volumes, &self.volumes)?;
        if spec.volumes.iter().any(|id| self.busy(id)) {
            return Err(conflict("VOLUME_ATTACHED"));
        }
        let uid = (self.config.uid_base..self.config.uid_base + self.config.max_vms as u32)
            .find(|uid| {
                self.vms.values().all(|vm| vm.uid != *uid)
                    && self.pending_vms.values().all(|vm| vm.uid != *uid)
            })
            .ok_or_else(|| conflict("VM_LIMIT"))?;
        self.pending_vms
            .insert(spec.id.clone(), PendingVm { spec: spec.clone(), uid, state: "starting" });
        Ok((self.config.clone(), volumes, uid))
    }

    pub fn take_vm(&mut self, id: &str) -> Result<Option<Vm>, Error> {
        let Some(vm) = self.vms.remove(id) else {
            if self.pending_vms.contains_key(id) {
                return Err(conflict("VM_CLEANUP_PENDING"));
            }
            return Ok(None);
        };
        self.pending_vms
            .insert(id.into(), PendingVm { spec: vm.spec.clone(), uid: vm.uid, state: "stopping" });
        Ok(Some(vm))
    }

    pub fn reserve_volume(&mut self, volume: &Volume) -> Result<bool, Error> {
        if self.closing {
            return Err(Error::unavailable("shutdown"));
        }
        if let Some(prior) = self.volumes.get(&volume.id) {
            return if prior.binding == volume.binding {
                Ok(false)
            } else {
                Err(conflict("ID_CONFLICT"))
            };
        }
        if self.volumes.len() + self.pending_volumes.len() >= 256 {
            return Err(conflict("VOLUME_LIMIT"));
        }
        self.pending_volumes.insert(volume.id.clone());
        Ok(true)
    }

    fn validate_create(&self, spec: &CreateVm) -> Result<(), Error> {
        if self.closing {
            return Err(Error::unavailable("shutdown"));
        }
        if !valid_id(&spec.id) || spec.volumes.len() > 8 {
            return Err(Error::new(StatusCode::BAD_REQUEST, "INVALID_VM", "vm_validate"));
        }
        if !self.config.profiles.contains_key(&spec.profile) {
            return Err(Error::new(StatusCode::BAD_REQUEST, "UNKNOWN_PROFILE", "vm_validate"));
        }
        if let Some(vm) = self.vms.get(&spec.id) {
            if vm.spec != *spec {
                return Err(conflict("ID_CONFLICT"));
            }
        } else if self.vms.len() + self.pending_vms.len() >= self.config.max_vms {
            return Err(conflict("VM_LIMIT"));
        }
        let path = launcher::jail(&self.config, &spec.id).join("root/backend/fs.sock");
        if path.as_os_str().len() >= 108 {
            return Err(Error::new(StatusCode::BAD_REQUEST, "SOCKET_PATH_TOO_LONG", "vm_validate"));
        }
        if !self.vms.contains_key(&spec.id)
            && (launcher::jail(&self.config, &spec.id).exists()
                || launcher::cgroup(&spec.id).exists()
                || virtiofs::group(&spec.id).exists())
        {
            return Err(conflict("STALE_JAIL"));
        }
        Ok(())
    }

    pub fn view(&self, id: &str) -> serde_json::Value {
        if let Some(pending) = self.pending_vms.get(id) {
            return serde_json::json!({"id":id,"profile":pending.spec.profile,"volumes":pending.spec.volumes,
                "directories":pending.spec.directories,"state":pending.state,"guestReady":false});
        }
        let vm = &self.vms[id];
        serde_json::json!({"id":id,"profile":vm.spec.profile,"volumes":vm.spec.volumes,"directories":vm.spec.directories,
            "state":"vmm-running","guestReady":false,"ageSecs":vm.born.elapsed().as_secs()})
    }

    pub fn busy(&self, volume: &str) -> bool {
        self.pending_volumes.contains(volume)
            || self.pending_vms.values().any(|vm| vm.spec.volumes.iter().any(|id| id == volume))
            || self.exports.get(volume).is_some_and(|s| s.available_permits() == 0)
            || self.vms.values().any(|vm| vm.spec.volumes.iter().any(|id| id == volume))
    }

    fn reserve_directories(&self, ids: &[String]) -> Result<(), Error> {
        for id in ids {
            for other in self
                .vms
                .values()
                .map(|vm| &vm.spec)
                .chain(self.pending_vms.values().map(|vm| &vm.spec))
                .flat_map(|spec| &spec.directories)
            {
                if crate::directory::overlap(
                    &self.config.bindings[id],
                    &self.config.bindings[other],
                )? {
                    return Err(conflict("DIRECTORY_ATTACHED"));
                }
            }
        }
        Ok(())
    }

    pub fn expired_vms(&mut self) -> Vec<String> {
        self.vms
            .iter_mut()
            .filter_map(|(id, vm)| {
                let expired = vm.born.elapsed()
                    >= Duration::from_secs(
                        self.config.profiles[&vm.spec.profile].max_lifetime_secs,
                    );
                let exited = !matches!(vm.child.try_wait(), Ok(None));
                let backend_exited =
                    vm.backend.as_mut().is_some_and(|child| !matches!(child.try_wait(), Ok(None)));
                (expired || exited || backend_exited).then(|| id.clone())
            })
            .collect()
    }
}

impl Vm {
    pub(crate) fn new(
        spec: CreateVm,
        uid: u32,
        child: Child,
        backend: Option<Child>,
        connections: usize,
    ) -> Self {
        Self {
            spec,
            uid,
            child,
            backend,
            born: Instant::now(),
            stop: CancellationToken::new(),
            connections: Arc::new(Semaphore::new(connections)),
            instance: uuid::Uuid::new_v4().to_string(),
            publications: BTreeMap::new(),
        }
    }
}

pub fn conflict(code: &'static str) -> Error {
    Error::new(StatusCode::CONFLICT, code, "resource_reserve")
}

// Detached mutations finish even if the HTTP caller disconnects; retry by stable id.
pub async fn mutation<T, F, Fut>(shared: Shared, action: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce(Shared) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, Error>> + Send + 'static,
{
    tokio::spawn(action(shared)).await.map_err(|_| Error::unavailable("mutation_worker"))?
}
