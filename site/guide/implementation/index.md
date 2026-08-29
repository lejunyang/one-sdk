# 实现原理

本节面向希望审阅、扩展或排查 osdk 的开发者，按能力拆解当前实现。用户指南回答“怎么用”，这里回答一次请求如何被解析、验证、提交，以及各层明确不保证什么。

## 架构主线

典型的归档型 SDK 安装会依次经过以下边界：

```text
CLI 与分层配置
  -> 项目版本发现、别名展开与精确版本解析
  -> backend 生成候选来源和 InstallPlan
  -> 下载缓存与断点续传
  -> checksum / signature / 可选 attestation 验证
  -> 在路径约束下解压到临时目录
  -> BLAKE3 内容寻址存储（CAS）
  -> hardlink / reflink / copy 物化
  -> receipt 与完成标记
  -> shim、激活环境和项目 lock
```

这不是所有 backend 都必须逐字复用的执行路径。统一接口允许 Rust 等委托型 backend 调用受控的上游管理器；模型资产也有独立的 provider、manifest 和快照流程。共同约束来自 [`Backend` trait](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs)、[安装管线](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) 和 [CLI 编排层](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs)。

## 按能力阅读

| 页面 | 重点 |
| --- | --- |
| [版本解析](./resolution) | 配置发现优先级、别名、范围、预发布版本与 package manager 发现 |
| [安装管线](./installation) | 并发编排、下载、验证、解压、提交与失败清理 |
| [激活、shim 与锁文件](./activation-lockfile) | 热路径解析、可逆 shell 状态、项目信任与平台化 lock |
| [下载源与项目 Registry](./sources-registries) | 两套控制面、测速排序、凭据边界和单次启动语义 |
| [HTTP 制品 backend](./http-artifacts) | 严格内联 URL 解析、精确身份、SHA-256 缓存重放、安全解压与发布 |
| [原生容器诊断与操作](./containers) | 运行时选择、匿名 OCI 校验、直接 pull 启动、原生 plan/preview 身份与 cache 所有权 |
| [存储与缓存](./storage-cache) | SDK/模型 CAS、物化回退、下载缓存和各 manager 原生缓存 |
| [校验与供应链边界](./verification) | checksum、Minisign、GitHub Artifact Attestation 与归档安全边界 |
| [Backend 与模型 Provider](./backends-models) | 内置/声明式/GitHub backend，以及 Hugging Face、ModelScope 模型快照 |
| [npm 开发工具](./npm-tools) | `npm:<package>` 身份、项目/全局/隔离安装器、脚本策略、原生 lock、inventory 与冲突拒绝 |
| [可靠性与并发](./reliability) | 锁、原子发布、重试、离线回退、幂等性和 GC 边界 |

## 需要先记住的边界

- Lock 文件和安装 receipt 是可复现输入与审计记录，不是信任锚；Rust 的浮动 channel 仍是 rustup channel 名，不构成不可变版本。来自缓存或锁定 artifact 的重装仍应用当前 checksum/attestation 策略；普通 CLI 会在进入 pipeline 前直接复用已带完成标记的安装，不重新计算 checksum。只有实际进入 pipeline 的调用才可能在该快路径重验请求的 attestation。
- GitHub Artifact Attestation 默认是 `off`，且只适用于能提供相应 bundle 和身份约束的 GitHub artifact 流程。
- osdk 的 BLAKE3 CAS 去重 SDK 与模型的已验证文件；npm、pnpm、Yarn、Bun、Deno 的原生 package cache 仍各自隔离，当前没有跨 manager 的 package tarball CAS。
- `osdk cache clean` 只清理 osdk 的下载缓存，不会删除原生 package cache、安装目录、模型或 CAS。
- `osdk container cache status` 查询运行时自有的聚合接口。原生镜像拉取仍由所选 Docker
  或 containerd 控制面所有；限定范围的清理只支持 Docker 与 BuildKit。这些路径都不会读取
  或创建 osdk OCI 存储，也不会扫描实现私有的原生存储。
- SDK 来源选择与项目依赖 registry 选择是两套机制。registry preflight 只决定单次启动前的环境；manager 最多运行一次，候选全部不健康时 fail closed，根本不启动。
- 安装锁按具体对象缩小竞争范围；CAS 对象发布、manifest 发布与 GC 没有一个覆盖全局的串行化临界区。GC 的正确性边界应按[存储与缓存](./storage-cache)中的说明理解。

## 代码导航

核心 crate 的模块图从 [`osdk-core/src/lib.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/lib.rs) 开始。CLI 参数与命令分发分别位于 [`cli.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/cli.rs) 和 [`commands.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs)；跨进程 shim 的入口位于 [`osdk-shim`](https://github.com/lejunyang/one-sdk/tree/main/crates/osdk-shim)。实现断言以源码和测试为准，研究报告只保留设计背景与历史决策。
