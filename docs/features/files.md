# Files And Mounts

Rust Testcontainers lets you seed container filesystems before startup, retrieve artifacts produced inside containers, and bind host paths at runtime. The APIs mirror the ergonomics of the Java library while following idiomatic Rust patterns.

## Copying Files Into Containers

Use `ImageExt::with_copy_to` to stage files or directories before the container starts. Content can come from raw bytes or host paths:

```rust
use testcontainers::{GenericImage, WaitFor};

let project_assets = std::path::Path::new("tests/fixtures/assets");
let image = GenericImage::new("alpine", "latest")
    .with_wait_for(WaitFor::seconds(1))
    .with_copy_to("/opt/app/config.yaml", br#"mode = "test""#.to_vec())
    .with_copy_to("/opt/app/assets", project_assets);
```

Everything is packed into a TAR archive, so nested directories are preserved. The helper accepts either `Vec<u8>` or any path-like value that implements `CopyDataSource`.

## Copying Files From Running Containers

Use `copy_file_from` to pull data produced inside the container:

```rust
use tempfile::tempdir;
use testcontainers::{GenericImage, WaitFor};

let container = GenericImage::new("alpine", "latest")
    .with_cmd(["sh", "-c", "echo '42' > /tmp/result.txt && sleep 30"])
    .with_wait_for(WaitFor::seconds(1))
    .start()
    .await?;

let destination = tempdir()?.path().join("result.txt");
container
    .copy_file_from("/tmp/result.txt", &destination)
    .await?;
assert_eq!(tokio::fs::read_to_string(&destination).await?, "42\n");
```

- `copy_file_from` downloads the requested path, validates that the archive resolves to that regular file, and writes the payload directly to the supplied host path.
- Call `copy_file_from_to_bytes` when you prefer to capture the file contents in-memory: `let bytes = container.copy_file_from_to_bytes("/tmp/result.txt").await?;`.
- Both helpers apply the same validation semantics, returning an error when the container path resolves to a directory or the archive contains unexpected entries.

The blocking `Container` type offers the same pair of helpers.

## Mounts For Writable Workspaces

When a bind or tmpfs mount fits better than copy semantics, use the `Mount` helpers:

```rust
use std::path::Path;
use testcontainers::core::{mounts::Mount, AccessMode, MountType};

let host_data = Path::new("/var/tmp/integration-data");
let mount = Mount::bind(host_data, "/workspace")
    .with_mode(AccessMode::ReadWrite)
    .with_type(MountType::Bind);

let image = GenericImage::new("python", "3.13")
    .with_mount(mount)
    .with_cmd(["python", "/workspace/run.py"]);
```

Bind mounts share host state directly, while tmpfs mounts give you ephemeral in-memory storage for scratch data or caches.

## Selecting An Approach

- **Copy before startup** when you need deterministic inputs.
- **Copy from containers** to capture build artifacts, logs, or fixtures produced during a test run.
- **Mounts** when the container must read/write large amounts of data without re-tarring it every time.

Mixing these tools keeps tests hermetic while still letting you inspect outputs locally. Document each choice in code so teammates know whether data is ephemeral (`tmpfs`), seeded once (`with_copy_to`), or captured for later assertions (`copy_file_from`).
