# 可靠性、并发与提交语义

osdk 通过有限并发、源探测与故障转移、可恢复下载、跨进程锁、内容寻址存储和完成标记组合出安装可靠性。这些机制不是一个覆盖整条命令的数据库事务；不同层的原子性和失败边界如下。

## 并发模型

`settings.jobs` 控制一条命令中工具安装和模型文件下载的最大并发数。默认取逻辑可用并行度、最多 8；无法检测时为 4。环境变量或配置中的 0 会被忽略，命令执行处仍使用 `max(1)`。实现见 [`config/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/config/mod.rs)、[`commands.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs) 和 [`model/pull.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/pull.rs)。

多工具安装先应用 Node-first barrier：若请求包含 npm、pnpm、Yarn 或动态
`npm:<package>` 且没有 Node，CLI 自动注入 Node；所有 Node 请求先串行完成并生成 shim，
其余请求才进入 `buffer_unordered(jobs)`。这避免 Aube npm 工具与其受管 runtime 竞态。
barrier 之后的完成顺序不固定，shim 按该完成顺序生成，只有返回的解析记录随后按
backend 名称排序。任何任务失败会使批次返回错误，但已完成的独立安装不会回滚，因此
它是“依赖 barrier + 有界并发 + 每项提交”，不是全批事务。

源测速会并发探测全部候选，不受 `jobs` 限制。单次探测默认超时 1500 ms，最多读取约 1 MB，按首字节时间和吞吐量评分；成功结果默认缓存 6 小时。`auto` 使用测速排名，`ordered` 使用配置优先级，pin 会被放在首位，但其余源仍作为 fallback。详见 [`source/select.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs)。

## 下载、重试与缓存

[`download.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/download.rs)将响应流写入目标旁的 `.partial`，成功 flush 后用 rename 发布最终文件。伴随的 `.partial.json` 记录 URL、ETag 和 Last-Modified。只有 partial 与当前 URL 匹配且存在 validator 时才发送 `Range` + `If-Range`；否则删除旧 partial 并从头开始。服务器忽略 range 时会截断重写，返回错误起点的 `206` 会失败，`416` 会清理 partial 后进行一次完整请求。

每个 URL 最多尝试 3 次。只重试限流、5xx、request timeout、连接中断/失败以及 reqwest request/body/decode 类错误；退避为第一次失败后 400 ms、第二次失败后 800 ms。其他 4xx、校验失败、解压失败和策略失败不重试。一个 URL 的重试耗尽后，安装流水线按候选顺序切换源。

共享 HTTP client 的连接超时是 15 秒、idle pool timeout 是 30 秒、重定向最多 10 次；没有设置统一的整请求 deadline。元数据请求在线失败时可回退到已有 stale cache，离线模式只读缓存并在 miss 时失败。GitHub direct/proxy URL 使用 canonical upstream URL 作为同一缓存身份。见 [`http/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/http/mod.rs)。

安全边界：下载 rename 只保证成功前不出现最终文件，不保证落盘持久性，因为没有 `sync_all`；partial metadata 本身也不是原子写。缓存命中会跳过重新下载，但后续 checksum/attestation 策略仍决定是否接受其内容。

## SDK 安装提交

归档安装在 [`pipeline::run_with_attestation`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) 中按 `tool@version` 获取跨进程排他文件锁。持锁后：

1. 带 `.osdk-complete` 的安装视为已完成；若本次要求 attestation，仍会对缓存制品重新验证并合并 evidence。
2. 没有 marker 的旧安装目录被视为残留并清理。
3. 下载、checksum/attestation、解压、CAS ingest、materialize 和 receipt 依次执行。
4. `.osdk-complete` 最后写入，读取方只把带 marker 的目录视为已安装。

失败不会写 marker，下一次运行可清理并重建。但 SDK 树是直接 materialize 到最终目录，不是先构建完整目录再整体 rename；中途失败可能留下部分文件，直到下次运行清理。因此 complete marker 是提交判据，不是整个目录的原子替换。后处理发生在核心流水线返回以后；后端若在后处理失败，核心 marker 可能已经存在。

裸二进制安装同样最后写 receipt 和 marker，也会在失败前保留不完整目录供下次清理；不过当前 [`install_single_binary`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) 没有像归档路径一样自行获取 `tool@version` 文件锁。调用方通常通过有界调度避免同一请求重复，但跨进程并发安装同一个裸二进制不具备归档路径的序列化保证。

## CAS 与物化

[`store/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/mod.rs)用 BLAKE3 内容哈希作为对象名。新对象优先 rename；跨文件系统时先复制到临时对象再 rename，因此正常读取者不会看到半个 CAS 对象。竞争者已先发布对象时，保留获胜对象并清理临时文件。物化默认 `auto`：同文件系统依次尝试 hardlink、reflink、copy；跨文件系统尝试 reflink 后 copy。symlink 只在显式选择时使用。详见 [`store/link.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/link.rs)。

边界：CAS 的并发发布依赖原子 rename 和“目标已存在”检查，没有围绕每个对象单独加锁；临时名的唯一性主要来自进程 ID。同进程内同时 ingest 同一新 hash 的极端竞争不应被表述为严格事务。GC 会扫描安装和模型 manifest，遇到损坏的 manifest 会拒绝继续，避免因静默忽略而删除仍在使用的对象；当前 GC 调用没有全局锁，因此它不能与并发安装/发布形成严格隔离。

## 模型快照

模型文件也以 `jobs` 并发下载，先验证 provider 给出的大小和 SHA-256；若 provider 没给 SHA-256，osdk 计算一个用于本地 manifest。路径必须是普通相对组件，拒绝空路径、绝对路径和 `.`/`..`。实现见 [`model/pull.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/pull.rs)。

[`ModelStore::publish`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs)按内容身份生成 snapshot key，并对该 snapshot 获取跨进程锁。它在隐藏临时目录完成 CAS 物化、manifest 和 complete marker 后，再 rename 为最终 snapshot；失败会清理临时目录。`current.json` 也通过临时文件 + rename 更新。不同 snapshot 使用不同锁，因此同一模型的两个 revision 并发发布时，两个快照都可成功，最后更新 `current.json` 的一个成为当前版本。

## 锁文件与信任存储

`osdk.lock` 按平台独立保存解析版本和制品身份，写入采用同目录 `tmp-<pid>` + rename；信任存储和 JSON current pointer 采用类似方式。它们提供临时写入再发布的边界，但没有 fsync/durability、跨平台替换原子性或跨进程 read-modify-write 锁保证，因此并发 writer 可能发生 last-writer-wins、临时名冲突或目标已存在错误。调用方不应把 rename 等同于多进程事务隔离。锁文件实现见 [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs)，项目配置信任见 [`trust.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/trust.rs)。

项目配置的信任身份由 canonical path 与规范化 TOML 的 BLAKE3 共同定义。只有 `tools` 和 `aliases` 的项目文件无需显式信任；出现 `settings`、`sources`、`registries` 等顶层键时需要信任。信任管理只加载用户级配置，避免项目配置影响“是否信任自身”的判断。移动文件或改变有效 TOML 内容会使原记录失效；`OSDK_TRUSTED_CONFIG_PATHS` 则按 canonical 路径前缀信任，范围更宽，应谨慎设置。

## 关键测试

- [`pipeline/download.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/download.rs)：Range/If-Range 恢复，以及中断下载不会发布最终制品。
- [`pipeline/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs)：两个并发归档安装只提交一个完整结果，失败不写 marker，离线缓存与 checksum gate。
- [`model/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs)与 [`model/pull.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/pull.rs)：不可变 snapshot、篡改检测、文件选择身份和离线重建。
- [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs)：跨平台条目合并、已保存版本字符串与选项恢复、Rust 浮动 channel 边界、模型摘要和 evidence 持久化。
- [`trust.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/trust.rs)：canonical path、内容变化、symlink 与仓库移动后的失效行为。
