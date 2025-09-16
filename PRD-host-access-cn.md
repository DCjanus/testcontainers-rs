## PRD：主机端口暴露与 host.testcontainers.internal 支持（testcontainers-rs）

- **文档状态**：草案（Draft）
- **作者**：@DCjanus
- **关联议题**：[feature request: Support for Host Port Exposure (host.testcontainers.internal)](https://github.com/testcontainers/testcontainers-rs/issues/821)
- **目标版本**：vX.Y.Z（待定）

### 背景

在集成测试中，容器内的被测应用常需要访问运行在宿主机上的服务（本地 mock、数据库、调试代理等）。Testcontainers Java 提供了统一主机名 `host.testcontainers.internal` 的能力，使容器内可稳定访问宿主机端口，并在不同平台（Docker Desktop、Linux、DinD）间自动适配。

testcontainers-rs 目前支持 `with_host(key, Host::HostGateway)`，可以手动映射 `--add-host=...:host-gateway`，但缺少：

- 统一的默认主机名（`host.testcontainers.internal`）与便捷 API；
- 在不支持 `host-gateway` 的环境中自动回退的能力；
- 面向用户的文档与测试用例。

### 目标（Goals）

- 提供开箱即用的宿主访问能力：在容器内通过 `host.testcontainers.internal:<port>` 访问宿主机服务。
- 以最小 API 增量融入现有风格，便捷且可组合。
- 跨平台适配：优先原生支持，其次自动发现网关 IP；后续支持 SSH sidecar 回退。

### 非目标（Non-Goals）

- 首个版本不实现 SSH 反向端口转发 sidecar；仅在文档中说明规划与限制。
- 不解决宿主服务自身的监听/防火墙/鉴权问题。
- 暂不覆盖 IPv6 专用路径。

### 用户故事 / 用例

- 作为测试开发者，我可以启动一个本地 HTTP mock（监听 0.0.0.0:18080），容器内应用可通过 `http://host.testcontainers.internal:18080` 调用它。
- 在 CI 上（Docker Desktop 或 Linux+docker 20.10+），无需修改测试脚本，即可工作。
- 在不支持 `host-gateway` 的环境中，库自动降级为将 `host.testcontainers.internal` 解析到网关 IP。

### API 设计

在 `ImageExt` 引入 Host Access 配置对象，同时保持现有 API 风格：

- `with_host_access(self) -> ContainerRequest<I>`
  - 为容器注入统一主机名：等价于 `with_host("host.testcontainers.internal", Host::HostGateway)`；
  - 在创建容器时做能力探测：不支持 `host-gateway` 时自动改写为具体网关 IP（`Host::Addr(IpAddr)`).
- `with_host_access_config(self, config: HostAccess)`
  - 接受一个 `HostAccess` builder，可声明是否注入 alias 以及需要暴露的宿主机端口集合；
  - `HostAccess::alias_only()` / `HostAccess::disabled()` / `.expose_ports([...])` 等方法便于组合；
- 继续保留 `with_exposed_host_port(s)` 作为便捷包装：内部等价于在现有配置上追加端口声明。

示例（Rust）：

```rust
use testcontainers::{GenericImage, ImageExt};
use std::net::TcpListener;

// 宿主侧启动一个本地服务（示意）
let _listener = TcpListener::bind(("0.0.0.0", 18080)).unwrap();

// 容器侧访问宿主：
let image = GenericImage::new("alpine", "3.20")
    .with_cmd(["sh", "-c", "wget -qO- http://host.testcontainers.internal:18080/health || true"])
    .with_host_access();

let _container = image.start()?; // 或 .start().await
```

### 行为定义与优先级

1. 原生支持（优先级最高）

- 条件：Docker Desktop（默认支持）或 Linux Docker ≥ 20.10 支持 `host-gateway` 关键字。
- 行为：`/etc/hosts` 注入 `host.testcontainers.internal:host-gateway`，无需额外转发。

2. 动态网关发现（兼容路径）

- 条件：检测到 `host-gateway` 不可用。
- 行为：在与目标容器相同网络/模式下启动一次临时探测容器，执行 `ip route` 获取默认网关 IP，
  将 `host.testcontainers.internal` 改写为该 IP（`/etc/hosts`）。

3. SSH sidecar 回退（第二阶段规划）

- 条件：以上两种方式不可用（如部分 rootless、复杂 DinD）。
- 行为：创建 `sshd` sidecar（加入同一网络），通过远程端口转发打通；本 PR 不实现，仅文档预告。

### 兼容性与约束

- 网络模式：`bridge`、自定义 bridge 网络、`host`、`container:<id>` 均应不崩溃；`/etc/hosts` 注入在共享网络命名空间场景仍然生效。
- 宿主服务需监听可达地址（推荐 0.0.0.0）。
- Rootless Docker、DinD、严格防火墙环境可能存在可达性差异；首版通过“网关发现”尽力兼容。
- IPv6：暂不覆盖；未来若需要，可扩展 `Host` 与发现逻辑。

### 技术方案概述

- 新增模块：`core/host_access.rs`
  - `detect_host_gateway_support(docker: &Docker) -> bool`
    - 通过版本/平台信息判断 `host-gateway` 支持度（避免破坏性试错）。
  - `discover_gateway_ip(docker: &Docker, network_mode: Option<&str>) -> Option<IpAddr>`
    - 启动轻量探测容器（如 `alpine`），运行 `ip route`，提取默认网关；带最小缓存（按 network）。
- 扩展 `ImageExt`（`core/image/image_ext.rs`）
  - 增加 `with_host_access_config` 与 `HostAccess` builder；
  - `with_host_access`、`with_exposed_host_port(s)` 作为 builder 的便捷包装；
- `ContainerRequest` 新增 `host_access: Option<HostAccess>` 字段与访问器（仅存储语义）。
- 创建容器流程（`runners/async_runner.rs`）
  - 在拼装 `ContainerCreateBody` 前检查是否启用 host access；
  - 若 `host-gateway` 不可用，则通过 `discover_gateway_ip` 将 `extra_hosts` 改写为具体 IP。
- 不变更现有端口映射逻辑，确保向后兼容。

### 测试计划

- 单元/契约测试：
  - `with_host_access()` -> `HostConfig.extra_hosts` 包含 `host.testcontainers.internal`；
  - 在“模拟不支持 host-gateway”路径下，断言写入了具体 IP 字符串。
- 端到端测试（可 `#[ignore]` 或以环境变量控制）：
  - 宿主启动 HTTP 服务；容器内 `wget http://host.testcontainers.internal:<port>` 成功。
  - 覆盖 `bridge` 与自定义网络；`host`/`container:<id>` 至少跑一条 smoke。

### 发布与迁移

- 向后兼容：新增方法不影响现有调用。
- 文档：在 `docs/features/` 新增“Host Port Exposure”，在 quickstart 中加入简单示例。
- 版本：Minor 版本升级（新增特性）。

### 指标与验收

- 在 Docker Desktop 与 Linux（≥20.10）环境下，E2E 测试 100% 通过。
- 常见 CI 环境（GitHub Actions ubuntu-latest）能稳定访问宿主端口。
- 单测覆盖创建配置与能力探测关键分支。

### 风险与缓解

- 版本/平台判定不准：采用保守默认+探测回退，失败时给出清晰日志，指导手动 `with_host(..., Host::Addr(...))`。
- 探测容器带来时间开销：做结果缓存，并仅在必要时执行。
- Rootless/隔离环境仍不可达：文档明确限制，Phase 2 提供 SSH sidecar。

### 里程碑

- Phase 1（本 PR）
  - API：`with_host_access`、`with_host_access_config`、`HostAccess` builder 与便捷方法；
  - 能力探测 + 网关发现；单测 + 可选 E2E；文档初版。
- Phase 2（后续 PR）
  - SSH sidecar 与远程端口转发；更全面的集成测试矩阵。

### 参考

- 议题与 Java 方案背景：[issue #821](https://github.com/testcontainers/testcontainers-rs/issues/821)
