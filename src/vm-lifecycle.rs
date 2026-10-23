use crate::{
    config::{Config, Profile},
    error::Error,
    launcher,
    manager::{CreateVm, Vm},
    virtiofs,
    volume::Volume,
};
use std::time::{Duration, Instant};
use tokio::process::Child;

pub struct VmLauncher(pub Config);
impl VmLauncher {
    pub async fn launch(
        &self,
        spec: &CreateVm,
        profile: &Profile,
        volumes: &[Volume],
        uid: u32,
    ) -> Result<Vm, Error> {
        let deadline = Instant::now() + Duration::from_secs(profile.startup_timeout_secs);
        let root = launcher::prepare(
            &self.0,
            &spec.id,
            profile,
            volumes,
            &spec.directories,
            uid,
            deadline,
        )
        .await?;
        let (child, backend) = self.launch_processes(spec, profile, &root, uid, deadline).await?;
        let vm = Vm::new(spec.clone(), uid, child, backend, profile.max_connections);
        Ok(vm)
    }

    async fn launch_processes(
        &self,
        spec: &CreateVm,
        profile: &Profile,
        root: &std::path::Path,
        uid: u32,
        deadline: Instant,
    ) -> Result<(Child, Option<Child>), Error> {
        let mut backend = match spec.directories.first() {
            Some(name) => Some(
                virtiofs::start(&self.0, &spec.id, profile, &self.0.bindings[name], uid, deadline)
                    .await?,
            ),
            None => None,
        };
        let result = self.launch_vmm(spec, profile, root, uid, deadline).await;
        if result.is_err() {
            if let Some(backend) = &mut backend {
                virtiofs::stop(&spec.id, backend).await?;
            }
        }
        Ok((result?, backend))
    }

    async fn launch_vmm(
        &self,
        spec: &CreateVm,
        profile: &Profile,
        root: &std::path::Path,
        uid: u32,
        deadline: Instant,
    ) -> Result<Child, Error> {
        let mut child = launcher::spawn(&self.0, &spec.id, profile, uid)?;
        if let Err(error) = launcher::ready(&mut child, root, deadline).await {
            crate::supervisor::stop(&mut child, &launcher::cgroup(&spec.id))
                .await
                .map_err(|e| Error::io("failed_vm_stop", e))?;
            return Err(error);
        }
        Ok(child)
    }
}

pub async fn stop(config: &Config, id: &str, vm: &mut Vm) -> Result<&'static str, Error> {
    vm.stop.cancel();
    let mode = stop_vm(config, id, vm).await?;
    if let Some(backend) = &mut vm.backend {
        virtiofs::stop(id, backend).await?;
    }
    launcher::clean(config, id).await?;
    Ok(mode)
}

async fn stop_vm(config: &Config, id: &str, vm: &mut Vm) -> Result<&'static str, Error> {
    let group = launcher::cgroup(id);
    let mode = if vm.child.try_wait().map_err(|e| Error::io("vm_poll", e))?.is_none() {
        if launcher::poweroff(config, id, &mut vm.child).await? {
            "guest-shutdown"
        } else {
            "forced"
        }
    } else if crate::supervisor::populated(&group).map_err(|e| Error::io("vm_tree_poll", e))? {
        "forced"
    } else {
        "already-exited"
    };
    crate::supervisor::stop(&mut vm.child, &group)
        .await
        .map_err(|e| Error::io("vm_tree_reap", e))?;
    Ok(mode)
}
