use std::{
    collections::HashMap,
    convert::TryFrom,
    future,
    net::IpAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use russh::{client, Channel, Disconnect};
use tokio::{
    io::{self, copy_bidirectional, AsyncWriteExt},
    net::TcpStream,
    task::JoinHandle,
    time::sleep,
};
use ulid::Ulid;
use url::Host as UrlHost;

use super::async_container::ContainerAsync;
use crate::{
    core::{
        async_drop,
        containers::{ContainerRequest, Host},
        error::TestcontainersError,
        image::ImageExt,
        ports::IntoContainerPort,
        WaitFor,
    },
    images::generic::GenericImage,
    runners::AsyncRunner,
    Image,
};

pub(crate) const HOST_INTERNAL_ALIAS: &str = "host.testcontainers.internal";
const SSHD_IMAGE: &str = "testcontainers/sshd";
const SSHD_TAG: &str = "1.3.0";
const SSH_USERNAME: &str = "root";
const SSH_PORT: u16 = 22;

// Configuration constants for SSH connection
const SSH_CONNECTION_MAX_ATTEMPTS: u32 = 20;
const SSH_CONNECTION_RETRY_DELAY_MS: u64 = 100;

/// Manages the lifetime of the SSH reverse tunnels used to expose host ports.
pub(crate) struct HostPortExposure {
    _sidecar: Box<ContainerAsync<GenericImage>>,
    ssh_handle: Option<client::Handle<HostExposeClient>>,
    state: Arc<ForwardState>,
}

impl HostPortExposure {
    /// Sets up host port exposure for the provided container request.
    pub(crate) async fn setup<I: Image>(
        container_req: &mut ContainerRequest<I>,
    ) -> Result<Option<Self>, TestcontainersError> {
        let mut requested_ports = match container_req
            .host_port_exposures()
            .map(|ports| ports.to_vec())
        {
            Some(ports) if !ports.is_empty() => ports,
            _ => return Ok(None),
        };

        requested_ports.sort_unstable();
        requested_ports.dedup();

        if container_req.hosts.contains_key(HOST_INTERNAL_ALIAS) {
            return Err(TestcontainersError::other(
                "host port exposure is not supported when 'host.testcontainers.internal' is already defined",
            ));
        }

        if requested_ports.contains(&0) {
            return Err(TestcontainersError::other(
                "host port exposure requires ports greater than zero (port 0 is invalid)",
            ));
        }

        if requested_ports.contains(&SSH_PORT) {
            return Err(TestcontainersError::other(
                "host port exposure does not support exposing port 22 (SSH port is reserved)",
            ));
        }

        if let Some(network) = container_req.network() {
            if network == "host" {
                return Err(TestcontainersError::other(
                    "host port exposure is not supported with host network mode",
                ));
            }

            if network.starts_with("container:") {
                return Err(TestcontainersError::other(
                    "host port exposure is not supported with container network mode",
                ));
            }
        }

        #[cfg(feature = "reusable-containers")]
        {
            use crate::ReuseDirective;
            if !matches!(container_req.reuse(), ReuseDirective::Never) {
                return Err(TestcontainersError::other(
                    "host port exposure is not supported for reusable containers (due to SSH tunnel conflicts)",
                ));
            }
        }

        let password = format!("tc-{}", Ulid::new());

        let mut sshd = GenericImage::new(SSHD_IMAGE, SSHD_TAG)
            .with_exposed_port((SSH_PORT).tcp())
            .with_wait_for(WaitFor::seconds(1))
            .with_env_var("PASSWORD", password.clone());

        if let Some(network) = container_req.network() {
            sshd = sshd.with_network(network.clone());
        }

        let sidecar = sshd.start().await?;

        let ssh_host = sidecar.get_host().await?;
        let ssh_host_port = sidecar.get_host_port_ipv4((SSH_PORT).tcp()).await?;
        let sidecar_ip = resolve_sidecar_ip(&sidecar).await?;

        container_req
            .hosts
            .insert(HOST_INTERNAL_ALIAS.to_string(), Host::Addr(sidecar_ip));

        let tcp_stream = connect_with_retry(&ssh_host, ssh_host_port).await?;

        let config = client::Config {
            nodelay: true,
            keepalive_interval: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let state = Arc::new(ForwardState::new());
        let handler = HostExposeClient::new(Arc::clone(&state));
        let config = Arc::new(config);

        let mut ssh_handle = client::connect_stream(config, tcp_stream, handler)
            .await
            .map_err(TestcontainersError::from)?;

        let auth_result = ssh_handle
            .authenticate_password(SSH_USERNAME, password)
            .await
            .map_err(|err| {
                map_ssh_error("SSH authentication failed for host port exposure", err)
            })?;

        if !auth_result.success() {
            return Err(TestcontainersError::other(
                "SSH authentication failed for host port exposure - check SSHD container logs and credentials",
            ));
        }

        for port in requested_ports {
            state.register_port(port, port);

            let bound_port = ssh_handle
                .tcpip_forward("0.0.0.0", u32::from(port))
                .await
                .map_err(|err| {
                    map_ssh_error(
                        &format!("failed to request remote port forwarding for {port}"),
                        err,
                    )
                })?;

            let bound_port = u16::try_from(bound_port).map_err(|_| {
                TestcontainersError::other(format!(
                    "remote sshd assigned invalid port for host exposure: requested {port}, bound {bound_port} - port range mismatch"
                ))
            })?;

            state.register_port(bound_port, port);

            if bound_port != port {
                log::warn!(
                    "remote sshd assigned different port for host exposure: requested {port}, bound {bound_port}"
                );
            }
        }

        Ok(Some(Self {
            _sidecar: Box::new(sidecar),
            ssh_handle: Some(ssh_handle),
            state,
        }))
    }

    pub(crate) fn shutdown(&mut self) {
        self.state.stop();

        if let Some(handle) = self.ssh_handle.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                async_drop::async_drop(async move {
                    if let Err(err) = handle
                        .disconnect(
                            Disconnect::ByApplication,
                            "testcontainers host port exposure cleanup",
                            "",
                        )
                        .await
                    {
                        log::debug!("ssh disconnect during host exposure cleanup failed: {err}");
                    }
                });
            }
        }

        for task in self.state.drain_tasks() {
            task.abort();
        }
    }
}

impl Drop for HostPortExposure {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn resolve_sidecar_ip(
    sidecar: &ContainerAsync<GenericImage>,
) -> Result<IpAddr, TestcontainersError> {
    sidecar.get_bridge_ip_address().await
}

async fn connect_with_retry(host: &UrlHost, port: u16) -> Result<TcpStream, TestcontainersError> {
    let host_str = host.to_string();
    let mut attempts = 0;

    loop {
        match TcpStream::connect((host_str.as_str(), port)).await {
            Ok(stream) => {
                if let Err(err) = stream.set_nodelay(true) {
                    return Err(TestcontainersError::other(format!(
                        "failed to configure ssh tcp stream: {err}"
                    )));
                }
                return Ok(stream);
            }
            Err(err) if attempts < SSH_CONNECTION_MAX_ATTEMPTS => {
                attempts += 1;
                sleep(Duration::from_millis(SSH_CONNECTION_RETRY_DELAY_MS)).await;
                log::trace!(
                    "waiting for sshd sidecar to be reachable at {host}:{port}: {err}",
                    host = host_str.as_str()
                );
            }
            Err(err) => {
                return Err(TestcontainersError::other(format!(
                    "failed to connect to sshd sidecar at {host}:{port}: {err}",
                    host = host_str.as_str()
                )))
            }
        }
    }
}

async fn forward_connection(
    channel: Channel<client::Msg>,
    host_port: u16,
    remote_port: u16,
    connected_address: String,
    originator_address: String,
    originator_port: u32,
    stop_flag: Arc<AtomicBool>,
) -> io::Result<()> {
    if stop_flag.load(Ordering::SeqCst) {
        return Ok(());
    }

    let mut stream = match TcpStream::connect(("127.0.0.1", host_port)).await {
        Ok(stream) => stream,
        Err(err) => {
            log::error!(
                "failed to connect to host port {host_port} for exposure tunnel (remote {remote_port} via {connected_address} from {originator_address}:{originator_port}): {err}"
            );
            return Err(err);
        }
    };

    if let Err(err) = stream.set_nodelay(true) {
        log::debug!("failed to configure tcp stream for host exposure port {host_port}: {err}");
    }

    let mut channel_stream = channel.into_stream();

    copy_bidirectional(&mut channel_stream, &mut stream).await?;

    if let Err(err) = stream.shutdown().await {
        log::trace!(
            "failed to shutdown tcp stream after host exposure proxy for port {remote_port}: {err}"
        );
    }

    Ok(())
}

fn map_ssh_error(context: &str, err: russh::Error) -> TestcontainersError {
    TestcontainersError::other(format!("{context}: {err}"))
}

#[derive(Debug)]
struct ForwardState {
    stop_flag: Arc<AtomicBool>,
    port_mapping: Mutex<HashMap<u16, u16>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl ForwardState {
    fn new() -> Self {
        Self {
            stop_flag: Arc::new(AtomicBool::new(false)),
            port_mapping: Mutex::new(HashMap::new()),
            tasks: Mutex::new(Vec::new()),
        }
    }

    fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }

    fn is_stopped(&self) -> bool {
        self.stop_flag.load(Ordering::SeqCst)
    }

    fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop_flag)
    }

    fn register_port(&self, remote_port: u16, host_port: u16) {
        self.port_mapping
            .lock()
            .expect("forward state port mapping lock poisoned")
            .insert(remote_port, host_port);
    }

    fn host_port(&self, remote_port: u16) -> Option<u16> {
        self.port_mapping
            .lock()
            .expect("forward state port mapping lock poisoned")
            .get(&remote_port)
            .copied()
    }

    fn add_task(&self, handle: JoinHandle<()>) {
        let mut tasks = self
            .tasks
            .lock()
            .expect("forward state task list lock poisoned");
        tasks.retain(|task| !task.is_finished());
        tasks.push(handle);
    }

    fn drain_tasks(&self) -> Vec<JoinHandle<()>> {
        self.tasks
            .lock()
            .expect("forward state task list lock poisoned")
            .drain(..)
            .collect()
    }
}

#[derive(Clone)]
struct HostExposeClient {
    state: Arc<ForwardState>,
}

impl HostExposeClient {
    fn new(state: Arc<ForwardState>) -> Self {
        Self { state }
    }
}

impl client::Handler for HostExposeClient {
    type Error = HostExposeError;

    fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        future::ready(Ok(true))
    }

    fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        _session: &mut client::Session,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        let state = Arc::clone(&self.state);
        let connected_address = connected_address.to_string();
        let originator_address = originator_address.to_string();
        async move {
            if state.is_stopped() {
                return Ok(());
            }

            let remote_port = match u16::try_from(connected_port) {
                Ok(port) => port,
                Err(_) => {
                    log::warn!(
                        "host port exposure received forwarded connection with invalid remote port: {connected_port}"
                    );
                    return Ok(());
                }
            };

            let Some(host_port) = state.host_port(remote_port) else {
                log::warn!(
                    "host port exposure received forwarded connection for unregistered port: {remote_port}"
                );
                return Ok(());
            };

            let stop_flag = state.stop_flag();
            let handle = tokio::spawn(async move {
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }

                if let Err(err) = forward_connection(
                    channel,
                    host_port,
                    remote_port,
                    connected_address,
                    originator_address,
                    originator_port,
                    stop_flag,
                )
                .await
                {
                    log::debug!(
                        "host port exposure proxy for remote port {remote_port} ended with error: {err}"
                    );
                }
            });

            state.add_task(handle);

            Ok(())
        }
    }
}

#[derive(Debug)]
enum HostExposeError {
    Ssh(russh::Error),
    Exposure(TestcontainersError),
}

impl From<russh::Error> for HostExposeError {
    fn from(err: russh::Error) -> Self {
        Self::Ssh(err)
    }
}

impl From<TestcontainersError> for HostExposeError {
    fn from(err: TestcontainersError) -> Self {
        Self::Exposure(err)
    }
}

impl From<HostExposeError> for TestcontainersError {
    fn from(err: HostExposeError) -> Self {
        match err {
            HostExposeError::Ssh(err) => TestcontainersError::other(format!("ssh error: {err}")),
            HostExposeError::Exposure(err) => err,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generates_passwords() {
        let password_a = format!("tc-{}", Ulid::new());
        let password_b = format!("tc-{}", Ulid::new());
        assert_ne!(password_a, password_b);
    }
}
