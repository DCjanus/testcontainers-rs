# 可选的主机端口暴露特性

> ⚠️ 提交正式 PR 前，请删除当前的规划文档：[`host-port-exposure-feature-plan.md`](host-port-exposure-feature-plan.md)。

## 原始诉求

### 长期需要保留
- 将新增的 SSH 反向隧道能力作为可选 feature，引入与关闭时均易于维护，避免复杂的 feature gate 组合。
- 尽量减少最终合并到 `origin/main` 的改动面，降低 reviewer 负担。
- 全程避免直接或间接依赖 `ring`，优先基于 `aws-lc-rs` 等替代方案实现加密能力。
- 在正式提交代码前删除本规划文档。
- 在代码功能稳定后、提交前于 `docs` 目录中新增一篇 Markdown 说明文档，参考 <https://java.testcontainers.org/features/networking/#exposing-host-ports-to-the-container> 的呈现方式，指导用户启用与使用该特性。

## AI 推论与补充
- 建议 feature 命名为 `host-port-exposure`，以便用户按需启用并保留扩展空间。
- 将 `russh` 设置为可选依赖，并确保其配置完全基于 `aws-lc-rs` 或其他非 `ring` 后端；同时不启用 `rsa` feature，以减少不必要的算法实现。
- 在未启用 feature 时不导出与主机端口暴露相关的 API 或类型，并通过编译期缺失来提示用户启用该 feature，避免运行时才发现错误。
- 将相关集成测试、示例和文档同样置于该 feature 下，避免默认构建受影响。
- 在 README 和 changelog 中补充启用方式与注意事项。

## 全局要求
- 所有提交信息必须使用英文语义化规范（如 `feat: ...`、`fix: ...`），并保持简洁精准地描述改动。
- 每次完成阶段性修改后，至少运行以下命令验证不同 feature 组合：
  ```bash
  cargo fmt --all
  cargo clippy --no-default-features
  cargo clippy --features host-port-exposure
  cargo clippy --features reusable-containers
  cargo clippy --features host-port-exposure,reusable-containers
  ```

## 背景
- [issue #821](https://github.com/testcontainers/testcontainers-rs/issues/821) 希望在不依赖 Docker Desktop DNS 技巧的前提下，实现容器访问宿主机端口。
- [PR #846](https://github.com/testcontainers/testcontainers-rs/pull/846) 通过引入 SSH sidecar 做反向隧道来满足这一需求。
- 由于 `russh` 默认依赖 `ring` 和 `rsa`，维护者希望将新能力放到可选 feature 中，并进一步倾向于完全避免这些依赖。

## 修改方案
- 将 `russh` 设为可选依赖，引入名为 `host-port-exposure` 的 cargo feature，开启时才启用 `russh`；同时配置其仅使用 `aws-lc-rs` 或其它非 `ring` 的加密后端，且不启用 `rsa` feature。
- 在 `core::containers::mod.rs` 中以 `#[cfg(feature = "host-port-exposure")]` 修饰 `mod host` 的声明，从根源上跳过整个模块与关联依赖，避免在未开启特性时仍编译相关代码。
- 将主机端口暴露相关的集成测试、示例和文档都放在同一 feature 下，并通过条件编译完全跳过这些代码，避免在未开启特性时破坏构建或测试。
- 在 crate README 与 changelog 中说明新特性，并告诉用户如何启用以使用主机端口暴露能力。

## 实施要点
- 通过条件编译控制 `with_exposed_host_port(s)` 等 API 的可用性：启用 feature 时正常暴露；关闭时通过 `cfg` gate 排除相关模块与 re-export，确保编译期就不会出现未实现的 API。
- 尽量控制改动范围，预计只需修改 `testcontainers/Cargo.toml`、`core/containers/host.rs`、`runners/async_runner.rs`、`core/containers/async_container.rs`、相关测试以及文档。
- 复用或增加一个仅在启用 `host-port-exposure` 时编译的集成测试，以确保 CI 能覆盖 SSH 隧道路径。
- 选择语义明确的 feature 名称（`host-port-exposure`），未来若出现其他隧道实现，也可以在同一特性下扩展，而不会让不需要的依赖回到默认构建。

## 详细实施步骤
1. **Cargo 配置**（已完成）
   - 将 `russh` 声明为 `optional = true`，关闭其默认特性，仅启用兼容 `aws-lc-rs` 的密码套件，确认完全移除对 `ring`、`rsa` 的隐式依赖。
   - 在 `[features]` 中新增 `host-port-exposure`，列出 `russh` 及所需的 `aws-lc-rs`、`bollard/aws-lc-rs` 等依赖；同步调整 `default` feature，避免在默认构建时引入 `russh`。
   - 为相关 dev-dependencies 和示例增加 `cfg(feature = "host-port-exposure")`，并在 `[[test]]` 节点上声明 `required-features = ["host-port-exposure"]`，确保默认测试集不触发额外依赖。

2. **核心模块条件编译**（已完成）
   - 在 `core/containers/mod.rs` 使用 `#[cfg(feature = "host-port-exposure")] mod host;`，并对对应的 `pub use` 与其他模块引用保持一致的条件编译处理。
   - `ContainerAsync` 结构体中的 `host_port_exposure` 字段、构造参数与 `Drop` 实现采用 `cfg(feature = "host-port-exposure")` 包裹；未启用特性时完全移除该字段，避免空引用。
   - 在 `AsyncRunner` 与 `SyncRunner` 中，将 `HostPortExposure::setup`、清理逻辑以及任何 `use super::host` 的语句放在 feature gate 内；必要时提供一个 `cfg(not(feature = ...))` 的轻量 helper 以确保其余代码路径仍能编译。

3. **API 与请求结构调整**（已完成）
   - 为 `ImageExt::with_exposed_host_port(s)`、`ContainerRequest::with_exposed_host_port(s)` 以及 `host_port_exposures` 字段和访问器添加 `#[cfg(feature = "host-port-exposure")]`，并更新文档注释提示所需 feature。
   - 梳理 `ContainerRequest` 的 `Default` / builder 逻辑，确认在 feature 关闭时不会出现未使用字段或 `serde` 反序列化错误；必要时使用 `cfg_attr` 维持序列化兼容性或提供 feature 专属结构体。

4. **测试与示例**（待完成）
   - 将 `tests/host_port_exposure.rs`、`tests/dual_stack_host_ports.rs` 等依赖该特性的测试通过 `#[cfg(feature = "host-port-exposure")]` 或 `required-features` 约束；同时检查 `testimages` 下相关镜像构建步骤是否需要 gate。
   - 为示例代码及文档中的命令添加 `--features host-port-exposure` 提示，确保用户在复制示例时不会遭遇编译失败。

5. **文档与发布说明**（待完成）
   - 在 `docs/features`（或合适位置）新增“暴露宿主机端口”说明页，包含启用方式、限制、常见错误；在 `mkdocs.yml`、`README.md`、`CHANGELOG.md` 中引用该页面。
   - 更新 crate-level 文档注释，使用 `cfg_attr(doc_cfg, doc(cfg(feature = "host-port-exposure")))` 标注公开 API，便于 docs.rs 显示 feature 依赖关系。

6. **验证与 CI**（待完成）
   - 本地执行 `cargo fmt`、`cargo clippy --no-default-features`、`cargo clippy --features host-port-exposure`、`cargo test --no-default-features`、`cargo test --features host-port-exposure`，验证两条构建路径均通过。
   - 在 CI 工作流中添加启用该 feature 的测试矩阵项，并检查 `docs` 构建在两种模式下是否需要额外 flag。
