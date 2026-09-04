# 存储、CAS、缓存与清理实现

osdk 把持久安装状态、内容寻址对象和可丢弃缓存分开。目录解析见 [`dirs.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/dirs.rs)，CAS 见 [`store/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/mod.rs)，链接策略见 [`store/link.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/link.rs)。

## 目录边界

默认布局如下。data 与 cache 根目录可覆盖，store 和 installs 另有专用覆盖变量；其余路径是从这些根派生的子目录：

- `<installs>/<tool>/<version>`：已物化的固定 backend SDK；`<installs>` 默认为 `<data>/installs`，也可由 `OSDK_INSTALL_DIR` 覆盖。
- `<installs>/<dynamic-tool>/<version>/b3-v2-<digest>`：osdk 自有隔离动态工具的指纹化安装根。
- `<installs>/npm-global/<package>/<version>/b3-v2-<digest>`：`use --global npm:<package>` 的指纹化安装根。包含 `.osdk-tool.json` 的旧版纯版本根只用于遗留识别，绝不会被复用或执行。
- `<data>/models/<name>/snapshots/<snapshot>`：已物化的模型快照。
- `<data>/store/<aa>/<bb>/<blake3>`：SDK 与模型文件共享的 CAS。
- `<data>/shims`：命令 shim。
- `<cache>/downloads`：下载的工具归档及其 checksum/source/attestation sidecar。
- `<cache>/tmp`：安装解压暂存区。
- `<cache>/remote`、`<cache>/sources`：远程 metadata 与源测速缓存。
- `<cache>/pkg`：下游包管理器与模型客户端的原生缓存。
- `<cache>/npm/v1/cache` 与 `<data>/store/npm`：npm 驱动的隔离、项目与全局 npm 工具共享的 cache/store；每个真实项目或受控安装根仍保留自己的原生 lock。

`Dirs::ensure` 在 CLI 初始化时建立核心目录。默认 store 与 installs 同在 data volume，便于 hardlink；`OSDK_STORE_DIR` 可把 store 移到其他卷，但这可能让物化回退到 reflink 或 copy。
每个动态根包含 `.osdk-install.json` schema 1；其嵌套 `identity` 包含 `tool`、`version`、
`platform`、`scope`、`material_options`、`dependencies`、`materials` 与规范 `b3-v2:`
`install_id`。复用、activation、shim 分发、`where`、uninstall 与 `reshim` 都由该身份驱动，
多个兄弟身份可以共存。`.osdk/npm-bin` 下的项目管理 npm 状态仍是独立的项目所有布局。
对 Cargo 开发工具，指纹还绑定精确受管 Rust 版本/平台、有界构建关键身份与 Registry/Git material。安装使用
sibling stage，其中包含私有 `home`、`cargo-home`、`target` 与 `tmp` 目录；这些 workspace
会在发布前删除，最终只保留已校验 `bin` 输出与原生 metadata。
对 Go 开发工具，指纹会绑定精确受管 Go runtime、所选 proxy/module root、tags 与白名单
构建环境。私有 home/GOPATH/temp 留在 sibling stage，而 module/build cache 分别共享在
`<cache>/pkg/go-mod` 与 `<cache>/pkg/go-build`。

## SDK 安装管线与锁

归档型 backend 进入 [`pipeline::run_with_attestation`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs#L129)，顺序是：

1. 固定 backend 获取 `installs/<tool>/.locks/<version>.lock` 的阻塞式独占进程锁；动态 backend 获取身份限定 lock，并持有到 backend-specific 收尾结束。
2. 在锁内检查 `.osdk-complete`；已完成安装直接复用，必要时仍重新验证 attestation 并合并 receipt evidence。
3. 删除同版本残留的不完整安装目录。
4. 复用或下载 `<cache>/downloads/...` 归档，验证 checksum/attestation。
5. 解压到 `<cache>/tmp/...-<pid>`。
6. 将普通文件写入 CAS，再物化安装树并写 `.osdk-manifest.json`。
7. 写 artifact receipt，最后写 `.osdk-complete`。

锁覆盖下载、验证、解压、CAS 写入、物化和完成标记。固定 backend 串行化**相同 tool/version**；动态 backend 只串行化相同完整安装身份，因此兄弟身份可以独立执行。锁由 [`FileLock`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/lock.rs) 持有至函数返回并在 drop 时释放。委托给外部管理器的 backend 不一定经过这条共享归档管线，不能把该锁描述为所有 backend 的无条件保证。

## CAS 写入与物化

每个普通文件以 BLAKE3 内容 hash 定位到两级 fan-out 路径。SDK 的 `ingest_file` 优先把解压源 rename 到最终对象；跨文件系统时复制到 `.tmp` 后 rename。并发 ingest 依靠最终对象存在性与 rename 竞争，但没有每对象锁。一个细节是 SDK 路径的共享临时名是 `<hash>.tmp`，失败的最终 rename 被忽略；如果最终对象仍不存在，当前实现仍会继续物化并在稍后报缺失/IO 错误，而不是在 ingest 点给出专门的竞争错误。

模型使用 [`ModelStore::publish`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs#L174)：先按 snapshot identity 获取 `models/<name>/.locks/<snapshot>.lock`，验证文件大小与可选 SHA-256，再用保留下载源的 `ingest_preserve` 写 CAS，物化到 PID 临时 snapshot，写 CAS manifest、模型 manifest 和完成标记，rename 到最终 snapshot，最后以临时文件加 rename 更新 `current.json`。模型锁只串行化相同模型 snapshot，不是模型级或全局 store 锁；同一模型的不同 snapshot 可并发发布并在 `current.json` 上 last-writer-wins，而 model removal 没有取得对应的模型级锁，也可能与发布竞争。这些 rename 没有 fsync/durability 或跨平台替换原子性保证。

[`materialize`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/link.rs#L132) 的 `auto` 顺序为：同文件系统先 hardlink，再 reflink，最后 copy；跨文件系统先尝试 reflink，再 copy。显式 hardlink/reflink 失败也降级为 copy。Symlink 只有显式选择才使用。manifest 记录每个普通文件的 CAS hash、模式以及符号链接目标，是 GC 的可达性来源。

## 不存在跨 manager 的包 CAS

CAS 去重的是 osdk 已验证并解压的 SDK 文件和模型文件。它**不解析、摄取或跨 npm/pnpm/Yarn/Bun/Deno/pip/Go/Cargo/Maven/Gradle 去重项目依赖包**。npm 驱动的 `npm:<package>` 操作共享 `<cache>/npm/v1/cache` 与 `<data>/store/npm`，pnpm 则使用自己的下游 cache/store；这些包内容都不进入 BLAKE3 SDK CAS。
Cargo 开发工具的 source 与构建数据同样只存在于 stage，不会提升为共享 Cargo cache 或
CAS；最终指纹化根只保留已发布 binary 与 osdk metadata。
Go module/build 数据使用上述 Go 自有 cache，同样不会进入 BLAKE3 SDK CAS；发布后的 Go
工具根只保留已校验 binary 与 osdk metadata。

[`cache_env`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/cache/mod.rs#L23) 与 backend 的 `exec_env` 只是把各 manager 的原生缓存重定向到 `<cache>/pkg` 下的独立子目录，例如 npm、pnpm store、Yarn、Bun 和 Deno。变量只有在用户未设置时才注入；若值来自上一轮 osdk hook，则允许刷新。目录共用一个父根不代表内容协议统一，因此不存在跨 manager 的 package CAS 或跨 manager blob 去重。

## `cache clean` 只清下载归档

[`commands::cache`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs#L1126) 的 `clean` 在确认后只删除整个 `<cache>/downloads`，随后尽力重新创建该目录。它不会清理：

- `<data>/store` CAS；
- `<data>/installs` 或 `<data>/models`；
- `<cache>/pkg` 下游 manager/client 缓存；
- `<cache>/remote` metadata、`<cache>/sources` probe 结果或 `<cache>/tmp`。

因此“cache clean”不是“清空所有缓存”。非交互执行必须通过 `--yes`、`OSDK_YES=true` 或 `settings.yes = true` 提供确认；拒绝确认时不删除。

## GC、卸载与并发 caveat

[`Cas::gc_roots`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/mod.rs#L207) 是一次 mark-and-sweep：先递归扫描 installs 与 models 下所有 `.osdk-manifest.json`，把 hash 加入 live set，再遍历 store 删除不在集合中的普通文件。manifest 无法解析时整个 GC fail closed，不删除任何对象；目录遍历错误则被跳过。名称恰好以 `.tmp` 结尾的文件会在 liveness 检查前被尽力删除；`.tmp-<pid>` 等其他临时命名没有特殊保护，会按普通未引用文件处理。

`osdk prune`、SDK `uninstall` 后和 `model remove` 后都会直接调用该函数。`uninstall` 先删除安装目录，之后才 GC；model remove 同理。当前实现**没有 claim/lease 机制，也没有获取全局 GC 锁**。尽管 [`lock.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/lock.rs) 的模块注释提到 guard store GC，实际 GC 调用点没有使用 `FileLock`。因此不能宣称 claim GC 是全局串行的，也不能宣称 GC 与正在进行的不同版本安装/模型发布互斥。

这会留下实际竞争窗口：GC 的 live-set 扫描与删除之间，另一个进程可能新建引用；安装也可能先产生 CAS 对象、后写 manifest，此时并发 GC 可把尚未被 manifest 声明的对象视为不可达。per-version/per-snapshot 锁无法关闭这个全局窗口。生产文档和运维流程应把并发 `prune`/卸载与安装视为未协调操作。

另外，GC 只删除对象文件，不清空空的 fan-out 目录。单个删除失败不会增加 removed-object 数量并会被静默跳过，但其大小可能已经计入返回的 byte 总数。
