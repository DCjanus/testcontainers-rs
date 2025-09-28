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
- 在未启用 feature 时提供轻量 stub，在运行时提示用户开启 feature，确保 API 签名保持兼容。
- 将相关集成测试、示例和文档同样置于该 feature 下，避免默认构建受影响。
- 在 README 和 changelog 中补充启用方式与注意事项。

## 背景
- [issue #821](https://github.com/testcontainers/testcontainers-rs/issues/821) 希望在不依赖 Docker Desktop DNS 技巧的前提下，实现容器访问宿主机端口。
- [PR #846](https://github.com/testcontainers/testcontainers-rs/pull/846) 通过引入 SSH sidecar 做反向隧道来满足这一需求。
- 由于 `russh` 默认依赖 `ring` 和 `rsa`，维护者希望将新能力放到可选 feature 中，并进一步倾向于完全避免这些依赖。

## 修改方案
- 将 `russh` 设为可选依赖，引入名为 `host-port-exposure` 的 cargo feature，开启时才启用 `russh`；同时配置其仅使用 `aws-lc-rs` 或其它非 `ring` 的加密后端，且不启用 `rsa` feature。
- 在 `core::containers::host` 中使用 `#[cfg(feature = "host-port-exposure")]` 包裹现有 `HostPortExposure` 实现；未开启特性时使用轻量 stub，在调用 `with_exposed_host_port(s)` 时返回清晰的错误信息。
- 将主机端口暴露相关的集成测试、示例和文档都放在同一 feature 下，避免在未开启特性时破坏构建或测试。
- 在 crate README 与 changelog 中说明新特性，并告诉用户如何启用以使用主机端口暴露能力。

## 实施要点
- 保持 `with_exposed_host_port(s)` API 始终可用，这样现有代码依旧能编译；当未开启特性时通过 stub 在运行时提示需要启用该 feature。
- 尽量控制改动范围，预计只需修改 `testcontainers/Cargo.toml`、`core/containers/host.rs`、`runners/async_runner.rs`、`core/containers/async_container.rs`、相关测试以及文档。
- 复用或增加一个仅在启用 `host-port-exposure` 时编译的集成测试，以确保 CI 能覆盖 SSH 隧道路径。
- 选择语义明确的 feature 名称（`host-port-exposure`），未来若出现其他隧道实现，也可以在同一特性下扩展，而不会让不需要的依赖回到默认构建。
