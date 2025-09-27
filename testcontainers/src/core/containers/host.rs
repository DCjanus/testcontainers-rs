use std::{
    convert::TryFrom,
    future,
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
const SSH_PORT: u16 = 22;

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
        // Stage 1: validate the request and derive the parameters we need later.
        let Some(plan) = prepare_host_exposure(container_req)? else {
            return Ok(None);
        };

        // Stage 2: start the SSH sidecar that powers the reverse tunnels.
        let sidecar = spawn_sshd_sidecar(&plan).await?;
        let bridge_ip = sidecar.get_bridge_ip_address().await?;

        container_req
            .hosts
            .insert(HOST_INTERNAL_ALIAS.to_string(), Host::Addr(bridge_ip));

        // Stage 3: establish the SSH session and authenticate against the sidecar.
        let mut ssh_connection = establish_ssh_connection(&sidecar, &plan).await?;

        // Stage 4: request remote port forwards for every requested host port.
        register_requested_ports(&plan.requested_ports, &mut ssh_connection.handle).await?;

        let SshConnection {
            handle: ssh_handle,
            state,
        } = ssh_connection;

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

struct SshConnection {
    handle: client::Handle<HostExposeClient>,
    state: Arc<ForwardState>,
}

struct HostExposurePlan {
    requested_ports: Vec<u16>,
    password: String,
    network: Option<String>,
    ssh_username: &'static str,
    ssh_port: u16,
    ssh_max_attempts: u32,
    ssh_retry_delay: Duration,
    ssh_max_retry_delay: Duration,
    ssh_image: &'static str,
    ssh_tag: &'static str,
}

fn prepare_host_exposure<I: Image>(
    container_req: &mut ContainerRequest<I>,
) -> Result<Option<HostExposurePlan>, TestcontainersError> {
    let mut requested_ports = match container_req
        .host_port_exposures()
        .map(|ports| ports.to_vec())
    {
        Some(ports) if !ports.is_empty() => ports,
        _ => return Ok(None),
    };

    // Ensure port list is deduplicated and does not include reserved entries.
    requested_ports.sort_unstable();
    requested_ports.dedup();

    if requested_ports.contains(&0) {
        return Err(TestcontainersError::other(
            "host port exposure requires ports greater than zero (port 0 is invalid)",
        ));
    }

    if requested_ports.contains(&SSH_PORT) {
        return Err(TestcontainersError::other(format!(
            "host port exposure does not support exposing port {} (SSH port is reserved)",
            SSH_PORT
        )));
    }

    if container_req.hosts.contains_key(HOST_INTERNAL_ALIAS) {
        return Err(TestcontainersError::other(
            "host port exposure is not supported when 'host.testcontainers.internal' is already defined",
        ));
    }

    let network = container_req.network().clone();
    if let Some(network_name) = network.as_deref() {
        if network_name == "host" {
            return Err(TestcontainersError::other(
                "host port exposure is not supported with host network mode",
            ));
        }

        if network_name.starts_with("container:") {
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

    Ok(Some(HostExposurePlan {
        requested_ports,
        password,
        network,
        ssh_username: "root",
        ssh_port: SSH_PORT,
        ssh_max_attempts: 20,
        ssh_retry_delay: Duration::from_millis(100),
        ssh_max_retry_delay: Duration::from_millis(2000),
        ssh_image: "testcontainers/sshd",
        ssh_tag: "1.3.0",
    }))
}

async fn spawn_sshd_sidecar(
    plan: &HostExposurePlan,
) -> Result<ContainerAsync<GenericImage>, TestcontainersError> {
    // Future improvement: swap the SSHD sidecar with a purpose-built container or
    // lightweight SOCKS5 proxy to unlock features like UDP forwarding while keeping
    // host port exposure flexible.
    let mut sshd = GenericImage::new(plan.ssh_image, plan.ssh_tag)
        .with_exposed_port(plan.ssh_port.tcp())
        .with_wait_for(WaitFor::seconds(1))
        .with_env_var("PASSWORD", plan.password.clone());

    if let Some(network) = plan.network.as_deref() {
        sshd = sshd.with_network(network);
    }

    sshd.start().await
}

async fn establish_ssh_connection(
    sidecar: &ContainerAsync<GenericImage>,
    plan: &HostExposurePlan,
) -> Result<SshConnection, TestcontainersError> {
    let ssh_host = sidecar.get_host().await?;
    let ssh_host_port = match ssh_host {
        UrlHost::Domain(_) => sidecar.get_host_port_ipv4(plan.ssh_port.tcp()).await?, // XXX: do we need to handle domain with IPv6 only?
        UrlHost::Ipv4(_) => sidecar.get_host_port_ipv4(plan.ssh_port.tcp()).await?,
        UrlHost::Ipv6(_) => sidecar.get_host_port_ipv6(plan.ssh_port.tcp()).await?,
    };

    let tcp_stream = connect_with_retry(&ssh_host, ssh_host_port, plan).await?;

    let config = client::Config {
        nodelay: true,
        keepalive_interval: Some(Duration::from_secs(10)),
        ..Default::default()
    };
    let state: Arc<ForwardState> = Arc::new(ForwardState::new());
    let handler = HostExposeClient::new(Arc::clone(&state));
    let config = Arc::new(config);

    let mut handle = client::connect_stream(config, tcp_stream, handler)
        .await
        .map_err(TestcontainersError::from)?;

    let auth_result = handle
        .authenticate_password(plan.ssh_username, &plan.password)
        .await
        .map_err(|err| map_ssh_error("SSH authentication failed for host port exposure", err))?;

    if !auth_result.success() {
        return Err(TestcontainersError::other(
            "SSH authentication failed for host port exposure - check SSHD container logs and credentials",
        ));
    }

    Ok(SshConnection { handle, state })
}

async fn register_requested_ports(
    requested_ports: &[u16],
    ssh_handle: &mut client::Handle<HostExposeClient>,
) -> Result<(), TestcontainersError> {
    for port in requested_ports {
        let bound_port = ssh_handle
            .tcpip_forward("0.0.0.0", u32::from(*port))
            .await
            .map_err(|err| {
                map_ssh_error(
                    &format!("failed to request remote port forwarding for {port}"),
                    err,
                )
            })?;

        let bound_port = u16::try_from(bound_port)
            .expect("remote sshd assigned port outside the valid host exposure range");

        if bound_port != *port {
            return Err(TestcontainersError::other(format!(
                "host port exposure required bound port {port}, but sshd assigned {bound_port}"
            )));
        }
    }

    Ok(())
}

impl Drop for HostPortExposure {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn connect_with_retry(
    host: &UrlHost,
    port: u16,
    plan: &HostExposurePlan,
) -> Result<TcpStream, TestcontainersError> {
    let host_str = host.to_string();
    let mut attempts = 0;
    let mut delay = plan.ssh_retry_delay;

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
            Err(err) if attempts < plan.ssh_max_attempts => {
                attempts += 1;
                sleep(delay).await;
                delay = std::cmp::min(delay * 2, plan.ssh_max_retry_delay);
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
    // Abort quickly when a shutdown is already in progress to avoid spawning new work.
    if stop_flag.load(Ordering::SeqCst) {
        return Ok(());
    }

    // Establish a local TCP connection that mirrors the SSH reverse tunnel.
    let mut stream = match TcpStream::connect(("localhost", host_port)).await {
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

    // Pump bytes in both directions until either side closes the connection.
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
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl ForwardState {
    fn new() -> Self {
        Self {
            stop_flag: Arc::new(AtomicBool::new(false)),
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
    ) -> impl future::Future<Output = Result<bool, Self::Error>> + Send {
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
    ) -> impl future::Future<Output = Result<(), Self::Error>> + Send {
        let state = Arc::clone(&self.state);
        let connected_address = connected_address.to_string();
        let originator_address = originator_address.to_string();
        async move {
            if state.is_stopped() {
                return Ok(());
            }

            let remote_port = u16::try_from(connected_port)
                .expect("forwarded connection reported port outside u16 range");

            let stop_flag = state.stop_flag();
            let handle = tokio::spawn(async move {
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }

                if let Err(err) = forward_connection(
                    channel,
                    remote_port,
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
