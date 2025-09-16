# Host access

Containers that participate in an integration test often need to call services
that run on the host machine: HTTP mocks, external databases, tracing agents,
or even your application under test. Docker assigns dynamic bridge IPs, so the
host address is not stable and varies across platforms.

`testcontainers` exposes the `with_exposed_host_port` and
`with_exposed_host_ports` helpers to declare which host ports should remain
reachable from the container. Calling either helper injects the
`host.testcontainers.internal` alias into `/etc/hosts`, allowing your code to
use a stable hostname together with the original host port number.

```rust
use std::net::TcpListener;
use testcontainers::{runners::AsyncRunner, GenericImage, ImageExt};

#[tokio::test]
async fn container_can_call_host() -> anyhow::Result<()> {
    // Example host process exposed on a well-known port
    let _listener = TcpListener::bind(("0.0.0.0", 18_080))?;

    let image = GenericImage::new("curlimages/curl", "latest")
        .with_cmd(["curl", "-sSf", "http://host.testcontainers.internal:18080"])
        .with_exposed_host_port(18_080);

    image.start().await?; // any non-zero exit would bubble up here
    Ok(())
}
```

## Automatic fallbacks

Recent Docker versions support the special `host-gateway` keyword. When the
engine exposes this feature, `testcontainers` forwards the keyword verbatim so
that no additional container is required.

Legacy environments (older Docker releases, Podman, some DinD setups) do not
understand `host-gateway`. In that case the library falls back to resolving the
bridge gateway address dynamically and injects the resolved IP instead. The
resulting alias is cached per network to avoid repeating the discovery probe.

If discovery fails, container creation returns an error together with context so
that you can fall back to manually wiring the host.

## Declaring host ports (future proofing)

The helpers record exposed ports on the `ContainerRequest`. Future fallbacks,
such as SSH sidecars, can leverage the same metadata without requiring any
changes to your tests.

## Limitations

* Host services must listen on addresses that are reachable from the bridge
  network (typically `0.0.0.0`).
* IPv6 support is not yet available.
* Rootless Docker and very restricted DinD environments may still require
  manual wiring until the planned SSH sidecar fallback lands.
