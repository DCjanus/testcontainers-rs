use std::{
    io::Write,
    net::{TcpListener, TcpStream},
    thread,
};

use anyhow::Result;
use testcontainers::{
    core::{ExecCommand, Host, WaitFor},
    runners::AsyncRunner,
    GenericImage, ImageExt, TestcontainersError,
};
use ulid::Ulid;

const ALPINE_IMAGE: &str = "alpine";
const ALPINE_TAG: &str = "3.19";
const HOST_ALIAS: &str = "host.testcontainers.internal";

fn host_url(port: u16) -> String {
    format!("http://{HOST_ALIAS}:{port}")
}

fn base_alpine_image() -> GenericImage {
    GenericImage::new(ALPINE_IMAGE, ALPINE_TAG)
}

fn wget_host(port: u16) -> ExecCommand {
    let url = host_url(port);
    ExecCommand::new(vec!["wget".into(), "-qO-".into(), url])
}

fn wget_host_with_timeout(port: u16) -> ExecCommand {
    let mut args = vec![
        "wget".to_string(),
        "-qO-".to_string(),
        "-T".to_string(),
        "2".to_string(),
        "-t".to_string(),
        "1".to_string(),
    ];
    args.push(host_url(port));
    ExecCommand::new(args)
}

fn ping_once(target: &str) -> ExecCommand {
    ExecCommand::new(["ping", "-c", "1", target])
}

fn respond_once(mut stream: TcpStream, body: &'static str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

/// Verifies a single container can reach a host service exposed through one requested port.
#[tokio::test]
async fn exposes_single_host_port() -> Result<()> {
    let _ = pretty_env_logger::try_init();

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();

    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            respond_once(stream, "hello-from-host");
        }
    });

    let image = base_alpine_image()
        .with_entrypoint("/bin/sh")
        .with_cmd(["-c", "sleep 30"])
        .with_exposed_host_port(port);

    let container = image.start().await?;

    let mut exec_result = container.exec(wget_host(port)).await?;

    let body = exec_result.stdout_to_vec().await?;
    assert_eq!(body, b"hello-from-host");

    Ok(())
}

/// Ensures multiple host ports requested by the same container tunnel traffic to the correct services.
#[tokio::test]
async fn exposes_multiple_host_ports() -> Result<()> {
    let _ = pretty_env_logger::try_init();

    let listener_a = TcpListener::bind(("127.0.0.1", 0))?;
    let port_a = listener_a.local_addr()?.port();

    let listener_b = TcpListener::bind(("127.0.0.1", 0))?;
    let port_b = listener_b.local_addr()?.port();

    thread::spawn(move || {
        if let Ok((stream, _)) = listener_a.accept() {
            respond_once(stream, "alpha");
        }
    });

    thread::spawn(move || {
        if let Ok((stream, _)) = listener_b.accept() {
            respond_once(stream, "bravo");
        }
    });

    let image = base_alpine_image()
        .with_entrypoint("/bin/sh")
        .with_cmd(["-c", "sleep 30"])
        .with_exposed_host_ports([port_a, port_b]);

    let container = image.start().await?;

    let mut exec_a = container.exec(wget_host(port_a)).await?;

    let mut exec_b = container.exec(wget_host(port_b)).await?;

    let body_a = exec_a.stdout_to_vec().await?;
    let body_b = exec_b.stdout_to_vec().await?;

    assert_eq!(body_a, b"alpha");
    assert_eq!(body_b, b"bravo");

    Ok(())
}

/// Confirms configuring `host.testcontainers.internal` manually prevents host port exposure setup.
#[tokio::test]
async fn fails_when_alias_conflicts() {
    let image = base_alpine_image()
        .with_host(HOST_ALIAS, Host::HostGateway)
        .with_exposed_host_port(8080);

    let start_err = image.start().await.unwrap_err();
    match start_err {
        TestcontainersError::Other(message) => {
            let msg = message.to_string();
            assert!(
                msg.contains("host port exposure"),
                "unexpected error message: {msg}"
            );
        }
        other => panic!("unexpected error variant: {:?}", other),
    }
}

/// Validates the runner rejects attempts to expose the reserved SSH port used by the sidecar.
#[tokio::test]
async fn fails_when_exposing_reserved_port() {
    let image = base_alpine_image().with_exposed_host_port(22);

    let start_err = image.start().await.unwrap_err();
    match start_err {
        TestcontainersError::Other(message) => {
            let msg = message.to_string();
            assert!(
                msg.contains("SSH port is reserved"),
                "unexpected error message: {msg}"
            );
        }
        other => panic!("unexpected error variant: {:?}", other),
    }
}

/// Checks two containers on the same user network receive only their own host port tunnels while remaining mutually reachable.
#[tokio::test]
async fn host_port_exposure_is_scoped_per_container() -> Result<()> {
    let _ = pretty_env_logger::try_init();

    let first_host_listener = TcpListener::bind(("127.0.0.1", 0))?;
    let first_host_port = first_host_listener.local_addr()?.port();

    let second_host_listener = TcpListener::bind(("127.0.0.1", 0))?;
    let second_host_port = second_host_listener.local_addr()?.port();

    thread::spawn(move || {
        if let Ok((stream, _)) = first_host_listener.accept() {
            respond_once(stream, "first-host-service");
        }
    });

    thread::spawn(move || {
        if let Ok((stream, _)) = second_host_listener.accept() {
            respond_once(stream, "second-host-service");
        }
    });

    let suffix = Ulid::new().to_string().to_lowercase();
    let network_name = format!("tc-host-port-net-{suffix}");
    let first_container_name = format!("host-port-first-{suffix}");
    let second_container_name = format!("host-port-second-{suffix}");

    let base_image = base_alpine_image().with_wait_for(WaitFor::seconds(1));

    let first_container = base_image
        .clone()
        .with_entrypoint("/bin/sh")
        .with_cmd(["-c", "sleep 60"])
        .with_container_name(first_container_name.clone())
        .with_network(network_name.clone())
        .with_exposed_host_port(first_host_port)
        .start()
        .await?;

    let second_container = base_image
        .with_entrypoint("/bin/sh")
        .with_cmd(["-c", "sleep 60"])
        .with_container_name(second_container_name.clone())
        .with_network(network_name)
        .with_exposed_host_port(second_host_port)
        .start()
        .await?;

    let mut first_container_to_first_host =
        first_container.exec(wget_host_with_timeout(first_host_port)).await?;
    let body = String::from_utf8(first_container_to_first_host.stdout_to_vec().await?)?;
    assert_eq!(body, "first-host-service");

    let exit = first_container
        .exec(wget_host_with_timeout(second_host_port))
        .await?
        .exit_code()
        .await?;
    assert_ne!(exit, Some(0));

    let mut second_container_to_second_host =
        second_container.exec(wget_host_with_timeout(second_host_port)).await?;
    let body =
        String::from_utf8(second_container_to_second_host.stdout_to_vec().await?)?;
    assert_eq!(body, "second-host-service");

    let exit = second_container
        .exec(wget_host_with_timeout(first_host_port))
        .await?
        .exit_code()
        .await?;
    assert_ne!(exit, Some(0));

    let mut first_container_reachability =
        first_container.exec(ping_once(&second_container_name)).await?;
    let _ = first_container_reachability.stdout_to_vec().await?;
    let exit = first_container_reachability.exit_code().await?;
    assert_eq!(exit, Some(0));

    let mut second_container_reachability =
        second_container.exec(ping_once(&first_container_name)).await?;
    let _ = second_container_reachability.stdout_to_vec().await?;
    let exit = second_container_reachability.exit_code().await?;
    assert_eq!(exit, Some(0));

    Ok(())
}
