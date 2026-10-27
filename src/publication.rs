use crate::{
    config::Config,
    error::Error,
    manager::{Shared, Vm},
    resource_operations::resource_lock,
};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    net::Ipv4Addr,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, process::Command};

static PUBLISHER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Specification {
    #[serde(default = "loopback")]
    pub listen: Ipv4Addr,
    pub host_port: u16,
    pub guest_port: u16,
    #[serde(default)]
    pub protocol: Protocol,
}

#[derive(Clone, Copy, Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

pub struct Published {
    pub spec: Specification,
    _reservation: OwnedFd,
}

fn loopback() -> Ipv4Addr {
    Ipv4Addr::LOCALHOST
}

impl Specification {
    fn key(&self) -> String {
        key(self.protocol, self.host_port)
    }
    fn validate(&self) -> Result<(), Error> {
        if self.listen.is_multicast() || self.listen == Ipv4Addr::BROADCAST {
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                "INVALID_LISTEN_ADDRESS",
                "publication_validate",
            ));
        }
        if self.host_port == 0 || self.guest_port == 0 {
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                "INVALID_PUBLICATION_PORT",
                "publication_validate",
            ));
        }
        Ok(())
    }
}

pub fn key(protocol: Protocol, port: u16) -> String {
    format!("{protocol:?}:{port}")
}

fn reserve(spec: &Specification) -> Result<OwnedFd, Error> {
    let fd = socket(spec.protocol)?;
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: spec.host_port.to_be(),
        sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(spec.listen.octets()) },
        sin_zero: [0; 8],
    };
    // SAFETY: address is initialized and fd remains alive for the publication.
    let result = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_in).cast(),
            std::mem::size_of_val(&address) as _,
        )
    };
    if result != 0 {
        return Err(bind_error());
    }
    Ok(fd)
}

fn socket(protocol: Protocol) -> Result<OwnedFd, Error> {
    let kind = match protocol {
        Protocol::Tcp => libc::SOCK_STREAM,
        Protocol::Udp => libc::SOCK_DGRAM,
    };
    // SAFETY: socket has no pointer arguments; the returned descriptor is checked.
    let raw =
        unsafe { libc::socket(libc::AF_INET, kind | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
    if raw < 0 {
        return Err(Error::io("publication_socket", std::io::Error::last_os_error()));
    }
    // SAFETY: this is the sole owner of the successfully created descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn bind_error() -> Error {
    let error = std::io::Error::last_os_error();
    tracing::warn!(stage = "publication_bind", errno = error.raw_os_error(), result = "failed");
    let code = if error.raw_os_error() == Some(libc::EADDRINUSE) {
        "HOST_PORT_IN_USE"
    } else {
        "HOST_ADDRESS_UNAVAILABLE"
    };
    Error::new(StatusCode::CONFLICT, code, "publication_bind")
}

fn context(config: &Config, vm: &Vm, action: &str) -> Result<Value, Error> {
    let slot = (vm.uid - config.uid_base) as usize;
    let network = config.profiles[&vm.spec.profile].network_slots.get(slot).ok_or_else(|| {
        Error::new(StatusCode::CONFLICT, "VM_HAS_NO_NETWORK", "publication_validate")
    })?;
    Ok(json!({"action":action,"id":vm.spec.id,"instance":vm.instance,"slot":slot,
        "netns":network.netns,"tap":network.tap,"guest_mac":network.guest_mac}))
}

async fn invoke(config: &Config, mut request: Value) -> Result<(), Error> {
    let _serial = PUBLISHER.lock().await;
    let helper = config
        .port_publisher
        .as_ref()
        .ok_or_else(|| Error::unavailable("port_publisher_disabled"))?;
    let operation = uuid::Uuid::new_v4().to_string();
    request["operation_id"] = json!(operation);
    let started = Instant::now();
    let output = tokio::time::timeout(Duration::from_secs(30), execute_helper(helper, &request))
        .await
        .map_err(|_| Error::unavailable("port_publisher_timeout"))??;
    crate::error::audit(
        &operation,
        "port_publication",
        "nft_apply",
        started,
        output.status.success(),
    );
    check_output(output)
}

async fn execute_helper(
    helper: &std::path::Path,
    request: &Value,
) -> Result<std::process::Output, Error> {
    let mut child = Command::new(helper)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::io("port_publisher_spawn", e))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin
        .write_all(&serde_json::to_vec(request).unwrap())
        .await
        .map_err(|e| Error::io("port_publisher_write", e))?;
    drop(stdin);
    child.wait_with_output().await.map_err(|e| Error::io("port_publisher_wait", e))
}

fn check_output(output: std::process::Output) -> Result<(), Error> {
    if output.status.success() {
        return Ok(());
    }
    let data: Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    let code = match data["code"].as_str() {
        Some("HOST_PORT_IN_USE") => "HOST_PORT_IN_USE",
        Some("PUBLICATION_NETWORK_NOT_READY") => "PUBLICATION_NETWORK_NOT_READY",
        Some("PUBLICATION_NETWORK_MISMATCH") => "PUBLICATION_NETWORK_MISMATCH",
        Some("PUBLICATION_CONNTRACK_FAILED") => "PUBLICATION_CONNTRACK_FAILED",
        _ => "NFT_PUBLICATION_FAILED",
    };
    Err(Error::new(StatusCode::CONFLICT, code, "port_publisher_apply"))
}

pub async fn reset(config: &Config) -> Result<(), Error> {
    if config.port_publisher.is_none() {
        return Ok(());
    }
    invoke(config, json!({"action":"reset"})).await
}

pub async fn clear(config: &Config, vm: &mut Vm) -> Result<(), Error> {
    if config.port_publisher.is_none() {
        return Ok(());
    }
    if config.profiles[&vm.spec.profile].network_slots.is_empty() {
        return Ok(());
    }
    invoke(config, context(config, vm, "clear")?).await?;
    vm.publications.clear();
    Ok(())
}

fn prepare_publish(
    manager: &mut crate::manager::Manager,
    id: &str,
    spec: &Specification,
) -> Result<(Config, Value, bool), Error> {
    if manager.closing {
        return Err(Error::unavailable("shutdown"));
    }
    let config = manager.config.clone();
    let vm = manager.vms.get_mut(id).ok_or_else(missing)?;
    if !matches!(vm.child.try_wait(), Ok(None)) {
        return Err(Error::unavailable("vm_exited"));
    }
    let existing = if let Some(prior) = vm.publications.get(&spec.key()) {
        if prior.spec != *spec {
            return Err(rejected("PUBLICATION_CONFLICT"));
        }
        true
    } else {
        false
    };
    if !existing && vm.publications.len() >= 16 {
        return Err(rejected("PUBLICATION_LIMIT"));
    }
    Ok((config.clone(), context(&config, vm, "add")?, existing))
}

pub async fn publish(
    shared: Shared,
    id: String,
    spec: Specification,
) -> Result<Specification, Error> {
    spec.validate()?;
    let _guard = resource_lock(&shared, "vm", &id).await;
    let (config, mut request, existing) = prepare_publish(&mut *shared.lock().await, &id, &spec)?;
    let reservation = if existing { None } else { Some(reserve(&spec)?) };
    request["spec"] = json!(spec);
    invoke(&config, request).await?;
    if let Some(reservation) = reservation {
        let mut manager = shared.lock().await;
        let vm = manager.vms.get_mut(&id).expect("VM lock held");
        vm.publications
            .insert(spec.key(), Published { spec: spec.clone(), _reservation: reservation });
    }
    Ok(spec)
}

fn rejected(code: &'static str) -> Error {
    Error::new(StatusCode::CONFLICT, code, "publication_validate")
}

pub async fn remove(
    shared: Shared,
    id: String,
    protocol: Protocol,
    port: u16,
) -> Result<(), Error> {
    let _guard = resource_lock(&shared, "vm", &id).await;
    let (config, mut request) = {
        let manager = shared.lock().await;
        let vm = manager.vms.get(&id).ok_or_else(missing)?;
        if !vm.publications.contains_key(&key(protocol, port)) {
            return Ok(());
        }
        (manager.config.clone(), context(&manager.config, vm, "remove")?)
    };
    request["spec"] = json!({"protocol":protocol,"host_port":port});
    invoke(&config, request).await?;
    let mut manager = shared.lock().await;
    manager.vms.get_mut(&id).expect("VM lock held").publications.remove(&key(protocol, port));
    Ok(())
}

fn missing() -> Error {
    Error::new(StatusCode::NOT_FOUND, "VM_NOT_FOUND", "publication_lookup")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specification(port: u16, protocol: Protocol) -> Specification {
        Specification { listen: loopback(), host_port: port, guest_port: 8080, protocol }
    }

    #[test]
    fn host_socket_conflicts_and_release_are_enforced() {
        let listener = std::net::TcpListener::bind((loopback(), 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut spec = specification(port, Protocol::Tcp);
        assert_eq!(reserve(&spec).unwrap_err().code, "HOST_PORT_IN_USE");
        drop(listener);
        spec.listen = Ipv4Addr::UNSPECIFIED;
        let reserved = reserve(&spec).unwrap();
        assert!(std::net::TcpListener::bind((loopback(), port)).is_err());
        drop(reserved);
        assert!(std::net::TcpListener::bind((loopback(), port)).is_ok());
    }

    #[test]
    fn tcp_udp_reservations_are_independent() {
        let socket = std::net::UdpSocket::bind((loopback(), 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        assert_eq!(
            reserve(&specification(port, Protocol::Udp)).unwrap_err().code,
            "HOST_PORT_IN_USE"
        );
        assert!(reserve(&specification(port, Protocol::Tcp)).is_ok());
    }

    #[test]
    fn publishing_never_accepts_shell_paths_or_invalid_ports() {
        assert!(specification(0, Protocol::Tcp).validate().is_err());
        specification(80, Protocol::Tcp).validate().unwrap();
        let mut spec = specification(18080, Protocol::Udp);
        spec.guest_port = 53;
        spec.validate().unwrap();
        assert!(serde_json::from_value::<Specification>(
            json!({"host_port":18080,"guest_port":8080,"namespace":"/tmp/other"})
        )
        .is_err());
        assert!(serde_json::from_value::<Specification>(
            json!({"host_port":18080,"guest_port":8080,"protocol":"icmp"})
        )
        .is_err());
    }
}
