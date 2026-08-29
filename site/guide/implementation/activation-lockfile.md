# 激活、Shim 与 Lockfile 实现

本页描述当前源码的实际行为。入口主要位于 [CLI 命令编排](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs)、[激活渲染器](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs)、[shim 生成器](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/shim/mod.rs)、[shim 进程](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-shim/src/main.rs) 和 [lockfile 模块](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs)。

## 激活不是安装，也不修改当前父进程

`osdk activate <shell>` 只把一段 shell 代码打印到标准输出；调用方必须 `eval` 或 `source` 它。Bash 使用 `PROMPT_COMMAND`，Zsh 注册 `precmd_functions`，Fish 监听 `PWD` 与 `fish_prompt`，PowerShell 使用带重入保护的 `PostCommandLookupAction`。所有实现都会立即调用一次 hook，因此无需等待第一次目录切换。对应入口是 [`commands::activate`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs#L1847) 与 [`activation_script`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L48)。

每次 hook 调用都会执行 `osdk hook-env`。[`compute_env_delta`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L217) 遍历 backend，按当前目录重新解析版本，只选已安装版本，并收集真实 bin 目录与 backend 环境变量。随后 CLI 叠加共享包管理器缓存变量和已启用的模型 provider 环境。输出脚本先从保存的原始 `PATH` 重建 PATH，再恢复已不再受管的变量，最后设置本次变量，因此反复刷新不会持续堆叠路径。原始值通过 `OSDK_ORIGINAL_PATH*`、`OSDK_ORIG_<KEY>*` 和 `OSDK_MANAGED_ENV` 保存；[`deactivation_script`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L107) 据此恢复。

## 解析顺序与 shim 优先级

活跃版本的解析顺序见 [`resolve_active`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs#L154)：

1. 最近祖先中的项目配置。
2. 对 npm、pnpm、Yarn，仅当项目配置没有适用的 package-manager 选择时，再看 `package.json#packageManager`，然后看 `devEngines.packageManager`。
3. 最近祖先中的 `.tool-versions`。
4. backend 声明的惯用版本文件。
5. Node 的结构化 `package.json` 版本范围。
6. 用户全局配置。

项目选择和全局选择分别以配置为入口。`osdk use <tool>` 写最近的项目配置，找不到时在当前目录创建 `osdk.toml`；`osdk use --global <tool>` 写用户配置目录的 `config.toml`。对项目感知的 `npm:<package>`，本地 `use` 还会在真实 Node 项目根更新 `osdk.lock`；`use --global npm:<package>` 则更新 `$OSDK_CONFIG_DIR/osdk.lock`。[`config_edit`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/config_edit.rs) 通过唯一临时文件、sync 与 replace 发布单个配置；单个项目配置的 read-modify-write 外仍没有进程锁，而全局 npm 变更使用专用全局状态锁把用户 lock、shim 和配置发布串行化。

激活 PATH 是 shim-first，但只在至少存在一个已生成 shim 且存在活跃真实 bin 目录时加入 shim 目录。其后 package-manager 路径排在 Node 路径之前，再是其他运行时，参见 [`prioritize_managed_paths`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L300)。Unix shim 是指向 `osdk-shim` 的符号链接；Windows 同时生成 `.cmd` 与 Git Bash wrapper。安装完成后生成 shim，`reshim` 可重建；缺少 shim 二进制只警告，真实 bin 目录仍可通过 activation 使用。

shim 启动时重新加载配置并按当前工作目录选择已安装版本，不访问网络。它会从子进程 PATH 中移除 shim 目录以阻止递归，加入真实 backend bin；JavaScript 包管理器还会加入受管 Node。npm、pnpm、Yarn、Bun 和 Deno 的依赖获取命令在真正执行前运行 registry preflight。Node 自带的 npm/npx 可被路由，但 Node backend 不取得独立 npm backend 的所有权，详见 [`routed_bin_names`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/shim/mod.rs#L25) 与 [`osdk-shim::real_main`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-shim/src/main.rs#L27)。

隔离与全局 `npm:<package>` 安装会把受控安装根中发现的命令和相对路径写入 inventory；
真实项目安装则校验请求包声明的 bin，并生成筛选后的
`.osdk/npm-bin/generations/<identity>/bin`；受信任项目激活只暴露该校验 generation，绝不
加入完整的 `node_modules/.bin`。shim
启动时扫描受管 inventory 以恢复 backend ownership，并为其 PATH 追加受管 Node。若多个
backend 导出同名 bin，运行时仅在当前配置能唯一选出 owner 时路由，否则拒绝任选一个；
CLI 生成或 `reshim` 则始终对多个已安装 backend owner 移除歧义的受管 shim 并报错。
同一 backend 的多个版本由活跃版本选择处理，不构成 owner 冲突。详见
[npm 开发工具实现](./npm-tools)。

osdk 自有的动态 `npm:<package>`、`cargo:<crate-or-https-url>` 与 `github:owner/repo`
安装使用 `.osdk-install.json` schema 1。
其嵌套 `identity` 记录 `tool`、`version`、`platform`、`scope`、`material_options`、
`dependencies`、`materials` 与规范 `b3-v2:` `install_id`。该指纹进入物理安装根，因此相同
backend/version 的多个身份可以共存。activation 加入路径或 shim 执行命令前，osdk 会派生
配置的精确身份，只选择对应根；复用、`where`、uninstall 与 `reshim` 使用相同选择。身份
缺失、过旧或不匹配都会 fail closed；`.osdk-tool.json` 只用于识别遗留状态，其 schema 1
和 schema 2 都不能授权执行。这与上面的 bin owner 歧义检查是两个独立条件。项目管理的 npm
activation 继续使用单独校验的 `.osdk/npm-bin` generation。Cargo 原生候选还会额外校验
receipt、metadata seal、binary digest，以及精确 Rust 版本/平台和有界构建关键身份；详见
[Cargo 开发工具实现](./cargo-tools)。

## 信任边界

CLI 初始化和 shim 都在加载项目配置前检查信任。只有 `[tools]` 与 `[aliases]` 的项目文件无需显式信任；出现 settings、sources、registries 等可影响执行或网络的顶层键时，配置必须被信任。信任身份是规范化文件路径加规范化 TOML 内容的 BLAKE3，因此内容修改或仓库移动会使记录失效；`OSDK_TRUSTED_CONFIG_PATHS` 可按规范化路径授权文件或目录。实现见 [`trust.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/trust.rs)。

## `osdk.lock` 的读取语义

当前 writer 使用 schema 4。它为委托编译的工具增加可选的类型化 `native` 表，
记录受管 runtime id、精确 runtime 版本，以及 `version-only`、
`immutable-revision` 或 `floating-ref` 重放等级。Cargo Registry 条目还在 `native.source`
中记录规范、无凭据的 sparse HTTPS index；Cargo Git 条目不能携带该字段。已有 schema 1 到 3
对非 native 工具仍可读取，并在下次成功写入时升级。由于旧 schema 无法表达 runtime
绑定，其中的 `cargo:` 或 Go module `go:` 条目会失败，并明确要求重新生成 schema 4
lock。Native metadata 只恢复为内部 request option，不会重复写入公开 `options` 表。

项目 lockfile 是项目根或其祖先目录中的 `osdk.lock`。[`find`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs#L255) 从当前目录向祖先查找最近的现有文件。无参数且无 `-o` 的 `osdk install` 才尝试读取这条项目 lock 路径；显式工具或任何 `-o` 都绕过它，改从配置/参数收集请求。另外，`use --global npm:<package>` 维护用户配置目录中的 `osdk.lock`；该用户 lock 不参与上述祖先查找，也不是无参项目 `install` 的输入。

读取时完整解析 TOML，并接受已有 lock schema 1 到 3 以及当前 lock schema 4；其他版本会被拒绝。随后只读取当前平台键；若文件存在但没有该平台区段，返回“没有锁定请求”，调用方回退到普通配置解析。带通用 artifact 子表的非 npm 工具会被转换成保存版本字符串的请求，并注入 artifact URL、文件名、可选 checksum 与 subdir 等内部选项；npm 工具明确不能带通用 artifact receipt。lock schema 3 或 4 的 npm 工具恢复公开选项以及 package、installer、scope、可选精确 Node 版本和原生 lock 身份；主 lock 中没有 graph payload 或路径。lock schema 2 仍是兼容读取格式：其 npm 条目指向 `osdk.lock.d/npm/<sha256>.yaml`，sidecar 通过大小、symlink、UTF-8 与 SHA-256 校验后，完整 graph 才作为内部 option 注入。这个旧 lock schema 2 graph sidecar 与 `.osdk-install.json` schema 1 无关。含 npm 条目的 lock schema 1 不会被消费，必须重新生成。大多数 backend 的字符串是精确版本，但 Rust 浮动 channel 仍会由 rustup 在安装时解释。锁中的原始 `request` 只用于记录，不参与这次版本选择。对带通用 artifact receipt 的 backend，全新或强制重装会在锁中存在 checksum 时校验它；若没有 digest/evidence 且 `require_checksums=false`，仍可能不做加密完整性校验。普通 CLI 会在进入 pipeline 前直接复用已带完成标记的安装，不重新校验 checksum；动态 npm/GitHub 复用还要求精确匹配 `.osdk-install.json` 身份。只有实际进入 pipeline 的调用才可能在其完成快路径重验请求的 attestation。锁中 evidence 是审计数据，不是验证输入。顶层模型记录由 `model pull` 写入，但当前无参数 `osdk install` 只消费平台工具记录，不会据此恢复模型。npm metadata、安装身份 schema 1 与旧 lock schema 2 sidecar 的兼容边界见 [npm 开发工具实现](./npm-tools)。

平台键是 `os-arch`，Linux musl 额外带 `-musl`。`osdk lock` 对 Node 的 `arch` 选项使用目标架构键；普通 `upgrade` 使用当前 host 平台键。

## `osdk.lock` 的写入语义

写入目标由 [`project_lock_path`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs#L614) 决定：如果加载过项目配置，就固定写在该配置旁；否则复用向上找到的最近 lockfile；两者都没有时写当前目录的 `osdk.lock`。

[`merge_resolved`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs#L771) 先读取整个现有文件，保留其他平台区段与顶层模型记录，但**清空并整体替换目标平台的 tools 表**。因此 `osdk lock node@20` 不是只合并一个 Node 条目：它会移除目标平台原先未包含在本次 resolved 集合里的工具。项目感知 npm `use` 与全局 npm `use` 改走 upsert：前者插入或替换 Node 和 npm 工具，后者在使用原生安装器时还包含受管 npm/pnpm manager；两者都保留同平台其他工具。写出时统一使用 lock schema 4，其中公开选项会在后续读取时注入请求，并与动态 inventory 身份核对；已有的非 native schema 1 到 3 仍可读取，并在下次成功写入时升级。不会尝试迁移尚未发布的 schema-4 之前 `cargo:` 或 Go module `go:` 条目，它们必须重新生成。Native 重放 metadata 只通过内部 request option 恢复，不会重复写入公开 options 表。含 npm 条目的 schema 1 仍拒绝消费或写入。旧 schema 2 npm sidecar 在迁移前会回读校验；当前写入只原子替换主 lock，不生成新 sidecar，也不删除旧 sidecar。内部 `__osdk_*` options 不会写出；本地链接的 Rust toolchain 被拒绝，因为它不能形成可复现远程 artifact。模型 pull 则由 `merge_model` 只插入或替换同名 `[models]` 项，并保留平台表与其他模型，但同样不会迁移 schema 1 npm 条目。

保存过程先序列化完整文档，写同目录中包含 PID 和进程内序号的唯一临时文件，sync 文件后 replace `osdk.lock`，Unix 上还会 sync 父目录。项目 lock 的 read-modify-write 周围仍**没有进程锁**：两个并发 writer 可以都读到旧状态，最后一次成功 replace 可能覆盖另一方的合并结果；读取也不持有共享锁。全局 npm `use` 是例外，它在更新用户 lock、shim 和用户配置时持有 `global-npm-state.lock`。

## 需要记住的边界

- “global” 指 osdk 控制的用户级选择与安装作用域；全局 npm `use` 会写用户 `osdk.lock`，但不会改写当前项目 lock。
- activation 只选择已经安装的版本；找不到匹配版本时跳过该 backend，不会自动安装。
- lockfile 记录保存的解析结果与 backend 专属复现身份；项目 lock 不会串行化并发 writer，而全局 npm `use` 的用户 lock 写入位于专用状态锁内。Rust 浮动 channel 不是不可变版本。重装会应用当前可用或策略要求的验证；已完成安装不会重做 checksum 校验。
- trust store 自身也采用临时文件加 rename，但没有 read-modify-write 锁、durability 或跨平台替换原子性保证；并发 trust/untrust 同样可能丢失更新。
