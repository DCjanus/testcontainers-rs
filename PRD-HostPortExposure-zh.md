## PRD: 在容器内访问宿主机端口（host.testcontainers.internal）

参考与背景：见 GitHub 议题 “Support for Host Port Exposure (host.testcontainers.internal)” [#821](https://github.com/testcontainers/testcontainers-rs/issues/821)。

### 背景与目标

- 容器需要以统一主机名访问宿主机端口，降低测试网络配置复杂度。
- 提供最小 API：仅声明需要访问的宿主端口，其余自动完成。
- 实现路径唯一：SSHD 侧车 + SSH 反向端口转发（基于 russh）。

### 当前进展（2025-02-14）

- 功能已默认启用，`russh` 作为常规依赖，无需额外 feature。
- `ContainerRequest` / `ImageExt` 已新增 `with_exposed_host_port(s)` API。
- `AsyncRunner` 流程可自动：
  - 启动 `testcontainers/sshd:1.3.0` 侧车并与目标容器共享网络。
  - 生成随机 root 密码，通过密码认证建立单个 SSH 会话（开启 keepalive）。
  - 为每个声明端口创建反向转发任务，并在容器 Drop 时清理。
- 自动写入 `/etc/hosts`：`host.testcontainers.internal` → 侧车 IP，已验证 cargo check。

**待办**

- `SyncRunner` 支持、容器复用/跨网络等场景。
- 集成测试及用户文档（FAQ/排错）。
- 事件日志/metrics 的补充观测能力。

### 范围与非目标

- 不覆盖用户显式的 `with_host(...)` 设置。
- 首版仅支持 TCP；不覆盖跨主机编排场景。

---

## 能力与配置

- 固定主机名别名：`host.testcontainers.internal`（不可配置）。
- 固定侧车镜像：`testcontainers/sshd:1.3.0`。
- API（增量）：
  - `with_exposed_host_port(port: u16)`
  - `with_exposed_host_ports(ports: impl IntoIterator<Item = u16>)`

### 依赖策略（避免与 rustls 冲突）

- 功能默认编译并依赖纯 Rust 的 `russh`，不再需要 OpenSSL。
- 为兼容既有项目，保留空特性 `host-expose` / `host-expose-vendored-openssl`，启用与否均不影响编译结果。
- Cargo.toml（示意）：

```toml
[features]
default = []
host-expose = []
host-expose-vendored-openssl = ["host-expose"]

[dependencies]
russh = { version = "0.54", default-features = false, features = ["ring", "rsa"] }
```

- 说明：兼容特性仅用于保持 Cargo 配置不报错，功能总是可用。

---

## 技术实现

### 拓扑与数据流

- 侧车（sshd）与被测容器处于同一用户网络。
- 被测容器的 `/etc/hosts` 注入：`host.testcontainers.internal` → 侧车容器 IP。
- 宿主建立到侧车的 SSH 会话，并在“远端（侧车）”监听 `<remote_port>`，将流量经 SSH 通道回送到宿主 `127.0.0.1:<host_port>`（等价于 `ssh -R 0.0.0.0:<remote_port>:127.0.0.1:<host_port>`）。
- 默认同号映射：`remote_port == host_port`，容器内可直接访问 `host.testcontainers.internal:<host_port>`。

### sshd 要求（侧车镜像）

- `AllowTcpForwarding yes`
- `GatewayPorts clientspecified`（允许远端转发绑定到 0.0.0.0）
- 支持密码登录（当前实现为随机 root 密码，后续可扩展为密钥登录）

### 基于 russh 的实现（关键步骤）

- 会话建立：
  - `client::connect_stream` 建立基于 Tokio 的 SSH 会话，`authenticate_password("root", <random-password>)` 完成密码认证。
  - 配置 keepalive：`Config.keepalive_interval = Some(Duration::from_secs(10))`。
- 远端监听（等价于 -R）：
  - `handle.tcpip_forward("0.0.0.0", remote_port)` 请求监听端口，使用返回的 `bound_port` 建立映射。
- 接入与桥接：
  - `Handler::server_channel_open_forwarded_tcpip` 收到入站连接后，`tokio::spawn` 启动任务。
  - 任务内 `channel.into_stream()` 与宿主 `TcpStream` 使用 `tokio::io::copy_bidirectional` 双向转发数据。
- 生命周期与清理：
  - 记录侧车容器 ID 与 SSH 会话句柄；容器结束即关闭会话、销毁侧车与临时网络。
  - Drop 时标记停止标志、取消转发任务，并发送 `disconnect` 请求完成清理。

---

## 代码改动落点

- `testcontainers/src/core/containers/request.rs`
  - 新增：`host_port_exposures: Option<Vec<u16>>`。
- `testcontainers/src/core/image/image_ext.rs`
  - 新增：`with_exposed_host_port`、`with_exposed_host_ports`。
- `testcontainers/src/runners/{async_runner,sync_runner}.rs`
  - 创建/复用网络；启动侧车（`testcontainers/sshd:1.3.0`）。
  - 生成一次性密码（root），通过密码认证建立 SSH 会话并复用注册多个 `-R` 转发。
  - 注入 hosts：`host.testcontainers.internal` → 侧车 IP。
  - 生命周期：容器 Drop 时断开会话、回收转发线程与侧车。

---

## 运行时流程

1. 解析配置（API 端口清单）。
2. 准备网络与侧车；生成随机密码、建立 SSH 会话与反向端口转发；写入 hosts。
3. 启动被测容器，进入既有 wait/inspect 流程。
4. 清理：关闭 SSH 会话，停止侧车与临时网络（按复用策略）。

---

## 错误处理与可观测性

- 记录事件：侧车启动/就绪、会话建立、远端监听、桥接错误、清理动作。
- 失败即报错：在被测容器启动前抛出明确错误（含端口与会话信息）。

---

## 测试

- 集成测试：
  - 宿主起临时 HTTP 服务（随机端口）；容器内 `curl http://host.testcontainers.internal:<port>` 返回 200。
  - 断开 SSH 会话后访问失败（验证清理与生命周期）。
- 单元测试：
  - API 解析（端口清单）。
  - hosts 注入不覆盖用户显式设置。
  - 侧车生命周期管理（创建/清理）。

---

## 性能与安全

- 额外 1 个侧车容器 + 1 条 SSH 会话；仅在声明端口时启用。
- 使用一次性随机密码、仅开启必要反向转发；会话随测试生命周期清理。

---

## 风险与替代

- 受限环境可能阻断 SSH/转发；文档提供手动网络与别名配置作为替代路径。

---

## 验收标准（DoD）

- 默认配置下示例用例通过；文档包含排错与常见问题。
- API 与项目风格一致；lint/CI 通过。
