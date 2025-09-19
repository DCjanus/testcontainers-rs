#![cfg(feature = "host-expose")]

use std::{
    io::{self, Write},
    net::{IpAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use ssh2::{Channel, Listener, Session};
use ulid::Ulid;

use crate::{
    core::{
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

use super::async_container::ContainerAsync;

pub(crate) const HOST_INTERNAL_ALIAS: &str = "host.testcontainers.internal";
const SSHD_IMAGE: &str = "testcontainers/sshd";
const SSHD_TAG: &str = "1.3.0";
const SSH_USERNAME: &str = "root";
const SSH_PORT: u16 = 22;

/// Manages the lifetime of the SSH reverse tunnels used to expose host ports.
pub(crate) struct HostPortExposure {
    _sidecar: Box<ContainerAsync<GenericImage>>,
    session: Session,
    stop_flag: Arc<AtomicBool>,
    workers: Vec<thread::JoinHandle<()>>,
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

        if requested_ports.iter().any(|port| *port == 0) {
            return Err(TestcontainersError::other(
                "host port exposure requires ports greater than zero",
            ));
        }

        if let Some(network) = container_req.network() {
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
                    "host port exposure is not supported for reusable containers",
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

        let ssh_host_port = sidecar.get_host_port_ipv4((SSH_PORT).tcp()).await?;
        let sidecar_ip = resolve_sidecar_ip(&sidecar).await?;

        container_req
            .hosts
            .insert(HOST_INTERNAL_ALIAS.to_string(), Host::Addr(sidecar_ip));

        let tcp_stream = connect_with_retry(ssh_host_port)?;

        let mut session = Session::new().map_err(|err| {
            TestcontainersError::other(format!("failed to create ssh session: {err}"))
        })?;
        session.set_tcp_stream(tcp_stream);
        session
            .handshake()
            .map_err(|err| TestcontainersError::other(format!("ssh handshake failed: {err}")))?;
        session
            .userauth_password(SSH_USERNAME, &password)
            .map_err(|err| {
                TestcontainersError::other(format!("ssh authentication failed: {err}"))
            })?;

        if !session.authenticated() {
            return Err(TestcontainersError::other(
                "ssh authentication for host port exposure failed",
            ));
        }

        session.set_keepalive(true, 10);

        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(requested_ports.len());

        for port in requested_ports {
            let (listener, bound_port) = session
                .channel_forward_listen(port, Some("0.0.0.0"), None)
                .map_err(|err| {
                    TestcontainersError::other(format!(
                        "failed to request remote port forwarding for {port}: {err}"
                    ))
                })?;

            if bound_port != port {
                log::warn!(
                    "remote sshd assigned different port for host exposure: requested {port}, bound {bound_port}"
                );
            }

            let worker_flag = Arc::clone(&stop_flag);
            let handle = thread::Builder::new()
                .name(format!("tc-host-expose-{port}"))
                .spawn(move || forward_port(worker_flag, listener, port))
                .map_err(|err| {
                    TestcontainersError::other(format!(
                        "failed to spawn host port exposure worker for port {port}: {err}"
                    ))
                })?;

            workers.push(handle);
        }

        Ok(Some(Self {
            _sidecar: Box::new(sidecar),
            session,
            stop_flag,
            workers,
        }))
    }

    pub(crate) fn shutdown(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);

        if let Err(err) =
            self.session
                .disconnect(None, "testcontainers host port exposure cleanup", None)
        {
            log::debug!("ssh disconnect during host exposure cleanup failed: {err}");
        }

        while let Some(handle) = self.workers.pop() {
            if let Err(err) = handle.join() {
                log::debug!("host port exposure worker join failed: {err:?}");
            }
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

fn connect_with_retry(port: u16) -> Result<TcpStream, TestcontainersError> {
    let addr = ("127.0.0.1", port);
    let mut attempts = 0;
    let max_attempts = 20;

    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => {
                stream.set_nodelay(true).map_err(|err| {
                    TestcontainersError::other(format!("failed to configure ssh tcp stream: {err}"))
                })?;
                return Ok(stream);
            }
            Err(err) if attempts < max_attempts => {
                attempts += 1;
                thread::sleep(Duration::from_millis(100));
                log::trace!("waiting for sshd sidecar to be reachable on port {port}: {err}");
            }
            Err(err) => {
                return Err(TestcontainersError::other(format!(
                    "failed to connect to sshd sidecar on port {port}: {err}"
                )))
            }
        }
    }
}

fn forward_port(stop: Arc<AtomicBool>, mut listener: Listener, host_port: u16) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok(channel) => handle_connection(channel, host_port),
            Err(err) => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }

                log::warn!("host port exposure listener for port {host_port} failed: {err}");
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn handle_connection(mut channel: Channel, host_port: u16) {
    match TcpStream::connect(("127.0.0.1", host_port)) {
        Ok(stream) => {
            if let Err(err) = proxy_stream(stream, &mut channel) {
                log::debug!(
                    "host port exposure proxy for port {host_port} ended with error: {err}"
                );
            }
        }
        Err(err) => {
            log::error!("failed to connect to host port {host_port} for exposure tunnel: {err}");
        }
    }
}

fn proxy_stream(stream: TcpStream, channel: &mut Channel) -> io::Result<()> {
    stream.set_nodelay(true).ok();
    let mut remote_read = channel.stream(0);
    let mut remote_write = remote_read.clone();

    let mut local_read = stream.try_clone()?;
    let mut local_write = stream;

    let writer = thread::spawn(move || -> io::Result<()> {
        io::copy(&mut local_read, &mut remote_write)?;
        remote_write.flush()
    });

    let copy_result = io::copy(&mut remote_read, &mut local_write);
    let _ = local_write.flush();

    match writer.join() {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => log::debug!("host port exposure upstream copy ended with io error: {err}"),
        Err(err) => log::debug!("host port exposure upstream copy thread panicked: {err:?}"),
    }

    let _ = channel.send_eof();
    let _ = channel.wait_eof();
    let _ = channel.close();
    let _ = channel.wait_close();

    copy_result.map(|_| ())
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
