#[cfg(test)]
use std::sync::Mutex as StdMutex;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    sync::OnceLock,
    time::Duration,
};

use bollard::models::{ContainerCreateBody, HostConfig, Network, SystemVersion};
use futures::StreamExt;
use log::{debug, warn};
use semver::Version as SemverVersion;
use tokio::sync::{Mutex, OnceCell};

use crate::core::{
    client::Client,
    containers::{ContainerRequest, HOST_ACCESS_ALIAS},
    error::{Result, TestcontainersError},
    Host, Image,
};

const GATEWAY_PROBE_IMAGE: &str = "alpine:3.20";
const GATEWAY_PROBE_SCRIPT: &str = r#"
if command -v ip >/dev/null 2>&1; then
  ip -4 route show default 2>/dev/null | awk 'NR==1 {print $3; exit}'
  ip route show default 2>/dev/null | awk 'NR==1 {print $3; exit}'
fi
if command -v route >/dev/null 2>&1; then
  route -n 2>/dev/null | awk 'NR==3 {print $2; exit}'
fi
"#;
const HOST_GATEWAY_API_MIN_MAJOR: u64 = 1;
const HOST_GATEWAY_API_MIN_MINOR: u64 = 41;
const HOST_GATEWAY_ENGINE_MIN_MAJOR: u64 = 20;
const HOST_GATEWAY_ENGINE_MIN_MINOR: u64 = 10;

static HOST_GATEWAY_SUPPORTED: OnceCell<bool> = OnceCell::const_new();
static GATEWAY_CACHE: OnceLock<Mutex<HashMap<String, Option<IpAddr>>>> = OnceLock::new();
#[cfg(test)]
static HOST_GATEWAY_OVERRIDE: OnceLock<StdMutex<Option<bool>>> = OnceLock::new();

#[cfg(test)]
fn host_gateway_override() -> &'static StdMutex<Option<bool>> {
    HOST_GATEWAY_OVERRIDE.get_or_init(|| StdMutex::new(None))
}

#[cfg(test)]
pub(crate) fn override_host_gateway_support(value: Option<bool>) {
    *host_gateway_override().lock().unwrap() = value;
}

#[derive(Debug, thiserror::Error)]
enum HostAccessError {
    #[error("failed to discover host gateway for network mode '{network:?}'")]
    MissingGateway { network: Option<String> },
    #[error("gateway probe did not return an IP address for network mode '{network:?}'")]
    ProbeOutput { network: Option<String> },
}

pub(crate) async fn resolve_extra_hosts<I: Image>(
    client: &Client,
    container_req: &ContainerRequest<I>,
) -> Result<Vec<(String, String)>> {
    let mut needs_gateway_resolution = false;
    let mut has_alias = false;

    for (key, host) in container_req.hosts() {
        if matches!(host, Host::HostGateway) {
            needs_gateway_resolution = true;
        }
        if key == HOST_ACCESS_ALIAS {
            has_alias = true;
        }
    }

    if !needs_gateway_resolution {
        return Ok(container_req
            .hosts()
            .map(|(key, host)| (key.into_owned(), host.to_string()))
            .collect());
    }

    if detect_host_gateway_support(client).await? {
        return Ok(container_req
            .hosts()
            .map(|(key, host)| (key.into_owned(), host.to_string()))
            .collect());
    }

    let network_mode = container_req.network().as_deref();
    let gateway_ip = discover_gateway_ip(client, network_mode).await?;
    let Some(gateway_ip) = gateway_ip else {
        return Err(TestcontainersError::other(
            HostAccessError::MissingGateway {
                network: network_mode.map(str::to_string),
            },
        ));
    };

    if has_alias {
        debug!(
            "Resolved {HOST_ACCESS_ALIAS} to gateway {gateway_ip} for network {:?}",
            network_mode
        );
    }

    Ok(container_req
        .hosts()
        .map(|(key, host)| match host {
            Host::Addr(addr) => (key.into_owned(), addr.to_string()),
            Host::HostGateway => (key.into_owned(), gateway_ip.to_string()),
        })
        .collect())
}

async fn detect_host_gateway_support(client: &Client) -> Result<bool> {
    #[cfg(test)]
    if let Some(value) = host_gateway_override().lock().unwrap().clone() {
        return Ok(value);
    }

    HOST_GATEWAY_SUPPORTED
        .get_or_try_init(|| async {
            Ok(host_gateway_supported_from_version(
                &client.docker_version().await?,
            ))
        })
        .await
        .map(|supported| *supported)
}

fn host_gateway_supported_from_version(version: &SystemVersion) -> bool {
    if let Some(api_version) = version.api_version.as_deref() {
        if api_supports_host_gateway(api_version) {
            return true;
        }
    }

    if let Some(engine_version) = version.version.as_deref() {
        if engine_supports_host_gateway(engine_version) {
            return true;
        }
    }

    false
}

fn api_supports_host_gateway(api_version: &str) -> bool {
    let mut parts = api_version.split('.');
    let major = parts
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .unwrap_or_default();
    let minor = parts
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .unwrap_or_default();

    major > HOST_GATEWAY_API_MIN_MAJOR
        || (major == HOST_GATEWAY_API_MIN_MAJOR && minor >= HOST_GATEWAY_API_MIN_MINOR)
}

fn engine_supports_host_gateway(engine_version: &str) -> bool {
    let cleaned = engine_version.trim();
    if let Ok(parsed) = SemverVersion::parse(cleaned) {
        return parsed.major > HOST_GATEWAY_ENGINE_MIN_MAJOR
            || (parsed.major == HOST_GATEWAY_ENGINE_MIN_MAJOR
                && parsed.minor >= HOST_GATEWAY_ENGINE_MIN_MINOR);
    }

    false
}

pub(crate) async fn discover_gateway_ip(
    client: &Client,
    network_mode: Option<&str>,
) -> Result<Option<IpAddr>> {
    let key = network_mode.unwrap_or_default().to_string();
    let cache = GATEWAY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    {
        let cache_guard = cache.lock().await;
        if let Some(cached) = cache_guard.get(&key) {
            return Ok(*cached);
        }
    }

    let discovered = try_discover_gateway(client, network_mode).await?;

    cache.lock().await.insert(key, discovered);

    Ok(discovered)
}

async fn try_discover_gateway(
    client: &Client,
    network_mode: Option<&str>,
) -> Result<Option<IpAddr>> {
    if let Some(ip) = inspect_gateway(client, network_mode).await? {
        return Ok(Some(ip));
    }

    run_gateway_probe(client, network_mode).await
}

async fn inspect_gateway(client: &Client, network_mode: Option<&str>) -> Result<Option<IpAddr>> {
    match network_mode {
        None | Some("") => inspect_network_gateway(client, "bridge").await,
        Some("host") => Ok(Some(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        Some(mode) if mode.starts_with("container:") => {
            inspect_container_gateway(client, mode.trim_start_matches("container:")).await
        }
        Some(mode) => inspect_network_gateway(client, mode).await,
    }
}

async fn inspect_network_gateway(client: &Client, network: &str) -> Result<Option<IpAddr>> {
    let network_details = match client.inspect_network(network).await {
        Ok(res) => res,
        Err(err) => {
            debug!("Failed to inspect network {network}: {err:?}");
            return Ok(None);
        }
    };

    extract_gateway_from_network(&network_details)
}

fn extract_gateway_from_network(network: &Network) -> Result<Option<IpAddr>> {
    let gateway = network
        .ipam
        .as_ref()
        .and_then(|ipam| ipam.config.as_ref())
        .and_then(|configs| {
            configs
                .iter()
                .filter_map(|cfg| cfg.gateway.as_deref())
                .find_map(|gw| gw.parse::<IpAddr>().ok())
        });

    Ok(gateway)
}

async fn inspect_container_gateway(client: &Client, container_id: &str) -> Result<Option<IpAddr>> {
    let container = match client.inspect(container_id).await {
        Ok(res) => res,
        Err(err) => {
            debug!("Failed to inspect container {container_id}: {err:?}");
            return Ok(None);
        }
    };

    let network_settings = match container.network_settings {
        Some(settings) => settings,
        None => return Ok(None),
    };

    if let Some(gateway) = network_settings
        .gateway
        .as_ref()
        .and_then(|g| g.parse::<IpAddr>().ok())
    {
        return Ok(Some(gateway));
    }

    let networks = match network_settings.networks {
        Some(networks) => networks,
        None => return Ok(None),
    };

    Ok(networks
        .values()
        .filter_map(|network| network.gateway.as_deref())
        .find_map(|gateway| gateway.parse::<IpAddr>().ok()))
}

async fn run_gateway_probe(client: &Client, network_mode: Option<&str>) -> Result<Option<IpAddr>> {
    client.pull_image(GATEWAY_PROBE_IMAGE).await?;

    let mut host_config = HostConfig {
        network_mode: network_mode.map(|mode| mode.to_string()),
        ..Default::default()
    };

    host_config.auto_remove = Some(false);

    let create_body = ContainerCreateBody {
        image: Some(GATEWAY_PROBE_IMAGE.into()),
        cmd: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            GATEWAY_PROBE_SCRIPT.trim().into(),
        ]),
        host_config: Some(host_config),
        ..Default::default()
    };

    let container_id = client.create_container(None, create_body).await?;

    let result = async {
        client.start_container(&container_id).await?;
        let exit_code = wait_for_container_exit(client, &container_id).await?;

        if exit_code != 0 {
            warn!(
                "Gateway probe container exited with non-zero status {exit_code} for network {:?}",
                network_mode
            );
        }

        let mut stdout_stream = client.stdout_logs(&container_id, false);
        let mut stdout = Vec::new();
        while let Some(chunk) = stdout_stream.next().await {
            let chunk = chunk.map_err(|err| TestcontainersError::other(err))?;
            stdout.extend_from_slice(&chunk);
        }

        let gateway = String::from_utf8_lossy(&stdout)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .and_then(|line| line.parse::<IpAddr>().ok());

        gateway.ok_or_else(|| {
            TestcontainersError::other(HostAccessError::ProbeOutput {
                network: network_mode.map(str::to_string),
            })
        })
    }
    .await;

    if let Err(err) = client.rm(&container_id).await {
        debug!(
            "Failed to remove gateway probe container {}: {err:?}",
            container_id
        );
    }

    result.map(Some)
}

async fn wait_for_container_exit(client: &Client, id: &str) -> Result<i64> {
    loop {
        match client.container_exit_code(id).await? {
            Some(code) => return Ok(code),
            None => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{images::generic::GenericImage, ImageExt};

    struct OverrideGuard;

    impl OverrideGuard {
        fn new(value: Option<bool>) -> Self {
            override_host_gateway_support(value);
            Self
        }
    }

    impl Drop for OverrideGuard {
        fn drop(&mut self) {
            override_host_gateway_support(None);
        }
    }

    #[tokio::test]
    async fn resolve_extra_hosts_use_keyword_when_supported() -> anyhow::Result<()> {
        let _guard = OverrideGuard::new(Some(true));
        let client = Client::lazy_client().await?;
        let request = GenericImage::new("alpine", "3.20").with_host_access();

        let hosts = resolve_extra_hosts(client.as_ref(), &request).await?;

        let (_, value) = hosts
            .into_iter()
            .find(|(key, _)| key == HOST_ACCESS_ALIAS)
            .expect("alias missing");

        assert_eq!(value, "host-gateway");
        Ok(())
    }

    #[tokio::test]
    async fn resolve_extra_hosts_use_ip_when_not_supported() -> anyhow::Result<()> {
        let _guard = OverrideGuard::new(Some(false));
        let client = Client::lazy_client().await?;
        let request = GenericImage::new("alpine", "3.20").with_host_access();

        let hosts = resolve_extra_hosts(client.as_ref(), &request).await?;

        let (_, value) = hosts
            .into_iter()
            .find(|(key, _)| key == HOST_ACCESS_ALIAS)
            .expect("alias missing");

        assert_ne!(value, "host-gateway");
        value.parse::<IpAddr>()?;
        Ok(())
    }
}
