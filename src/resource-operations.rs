use crate::{
    config::Config,
    error::{audit, Error},
    launcher,
    manager::{conflict, CreateVm, Shared, StopResult, Vm},
    vm_lifecycle::{self, VmLauncher},
    volume::{self, Volume},
};
use std::{sync::Arc, time::Instant};
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};
use tracing::Instrument;

// Only identical resource mutations serialize; the catalog lock never spans process execution or long filesystem operations.
pub async fn resource_lock(shared: &Shared, kind: &str, id: &str) -> OwnedMutexGuard<()> {
    let mutex = {
        let mut manager = shared.lock().await;
        manager.operations.retain(|_, lock| lock.strong_count() > 0);
        let key = format!("{kind}:{id}");
        let mutex = manager
            .operations
            .get(&key)
            .and_then(|lock| lock.upgrade())
            .unwrap_or_else(|| Arc::new(Mutex::new(())));
        manager.operations.insert(key, Arc::downgrade(&mutex));
        mutex
    };
    mutex.lock_owned().await
}

pub async fn create_vm(shared: Shared, spec: CreateVm) -> Result<serde_json::Value, Error> {
    let _serial = resource_lock(&shared, "vm", &spec.id).await;
    let (config, volumes, uid) = {
        let mut manager = shared.lock().await;
        if let Some(vm) = manager.vms.get(&spec.id) {
            if vm.spec != spec {
                return Err(conflict("ID_CONFLICT"));
            }
            return Ok(manager.view(&spec.id));
        }
        if manager.pending_vms.contains_key(&spec.id) {
            return Err(conflict("VM_CLEANUP_PENDING"));
        }
        manager.reserve_vm(&spec)?
    };
    let result = launch_vm(&config, &spec, &volumes, uid).await;
    if result.is_err() {
        // Failed cleanup keeps the reservation so its UID and mounts cannot be reused.
        launcher::clean(&config, &spec.id).await?;
    }
    let mut manager = shared.lock().await;
    manager.pending_vms.remove(&spec.id);
    manager.vms.insert(spec.id.clone(), result?);
    Ok(manager.view(&spec.id))
}

async fn launch_vm(
    config: &Config,
    spec: &CreateVm,
    volumes: &[Volume],
    uid: u32,
) -> Result<Vm, Error> {
    let started = Instant::now();
    let operation = uuid::Uuid::new_v4().to_string();
    let profile = config.profiles[&spec.profile].clone();
    let result = VmLauncher(config.clone())
        .launch(spec, &profile, volumes, uid)
        .instrument(tracing::info_span!("vm_create", operation_id=%operation, vm_id=%spec.id))
        .await;
    audit(&operation, "vm_create", "vmm_running", started, result.is_ok());
    result
}

pub async fn stop_vm(shared: Shared, id: String) -> Result<StopResult, Error> {
    let _serial = resource_lock(&shared, "vm", &id).await;
    let (config, mut vm) = {
        let mut manager = shared.lock().await;
        let Some(vm) = manager.take_vm(&id)? else {
            return Ok(StopResult::new(&id, "absent"));
        };
        (manager.config.clone(), vm)
    };
    let started = Instant::now();
    let operation = uuid::Uuid::new_v4().to_string();
    let result = vm_lifecycle::stop(&config, &id, &mut vm).await;
    audit(&operation, "vm_stop", "reaped_and_cleaned", started, result.is_ok());
    let mut manager = shared.lock().await;
    manager.pending_vms.remove(&id);
    match result {
        Ok(mode) => {
            tracing::info!(operation_id=%operation, operation="vm_stop", stage="outcome", mode, result="success");
            Ok(StopResult::new(&id, mode))
        }
        Err(error) => {
            manager.vms.insert(id, vm);
            Err(error)
        }
    }
}

pub async fn create_volume(shared: Shared, volume: Volume) -> Result<Volume, Error> {
    create_volume_using(shared, volume, |config, volume| async move {
        volume::create(&config, &volume).await
    })
    .await
}

async fn create_volume_using<F, Fut>(
    shared: Shared,
    volume: Volume,
    populate: F,
) -> Result<Volume, Error>
where
    F: FnOnce(crate::config::Config, Volume) -> Fut,
    Fut: std::future::Future<Output = Result<(), Error>>,
{
    let _serial = resource_lock(&shared, "volume", &volume.id).await;
    let config = {
        let mut manager = shared.lock().await;
        if !manager.reserve_volume(&volume)? {
            return Ok(volume);
        }
        manager.config.clone()
    };
    let started = Instant::now();
    let operation = uuid::Uuid::new_v4().to_string();
    let result = populate(config, volume.clone()).await;
    audit(&operation, "volume_create", "directory_import", started, result.is_ok());
    let mut manager = shared.lock().await;
    manager.pending_volumes.remove(&volume.id);
    result?;
    manager.volumes.insert(volume.id.clone(), volume.clone());
    manager.exports.insert(volume.id.clone(), Arc::new(Semaphore::new(1)));
    Ok(volume)
}

pub async fn delete_volume(shared: Shared, id: String) -> Result<(), Error> {
    let _serial = resource_lock(&shared, "volume", &id).await;
    let directory = {
        let mut manager = shared.lock().await;
        if manager.busy(&id) {
            return Err(conflict("VOLUME_BUSY"));
        }
        if !manager.volumes.contains_key(&id) {
            return Ok(());
        }
        manager.pending_volumes.insert(id.clone());
        volume::directory(&manager.config, &id)
    };
    let result = tokio::fs::remove_dir_all(directory).await;
    let mut manager = shared.lock().await;
    manager.pending_volumes.remove(&id);
    result.map_err(|e| Error::io("volume_delete", e))?;
    manager.volumes.remove(&id);
    manager.exports.remove(&id);
    Ok(())
}

pub async fn reap(shared: &Shared) {
    let ids = shared.lock().await.expired_vms();
    stop_all(shared, ids).await;
}

pub async fn drain(shared: &Shared) {
    let (ids, volumes) = {
        let mut manager = shared.lock().await;
        manager.closing = true;
        (
            manager.vms.keys().chain(manager.pending_vms.keys()).cloned().collect(),
            manager.pending_volumes.iter().cloned().collect::<Vec<_>>(),
        )
    };
    stop_all(shared, ids).await;
    for id in volumes {
        drop(resource_lock(shared, "volume", &id).await);
    }
}

async fn stop_all(shared: &Shared, ids: Vec<String>) {
    // Dropped join handles detach cleanup; aborting the reaper cannot abandon an owned VM.
    let work: Vec<_> =
        ids.into_iter().map(|id| tokio::spawn(stop_vm(shared.clone(), id))).collect();
    for task in work {
        match task.await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::error!(stage = error.stage, category = error.code, result = "failed")
            }
            Err(_) => {
                tracing::error!(stage = "vm_stop", category = "WORKER_FAILED", result = "failed")
            }
        }
    }
}

#[cfg(test)]
#[path = "resource-operations-tests.rs"]
mod tests;
