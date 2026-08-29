# SDK Sources 与项目 Registry

osdk 有两套独立的网络选择机制，不能混为一谈：

| 机制 | 获取内容 | 配置 | 生效点 | 是否重跑消费者 |
| --- | --- | --- | --- | --- |
| SDK/tool source | Node、Go、Python、manager 二进制、`npm:<package>` 等 artifact 和版本 metadata | `[sources]`、`[sources.<tool>]`、`--source` | backend 解析及安装前 | 下载 URL 可 failover；不会重跑已启动的外部 manager |
| 项目依赖 registry | npm/pnpm/Yarn/Bun/Deno 执行项目命令时解析的 npm-compatible 包 | `[registries.npm]` 与 manager 原生配置 | `osdk exec` / shim 启动 manager 前 | eligible 调用在选中候选后最多尝试启动一次；全部候选不健康时不启动 |

`osdk install pnpm@11` 的 source 决定从哪里取得 pnpm 本身；之后 `pnpm install` 的 registry 决定项目依赖从哪里解析。`--source` 不会改写项目 registry。

## SDK source 排名

[`effective_sources`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs) 从 backend 默认 source 开始，移除 disabled 项，加入用户 custom source（同 id 覆盖内置项），过滤 `enabled=false`，最后按较小 `priority` 排序。

[`ranked_source_list`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs) 的算法是：

1. 显式 pin 或一次性 `--source` 命中时移到首位，其余 source 仍作为 fallback；当前一次性覆盖先按用户输入工具名写入，因此调用必须使用规范 backend ID，别名不会收到该覆盖；
2. offline 时不探测；有候选集合指纹匹配的缓存时复用其排序，否则使用 priority 顺序；
3. `ordered` / `pinned` 策略也直接使用该顺序；
4. `auto` 优先读取按工具缓存的 probe 结果；全部缓存结果都在 TTL 内才算有效。cache schema 2 同时校验候选集合指纹，因此 URL、顺序、priority、enabled、凭据转发或 header 变化后不会沿用旧结果；header 值只保存 hash；
5. 缓存过期时并发探测所有 source，每个探测受 `probe_timeout_ms` 限制，最多读取约 1 MiB；
6. 成功结果按 `throughput - ttfb_ms` 的组合分数降序排列；失败项追加到尾部，仍保留为真实下载的最后 fallback。

版本 metadata 查询和 artifact URL 构造由各 backend 完成，所以不同 source 必须真正兼容对应 backend。共享 pipeline 随后按 URL 顺序下载；每个 URL 内的瞬时失败最多重试三次，失败后才切下一个 URL。probe 成功不是 artifact 完整性证明，checksum/attestation 在下载后独立执行。实现见 [`source/select.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs) 与 [`pipeline/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs)。

显式 `Source.headers` 用于 osdk 自己发起的 metadata 请求与 source probe，并独立于
`forward_credentials`。只有初始 URL 与配置的 index/download URL 同 origin 时才附加；
同源 redirect 继续携带，第一次跨源 redirect 后永久移除。header 值只以 hash 参与
metadata/probe cache identity，不明文写入 cache。Aube 2.1 embedded API 无法安全接收
任意 source header，因此 `npm:<package>` 的 Aube package fetch 不转发
`Source.headers`。项目操作可以使用原生可信配置；全局 npm 工具在隔离 prefix 下会拒绝
认证或私有原生配置透传。
Go command 工具有更严格的边界：存在 custom source 时只对这些 custom candidate（以及
显式 pin 的 candidate）排序，避免把私有 module path 发往公开默认 proxy。自定义 header
会被拒绝，因为 `go install` 无法执行 osdk 的逐请求转发策略。

## 项目 registry 启动前预检

registry planner 在 [`package_registry.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/package_registry.rs)，调用点是 [`apply_package_registry_plan`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs)。它只处理明确 allow-list 中可能拉取 npm 包的命令，并先识别 manager family；Yarn major 无法可靠确定时保守透传。

对需要预检的命令，每次调用都会执行新的、并发的匿名探测，不复用 SDK source probe cache。候选 endpoint 是 npm-compatible Registry 的标准 `<base>/-/ping`，必须满足：成功 HTTP 状态、非空 JSON object、响应不超过 64 KiB。每个请求有界超时；redirect chain 最多包含三个 URL（即最多跟随两次 redirect），且必须始终为原始 HTTPS origin，禁止降级、跨 origin、凭据 URL 与循环。探测不会携带 registry token、cookie 或从 manager 配置提取的 Authorization；普通系统 HTTP(S) proxy 环境仍由 HTTP client 使用。

候选选择规则：

- 显式 `[registries.npm].urls` 是完整候选集合，项目层整段覆盖用户层，并保持配置顺序，选择第一个健康项；
- 未显式配置时，已识别的匿名公共原生 registry 排在内置 npm mirror/npmjs 回退之前；
- 只有纯内置集合按本次 latency 选择最快健康项；
- URL 会规范化并去重。

选中后只向该子进程注入 manager 原生变量：npm `npm_config_registry`、pnpm `pnpm_config_registry`、Yarn Classic `YARN_REGISTRY`、Yarn Berry `YARN_NPM_REGISTRY_SERVER`、Bun `BUN_CONFIG_REGISTRY`、Deno `NPM_CONFIG_REGISTRY`。

## 单次启动与 fail-closed 行为

对实际进入 registry preflight 的 eligible invocation，选中健康候选后 osdk 最多发起一次 manager 启动，且启动后绝不重试；所有候选不健康时不发起启动。进程创建本身仍可能失败。保守透传的调用不会进入这一 fail-closed 预检路径。

控制流上，[`exec_cmd`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs) 先调用 `apply_package_registry_plan`，只有它返回成功后才唯一调用一次 `Command::status()`；直接 shim 路径也在 [`osdk-shim::real_main`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-shim/src/main.rs) 的唯一 `exec` 前执行同一 planner。`RegistryPlan::Unavailable` 在启动前转成错误；manager 启动后的非零退出码直接返回，不尝试另一个 registry，也不重放 lifecycle scripts。Unix 的 `osdk exec` 集成测试覆盖：

- [`exec_registry_fallback_injects_only_the_manager_variable_and_runs_once`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs)：首选不可用、后备健康时，每类 manager 的调用 marker 只有一行；
- [`exec_registry_never_retries_a_failed_manager_command`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs)：manager 启动后退出 42，marker 仍只有一次；
- [`exec_registry_all_unavailable_starts_no_manager_process`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs)：全部 probe 失败时 marker 不存在，并报告 command 未启动；
- planner 单测 [`all_failed_candidates_are_unavailable`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/package_registry.rs) 验证返回 `Unavailable`。shim 的单次启动性质来自上述控制流检查；这些集成测试没有覆盖直接 shim 或 Windows 路径。

## 保守透传与安全边界

以下情况不探测、不注入、不改写参数或配置，原样启动 manager：显式 registry CLI 参数；已设置对应 registry 环境变量；严格 offline 参数；命令不在网络 allow-list；显式 cwd/config 参数使配置上下文无法安全复现；原生配置不可读；存在 private/unknown registry、scoped registry、认证、TLS 或 manager-native proxy 设置；Yarn major 未知。原则是无法证明“匿名、公共、无 scope/auth”时不优化。

“registry 健康”只证明匿名 metadata endpoint 可用，不保证所有依赖都经过它。lockfile 或 metadata 中的绝对 tarball/Git URL、本地文件、workspace、git dependency、Deno JSR 或普通 URL import 都可能绕过默认 registry。osdk 不解析或重写 lockfile，也不代理凭据，因此固定到失效绝对 URL 的安装仍会失败；即使还有其他健康候选，也不会在 manager 已启动后重跑。完整设计边界见 [`docs/package-registry-design.md`](https://github.com/lejunyang/one-sdk/blob/main/docs/package-registry-design.md)。
