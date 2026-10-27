use crate::{
    error::Error,
    launcher,
    manager::{self, mutation, CreateVm, Shared},
    publication::{self, Protocol, Specification},
    resource_operations, transport,
    volume::{self, Volume},
};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio_util::io::ReaderStream;

#[derive(Clone)]
pub struct Api {
    pub manager: Shared,
    pub key_hash: [u8; 32],
    pub requests: Arc<tokio::sync::Semaphore>,
}

pub fn router(manager: Shared, key: &str) -> Router {
    let api = Api {
        manager,
        key_hash: Sha256::digest(key.as_bytes()).into(),
        requests: Arc::new(tokio::sync::Semaphore::new(128)),
    };
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/profiles", get(profiles))
        .route("/v1/vms", get(list).post(create))
        .route("/v1/vms/:id", get(inspect).delete(stop))
        .route("/v1/vms/:id/ports", get(published_ports).post(publish_port))
        .route("/v1/vms/:id/ports/:protocol/:port", axum::routing::delete(unpublish_port))
        .route("/v1/vms/:id/connect/:port", post(connect))
        .route("/v1/volumes", get(volumes).post(create_volume))
        .route("/v1/volumes/:id", axum::routing::delete(delete_volume))
        .route("/v1/volumes/:id/disk", get(export_volume))
        .layer(DefaultBodyLimit::max(16384))
        .layer(middleware::from_fn_with_state(api.clone(), authorize))
        .with_state(api)
}

async fn authorize(
    State(api): State<Api>,
    request: Request,
    next: Next,
) -> Result<Response, Error> {
    let key = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let hash: [u8; 32] = Sha256::digest(key.as_bytes()).into();
    if !bool::from(hash.ct_eq(&api.key_hash)) {
        return Err(Error::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "authenticate"));
    }
    if request.headers().contains_key(header::ORIGIN) {
        return Err(Error::new(StatusCode::FORBIDDEN, "BROWSER_ORIGIN_DENIED", "authenticate"));
    }
    let _permit =
        api.requests.clone().try_acquire_owned().map_err(|_| manager::conflict("REQUEST_LIMIT"))?;
    Ok(next.run(request).await)
}

async fn health() -> Json<serde_json::Value> {
    Json(
        json!({"service":"firecracker-server","version":env!("CARGO_PKG_VERSION"),"processId":std::process::id()}),
    )
}
async fn profiles(State(api): State<Api>) -> Json<serde_json::Value> {
    let manager = api.manager.lock().await;
    let profiles: Vec<_> = manager.config.profiles.iter().map(|(id, p)| json!({"id":id,"vcpus":p.vcpus,
        "memoryMiB":p.memory_mib,"ports":p.ports,"bindings":p.bindings,"network":!p.network_slots.is_empty()})).collect();
    let bindings: Vec<_> = manager.config.bindings.iter().map(|(id,b)| json!({"id":id,"target":b.target,"backend":b.backend,"readOnly":b.read_only,"sizeMiB":b.size_mib})).collect();
    Json(json!({"profiles":profiles,"bindings":bindings}))
}
async fn list(State(api): State<Api>) -> Json<serde_json::Value> {
    let manager = api.manager.lock().await;
    Json(
        json!({"vms":manager.vms.keys().chain(manager.pending_vms.keys()).map(|id| manager.view(id)).collect::<Vec<_>>()}),
    )
}
async fn inspect(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, Error> {
    let manager = api.manager.lock().await;
    if !manager.vms.contains_key(&id) && !manager.pending_vms.contains_key(&id) {
        return Err(missing());
    }
    Ok(Json(manager.view(&id)))
}
async fn create(
    State(api): State<Api>,
    Json(spec): Json<CreateVm>,
) -> Result<Json<serde_json::Value>, Error> {
    mutation(api.manager, move |shared| resource_operations::create_vm(shared, spec))
        .await
        .map(Json)
}
async fn stop(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> Result<Json<manager::StopResult>, Error> {
    mutation(api.manager, move |shared| resource_operations::stop_vm(shared, id)).await.map(Json)
}
async fn published_ports(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Error> {
    let manager = api.manager.lock().await;
    let vm = manager.vms.get(&id).ok_or_else(missing)?;
    Ok(Json(json!({"ports":vm.publications.values().map(|p| &p.spec).collect::<Vec<_>>()})))
}

async fn publish_port(
    State(api): State<Api>,
    Path(id): Path<String>,
    Json(spec): Json<Specification>,
) -> Result<Json<Specification>, Error> {
    mutation(api.manager, move |shared| publication::publish(shared, id, spec)).await.map(Json)
}

async fn unpublish_port(
    State(api): State<Api>,
    Path((id, protocol, port)): Path<(String, Protocol, u16)>,
) -> Result<Json<Value>, Error> {
    mutation(api.manager, move |shared| publication::remove(shared, id, protocol, port)).await?;
    Ok(Json(json!({"removed":true})))
}

async fn volumes(State(api): State<Api>) -> Json<serde_json::Value> {
    let manager = api.manager.lock().await;
    Json(
        json!({"volumes":manager.volumes.values().map(|v| json!({"id":v.id,"binding":v.binding,"busy":manager.busy(&v.id)})).collect::<Vec<_>>()}),
    )
}
async fn create_volume(
    State(api): State<Api>,
    Json(volume): Json<Volume>,
) -> Result<Json<Volume>, Error> {
    mutation(api.manager, move |shared| resource_operations::create_volume(shared, volume))
        .await
        .map(Json)
}
async fn delete_volume(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    mutation(api.manager, move |shared| async move {
        resource_operations::delete_volume(shared, id).await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn export_volume(State(api): State<Api>, Path(id): Path<String>) -> Result<Response, Error> {
    let (directory, permit) = {
        let manager = api.manager.lock().await;
        if !manager.volumes.contains_key(&id) {
            return Err(missing());
        }
        if manager.busy(&id) {
            return Err(manager::conflict("VOLUME_BUSY"));
        }
        let permit = manager.exports[&id]
            .clone()
            .try_acquire_owned()
            .map_err(|_| manager::conflict("VOLUME_BUSY"))?;
        (volume::directory(&manager.config, &id), permit)
    };
    let file = tokio::fs::File::open(directory.join("disk.ext4"))
        .await
        .map_err(|e| Error::io("volume_export_open", e))?;
    let size = file.metadata().await.map_err(|e| Error::io("volume_export_stat", e))?.len();
    let stream = ReaderStream::new(file);
    let body = futures_util::stream::unfold((stream, permit), |(mut stream, permit)| async move {
        stream.next().await.map(|chunk| (chunk, (stream, permit)))
    });
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_LENGTH, &size.to_string()),
        ],
        Body::from_stream(body),
    )
        .into_response())
}

async fn connect(
    State(api): State<Api>,
    Path((id, port)): Path<(String, u32)>,
    mut request: Request,
) -> Result<Response, Error> {
    if request.headers().get(header::UPGRADE).and_then(|v| v.to_str().ok()) != Some("mindos-vsock")
    {
        return Err(Error::new(StatusCode::BAD_REQUEST, "UPGRADE_REQUIRED", "tunnel_validate"));
    }
    let (path, stop, permit) = reserve_tunnel(&api, &id, port).await?;
    let guest = transport::connect(&path, port).await?;
    let tunnel = transport::Tunnel { guest, stop, permit };
    tokio::spawn(tunnel.run(hyper::upgrade::on(&mut request)));
    Ok((
        StatusCode::SWITCHING_PROTOCOLS,
        [(header::CONNECTION, "upgrade"), (header::UPGRADE, "mindos-vsock")],
    )
        .into_response())
}

type Reservation =
    (std::path::PathBuf, tokio_util::sync::CancellationToken, tokio::sync::OwnedSemaphorePermit);
async fn reserve_tunnel(api: &Api, id: &str, port: u32) -> Result<Reservation, Error> {
    let manager = api.manager.lock().await;
    if manager.pending_vms.contains_key(id) {
        return Err(manager::conflict("VM_NOT_READY"));
    }
    let vm = manager.vms.get(id).ok_or_else(missing)?;
    if !manager.config.profiles[&vm.spec.profile].ports.contains(&port) {
        return Err(Error::new(StatusCode::FORBIDDEN, "PORT_DENIED", "tunnel_authorize"));
    }
    let permit = vm
        .connections
        .clone()
        .try_acquire_owned()
        .map_err(|_| manager::conflict("CONNECTION_LIMIT"))?;
    Ok((launcher::jail(&manager.config, id).join("root/vsock.sock"), vm.stop.clone(), permit))
}

fn missing() -> Error {
    Error::new(StatusCode::NOT_FOUND, "NOT_FOUND", "resource_lookup")
}
