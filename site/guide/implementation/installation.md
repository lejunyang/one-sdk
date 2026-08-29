# 安装流水线

`osdk` 把“解析版本”和“安装 artifact”分开。CLI 负责请求编排、并发与 shim；backend 负责生成安装计划；共享 pipeline 负责下载、验证、解包、CAS 入库和完成标记。

## CLI 编排

[`install_requests`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs) 合并 `-o key=value` 选项，并按 `[settings].jobs` 使用有界并发安装不同工具。每个请求进入 [`install_one_without_shims`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs)：
若请求中包含 npm、pnpm、Yarn 或 `npm:<package>` 且缺少 Node，CLI 会先注入 Node；
安装调度把所有 Node 请求串行完成后，才让其余请求进入有界并发，避免动态 npm 工具
与其 runtime 竞态。
同一编排还要求每个 `cargo:` 工具恰好对应一个精确受管 Rust 请求。Rust 会在其余 Cargo
工具进入并发阶段前完成解析与安装，得到的精确结果再绑定到每个 Cargo 安装身份。
Go command package 使用同样的依赖 barrier：一个显式或配置的受管 `go` 请求会先于所有
`go:` 请求解析和安装，并把精确结果绑定到每个 Go 工具身份。

1. 安装类调用可选执行 `--refresh-sources`；该开关不在 `lock`、`outdated`、`list-remote` 的纯解析路径执行；
2. 查找 backend、展开版本 alias、解析精确版本；
3. 若完整安装标记已存在，只执行幂等的 `ensure_post_install`；
4. 否则调用 backend 的 `install`；
5. 所有安装完成后生成 shim，并按 backend 名排序结果。

这意味着除上述 runtime 前置依赖外，不同工具可以并发；固定 backend 的同一 `tool@version` 写入与动态 backend 的同一完整安装身份写入，仍分别由 pipeline 或 backend 文件锁串行化。若批次中任一安装失败，`try_collect` 返回错误，未进入最终 shim 生成阶段。显式 `install`/`exec` 的隔离 npm 兼容路径不走下面的归档 CAS pipeline，而使用 embedded Aube、隔离的安装根以及 osdk 自有的共享 Aube cache/store；项目感知或全局 `use` 还可在规划阶段选择 Aube、npm 或 pnpm，见 [npm 开发工具实现](./npm-tools)。Cargo 开发工具也绕过 archive CAS pipeline；其原生 lifecycle 在 sibling stage 整个期间持有身份锁，只在符合条件时优先使用受控 `cargo-binstall`，仅对退出码 94 回退到 `cargo install`，最后原子发布已校验 binary，详见 [Cargo 开发工具实现](./cargo-tools)。Go 开发工具复用该原生事务，并以 staged `GOBIN` 调用一次精确受管 `go install`，详见 [Go 开发工具实现](./go-tools)。

## Backend 生成计划

统一契约是 [`Backend`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs)。归档型 backend 通常先调用 `ranked_source_list`，再生成 [`InstallPlan`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs)：工具、精确版本、候选 URL、文件名、归档类型、可选 checksum、是否剥离顶层目录，以及可选安全子目录。Node 的代表实现见 [`node.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/node.rs)。

从 lockfile 恢复的请求优先通过 [`locked_install_plan`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) 使用锁定 URL、文件名、checksum 和 subdirectory，不必重新查询版本 registry；内置归档 backend 和纯数据声明式 backend 共用这条路径。pnpm、Yarn、Deno、Bun 和独立 npm 由 npm registry 包及 SRI 驱动；通用 GitHub backend 可额外要求 Sigstore/Rekor attestation。Rust 是重要例外：它使用隔离的 rustup/Cargo home，委托 rustup 安装工具链，并由 osdk 自己维护完成标记和 shim。

## 共享 pipeline 的事务顺序

[`pipeline::run_with_attestation`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) 执行以下步骤：

1. 固定 backend 获取 `<tool>/<version>.lock`；动态 backend 则持有带完整身份的 lock，直到后处理、`.osdk-install.json` 发布与完成标记全部结束。
2. 若直接调用 pipeline 且安装目录已有 `.osdk-complete`，它会提前返回；该次调用带 attestation 时会重新验证并合并证据。普通 CLI 安装通常更早在 `install_one_without_shims` 短路，只执行 `ensure_post_install`。
3. 删除没有完成标记的陈旧安装目录。
4. 使用稳定的 artifact cache 路径；离线且缓存缺失时立即失败。
5. 按计划 URL 顺序下载。单个 URL 内先做最多三次瞬时错误重试，仍失败才切换下一个 source。
6. 校验计划提供或缓存持久化的 checksum；有 attestation 时验证证明及其认证 digest。`require_checksums=true` 且两者都没有时失败。
7. 解包到 cache 下的 scratch 目录，并验证可选 subdirectory 不绝对、不含 `..`、不含平台前缀且仍位于 scratch 内。
8. 将文件 ingest 到 BLAKE3 CAS，再按配置用 reflink、hardlink 或 copy 物化安装树。
9. 写 `.osdk-artifact.json` receipt，最后写 `.osdk-complete`。只有最后一步完成后版本才会被列为已安装。

## 下载恢复与缓存

[`pipeline/download.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/download.rs) 先写同级 `.partial` 文件，成功后原子 rename。只有 partial metadata 中保存了同一 URL 的 ETag 或 Last-Modified 时才发送 `Range` + `If-Range`；服务端忽略 range、对象变化或返回不匹配的 `Content-Range` 时会安全重启或失败，不会盲目拼接。敏感请求头只附加在初始请求，跨 host redirect 不继承。
这里描述的是通用 artifact download plan 携带的下载 header。`Source.headers` 另用于
osdk metadata/source probe，并按 origin 约束；Aube 驱动的 npm package fetch 当前不转发
任意 `Source.headers`。项目操作可以使用原生可信配置；全局 npm 工具在隔离 prefix 下会
拒绝认证或私有原生配置透传。

在实际进入 pipeline 的安装或重装中，artifact cache 命中仍会执行适用的 checksum/attestation 逻辑，而不是把“文件存在”当作验证成功。离线重装可以使用先前持久化的 checksum；但没有缓存 artifact 时不会联网降级。普通 `osdk install` 若发现完整安装，会在 pipeline 之前复用它，不重新校验 receipt、checksum 或已安装字节。

## 失败语义与 caveat

- 下载 failover 只针对 artifact 获取；一个候选下载成功后，后续 checksum、attestation、解包或物化失败不会换源重新执行整条安装。
- pipeline 会在下一次尝试开始时清除陈旧安装目录和 scratch，并在成功物化后删除 scratch；解压或物化失败可能暂时留下 scratch。下载 `.partial` 会保留以便安全续传。
- checksum 是可选策略，除非 backend 本身提供、attestation 提供认证 digest，或配置启用 `require_checksums`。各 backend 的真实保证不同。
- `ensure_post_install` 可能有额外副作用。全新 Node 安装若 Corepack 后处理失败会删除安装树；在已安装快路径上，`ensure_post_install` 仍可能失败而现有完成标记继续保留。
- Rust 等 delegate backend 不经过完整归档 pipeline；Cargo 与 Go 开发工具也使用自己的原生 stage/receipt/seal 事务。其幂等性和验证边界应以对应 backend 实现为准。
- 对支持通用 artifact receipt 的 backend，lockfile 中的 artifact URL 固定来源身份并支持 metadata-free 重装。重新安装会执行当前可用或策略要求的 checksum/attestation；若没有 digest/evidence 且 `require_checksums=false`，仍可能不做加密完整性校验。npm 工具不使用通用 artifact receipt；当前 schema 4 保留 scope、installer 和可选原生 lock 身份，依赖图 payload 仍归安装器自己管理。schema 2 graph sidecar 只作为兼容读取路径，详见 [npm 开发工具实现](./npm-tools)。Cargo 与 Go native metadata 同样只描述 runtime/source/replay 身份，而不是完整 graph，因此不支持全新离线安装；详见 [Cargo 开发工具实现](./cargo-tools)与 [Go 开发工具实现](./go-tools)。

核心测试位于 [`pipeline/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs)、[`pipeline/download.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/download.rs)、[`backend/contract.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/contract.rs) 和端到端 [`isolated_cli.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs)。
