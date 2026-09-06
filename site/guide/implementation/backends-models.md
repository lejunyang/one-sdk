# 后端与模型 Provider 实现

本页面向希望理解或扩展 osdk 下载能力的维护者。SDK 与模型共享网络、来源选择和 CAS 等基础设施，但它们采用两套不同的领域接口：SDK 实现 `Backend`，模型仓库实现 `ModelProvider`。模型不是伪装成 SDK 的特殊 backend。

## SDK backend 合约

[`Backend`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs) 是所有 SDK 的统一边界。实现需要提供规范 ID、可选别名、默认来源、探测 URL、远端版本列表、安装逻辑、可执行文件目录与名称；还可以覆盖版本解析、卸载、安装后处理、激活环境变量和惯用版本文件。默认解析器把 `latest`、前缀、范围或精确版本解析为 `ToolVersion`，并始终保留 `-o/--opt` 选项。

`Ctx` 把目录布局、目标平台、合并后的配置、HTTP client、CAS 和进度显示传给实现。归档型 backend 通常只负责生成 `InstallPlan`，然后交给[共享安装流水线](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs)：

1. 根据 pin、顺序或测速缓存得到 best-first 来源列表；
2. 下载到共享缓存，失败时按来源顺序切换；
3. 验证 SHA-256、SHA-512、SRI 或已认证 attestation；
4. 安全解压到临时目录；
5. 内容写入 BLAKE3 CAS，以 hardlink、reflink 或 copy 物化；
6. 写 artifact receipt 和 `.osdk-complete`，使安装幂等且支持离线重装。

[`Registry`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/registry.rs) 注册内置 backend 和别名，并动态识别 `github:owner/repo`、`npm:<package>`、严格的 `http:https://...{version}...`、`cargo:<crate-or-https-url>` 与 `go:<module-or-command-path>` ID。它还从用户配置目录和数据目录的 `plugins/*.toml` 加载声明式 backend；重复 ID 或别名会直接报错，外部定义不能覆盖内置实现。

这些动态命名空间对 osdk 自有安装共用选项身份合约。解析或安装前，osdk 把受支持的
公开选项投影成
规范 map，拒绝未知公开 key，排除内部 `__osdk_*` lock 重放 metadata，并对 backend ID 与
规范选项计算与顺序无关、带 domain separation 的 BLAKE3 `b3-v2:` 身份。该身份覆盖 `tool`、
精确 `version`、`platform`、`scope`、规范 `material_options`、`dependencies` 与 `materials`。
`.osdk-install.json` schema 1 在嵌套 `identity` 中保存这些字段及 `install_id`，并把指纹用于
物理安装根，因此相同 backend/version 的多个身份可以共存。复用、activation、shim 执行、
`where`、uninstall 与 `reshim` 都要求配置精确匹配，绝不会回退到其他 fingerprint。旧
`.osdk-tool.json` 无论 schema 1 还是 2，都只用于遗留识别，不能授权复用或执行。项目管理的
npm 包不属于该 osdk 自有安装身份。

## 内置 backend 矩阵

| Backend | 解析与获取方式 | 完整性与安装语义 | 重要特性或限制 |
| --- | --- | --- | --- |
| `node` (`nodejs`) | Node index；官方、npmmirror、TUNA、USTC 归档 | `SHASUMS256.txt`；共享归档流水线 | 可选 `arch` 与 `corepack`；Corepack 是安装后动作 |
| `npm` | `npm` registry packument/tarball | npm SRI，强制校验；生成 `npm`/`npx` launcher | 独立于 Node 版本安装，但运行时仍需要活动 Node |
| `pnpm` | 完整 `pnpm` JavaScript distribution | npm SRI；osdk 生成 Node launcher | 自动加入受管 Node；按 major 设置 pnpm store 变量 |
| `yarn` | 1.x 用 `yarn`，2+ 用 `@yarnpkg/cli-dist` | npm SRI；生成 Node launcher | 原生管理 Classic 与 Berry，不委托 Corepack |
| `go` (`golang`) | go.dev JSON index；镜像可复用官方 index | index 中的 SHA-256；归档流水线 | 激活时设置 `GOROOT` |
| `python` (`py`, `cpython`) | 内置 PBS release index、Astral、GitHub proxy | 每个 release 的 `SHA256SUMS` | 支持 CPython、PyPy、GraalPy、Pyodide 与 variant；历史版本可用 `tag` 固定 |
| `java` (`jdk`, `openjdk`) | Foojay Disco API，Temurin 为默认 distribution | vendor checksum；JDK/JRE 归档 | `distribution`、`package-type=jdk\|jre`；激活时设置 `JAVA_HOME` |
| `maven` (`mvn`) | 内置单版本 release | 固定 SHA-512 | 当前 catalog 只包含一个版本 |
| `gradle` | 内置单版本 release | 固定 SHA-256 | 当前 catalog 只包含一个版本 |
| `kotlin` (`kotlinc`) | 内置单版本 GitHub release，可经代理 | 固定 SHA-256 | 当前 catalog 只包含一个版本 |
| `rust` (`rustup`) | rustup channel/version；官方、rsproxy、TUNA | rustup-init SHA-256；随后委托隔离 rustup | toolchain 不走归档 CAS；支持 `profile`、`components`、`targets`，设置隔离的 `RUSTUP_HOME`/`CARGO_HOME` |
| `deno` | `deno` packument + `@deno/<platform>` | npm SRI | 平台包；设置 `DENO_DIR` |
| `bun` | `bun` packument + `@oven/bun-<platform>` | npm SRI | 平台包；设置 `BUN_INSTALL_CACHE_DIR` |
| `npm:<package>` | npm packument；隔离安装使用受管 npm 子进程，项目/全局 `use` 可规划 npm 或 pnpm | 原生 lock 携带传递 integrity；默认禁脚本；`.osdk-install.json` schema 1 在指纹化 osdk 自有隔离/全局根中绑定 installer/build 身份 | 动态发现 `.bin`；自动加入受管 Node；lock schema 4 记录 scope、installer、可选原生 lock 身份与公开选项 |
| `cargo:<crate-or-https-url>` | crates.io 兼容 metadata 与配套 sparse index，或规范 HTTPS Git URL | 依赖精确 osdk 受管 Rust；隔离执行 `cargo-binstall`/`cargo install`；写原生 receipt、inventory 与 metadata seal | Registry 精确/latest/前缀，或 Git latest/tag/branch/完整 revision；schema 4 记录 runtime、replay 分类与 Registry source |
| `go:<module-or-command-path>` | Go proxy 的 `@latest`、版本列表与精确 `.info` metadata，并发现最长 module root | 依赖精确 osdk 受管 Go；隔离执行一次 `go install`；写原生 receipt、inventory 与 metadata seal | 精确/latest/前缀/伪版本；schema 4 记录 runtime、`version-only`、所选 proxy 与 module root |
| `github:owner/repo` | GitHub API，限流时回退 Atom/公开 release 页面；也支持静态 catalog | checksum、可选 minisign、GitHub artifact attestation；`.osdk-install.json` schema 1 在指纹化根中绑定 asset/layout/material 身份 | 自动选择 host asset；支持归档或裸二进制；复杂命名可用 regex/template/bin/rename/strip 规则 |

上述实现位于 [`backend/`](https://github.com/lejunyang/one-sdk/tree/main/crates/osdk-core/src/backend/)。npm 系列共用 [`npm.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/npm.rs) 的 packument、版本与 SRI 解析。通用来源排序位于 [`source/select.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs)。
动态 npm backend 的项目/全局/隔离安装、缓存、metadata-only lock、旧 lock schema 2 sidecar
兼容与 shim 边界见 [npm 开发工具实现](./npm-tools)。
严格 selector、精确 Rust 绑定、受控 provider fallback 与原生发布见
[Cargo 开发工具实现](./cargo-tools)。
module-root 发现、proxy 路由、构建环境策略、runtime 绑定与重放边界见
[Go 开发工具实现](./go-tools)。

## 声明式与 GitHub backend

[`DeclarativeBackend`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/declarative.rs) 是受限的 schema 1 TOML 扩展点。它支持静态或 URL 版本列表、平台模板变量、`tar.gz`/`tar.xz`/`tar.zst`/`zip`、固定或远端 checksum、`strip_root`、bin 路径和惯用版本文件。平台模板同时提供 osdk 短 token `{arch}` 与 LLVM target triple 的 CPU 部分 `{arch_llvm}`，因为编译器与工具链归档通常以 `x86_64`/`aarch64` 而非 `x64`/`arm64` 发布；后者复用 [`Arch::llvm_token`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/platform.rs)，而不是引入第二套命名表。定义文件最大 1 MiB，远端版本最多 10,000 个，并严格验证 URL、文件名、相对路径和 checksum。它刻意不执行 hook 或任意命令；所有安装必须经过共享验证与 CAS 流水线。
项目 lock 提供通用 artifact receipt 时，backend 会优先使用其中记录的 URL、文件名、
checksum 与子目录，再考虑当前模板。因此声明式工具与内置归档 backend 具有相同的
无 metadata 离线重装契约。
可选的 `[env]` 表允许定义描述其工具链所需的环境，这正是编译器能被用起来的前提：
构建系统通过 `CC`、`SYSROOT` 等变量而不是 `PATH` 定位交叉编译器。取值只从
`{install_path}`、`{version}`、`{id}` 渲染，若渲染后仍残留占位符，`exec_env` 会
失败关闭。变量名按常规环境变量标识符校验；`PATH` 与动态加载器变量
（`LD_PRELOAD`、`LD_LIBRARY_PATH`、`DYLD_INSERT_LIBRARIES`、`DYLD_LIBRARY_PATH`）
不区分大小写地保留，绝对路径、`..` 和控制字符在解析阶段即被拒绝，因此数据式定义
无法把子进程指向安装根之外。

[`GithubBackend`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/github.rs) 是运行时创建的命名空间 backend。它最多分页读取 1,000 个 release，忽略 draft，并按预发布策略过滤；随后按 OS、架构和 libc 为 asset 评分。显式规则可解决非标准 asset 名称。在线且启用签名校验时，可用的可信 minisign checksum manifest 会覆盖预载的静态摘要；否则使用静态摘要，再回退到普通 sidecar/shared checksum。配置的 GitHub attestation 策略独立应用。GitHub API、网页、Raw、release asset 和 attestation URL 都通过同一组规范化来源候选，但 token 只发给官方 API host。
其受支持的 asset、平台、catalog 摘要、rename、bin 与 strip 选项会先作为公开身份输入
校验，再写入 schema 1 动态安装 manifest。`catalog-url` 可用于获取，但会被刻意排除；必填的
`catalog-sha256` 在不把 catalog 位置写入动态 inventory 时标识内容，且含 userinfo、查询参数或 fragment 的 HTTP(S)
catalog URL 会被拒绝。因此，单有完成
标记不能复用由不同选项或旧 inventory 生成的 GitHub 安装；锁定重放还会核对已持久化
artifact receipt 的文件名、checksum 与子目录。身份不匹配时必须先卸载再重新安装该版本。
inventory 会先于完成标记发布，因此中断的收尾过程不会被误认为可复用安装。

## 模型是独立且 provider-specific 的

模型引用必须带 provider：`hf:owner/repo@revision` 或 `ms:owner/repo@revision`。[`ProviderId` 与 `ModelRef`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs) 将 provider 作为身份的一部分；默认 revision 也不同：Hugging Face 为 `main`，ModelScope 为 `master`。[`ModelProvider`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/provider/mod.rs) 只统一输出“已解析 revision + 文件 manifest”，并不假设两个服务具有相同 API。

| 语义 | Hugging Face | ModelScope |
| --- | --- | --- |
| 实现 | [`huggingface.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/provider/huggingface.rs) | [`modelscope.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/provider/modelscope.rs) |
| 元数据 API | `/api/models/{repo}/revision/{revision}?blobs=true` | `/api/v1/models/{repo}/repo/files?Revision=...&Recursive=true` |
| 文件 URL | `/{repo}/resolve/{commit}/{path}` | `/api/v1/models/{repo}/repo?Revision=...&FilePath=...` |
| 不可变 revision | 服务返回的 commit SHA | 请求 revision 加排序后的 path/size/SHA-256 manifest 的 BLAKE3 摘要 |
| 文件摘要 | LFS 文件带 SHA-256；普通 blob 缺失时下载后计算 | API 必须为每个文件返回合法 SHA-256，否则拒绝 |
| token | `OSDK_HF_TOKEN` → `HF_TOKEN` → `HUGGING_FACE_HUB_TOKEN`；Bearer | `OSDK_MODELSCOPE_TOKEN` → `MODELSCOPE_API_TOKEN`；Bearer + `m_session_id` cookie |
| 默认 endpoint | `https://huggingface.co` | 优先 `https://modelscope.cn`，回退 `https://www.modelscope.ai` |

因此，**ModelScope 不是把 Hugging Face base URL 换掉的镜像**。二者的元数据结构、下载 URL、认证头、默认 revision 和不可变快照推导方式都不同。provider 实现还会拒绝解析另一 provider 的 `ModelRef`。自动测速和失败切换只在同一 provider 的 endpoint 集合内进行，不会把同名 Hugging Face 仓库隐式替换成 ModelScope 仓库。

## 模型解析、下载与物化

`osdk model pull <name> <reference>` 的实现入口在 [`commands.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs)，核心流程在 [`model/pull.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/pull.rs)：

1. 解析显式 `--endpoint` 或 provider 环境变量；否则使用 provider 自己的默认/自定义来源。
2. 在 auto 模式下，[`model/source.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/source.rs) 对真实仓库先取 manifest，再对最大的可探测文件执行最多 1 MiB 的 Range 请求；结果按 provider、repo、revision 和来源配置缓存。
3. provider 解析远端 manifest；`--include`/`--exclude` glob 选择文件，`--variant` 只作为快照标签参与身份计算。
4. 文件按 `settings.jobs` 并发、可续传下载到 provider/repository/revision 隔离的 cache；校验声明 size 和 SHA-256。
5. [`ModelStore`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs) 再次校验文件，写入共享 CAS，在隐藏临时目录完成 snapshot 后 rename 到 `<models>/<logical-name>/snapshots/<snapshot-key>`，再以临时文件加 rename 更新 `current.json`；这些 rename 没有跨平台替换原子性或 durability 保证。
6. 默认把 provider、repo、requested/resolved revision、endpoint、variant 以及每个文件的 size/SHA-256 写入 `osdk.lock` 的顶层 `[models]`；token 和短期下载 URL不落盘。

`model list/path/verify/remove` 操作当前逻辑名。`verify` 同时检查 CAS BLAKE3 hash 和 SHA-256；`remove` 删除该逻辑名的全部 snapshot，再以 SDK installs 与 models 为 root 做 CAS GC。离线 pull 仍需已有 provider metadata cache 和逐文件 download cache，之后可重新物化已删除的 snapshot。

## Provider 环境持久化

`osdk model env enable [provider] [--force]` 只把 `sources.<provider>.env` 和可选 `env_force` 写入**用户级**配置，项目配置不能覆盖这两个开关。激活逻辑在 [`model/env.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/env.rs)：

- Hugging Face：`HF_ENDPOINT`、`HF_HOME`、`HF_HUB_CACHE`、`HF_XET_CACHE`、`HF_ASSETS_CACHE`；osdk 离线模式还设置官方支持的 `HF_HUB_OFFLINE=1`。
- ModelScope：`MODELSCOPE_ENDPOINT`、`MODELSCOPE_CACHE`；没有虚构 `MODELSCOPE_OFFLINE`。
- 默认保留用户已设置的变量；`--force` 才覆盖。shell activation 会记录原值，disable/deactivate 时恢复。
- 自定义 endpoint 默认 `forward_credentials=false`。当 osdk 管理该 endpoint 时，会清空 provider token，并把 home 指向隔离的 anonymous 目录，避免本地登录 cookie/token 泄漏。只有官方 endpoint 或用户明确 `--forward-credentials` 才允许下载请求携带凭据。token 本身从不写入 osdk 配置。

## 边界与注意事项

- SDK lock 按平台保存；model lock 位于顶层，因为模型文件通常与平台无关。模型 `variant` 是用户标签，不会自动推导量化格式，也不会改变文件选择。
- provider 身份贯穿引用、metadata/ranking/download cache、snapshot key、manifest 与 lock，已验证为 provider-specific；但顶层 `models` map 和本地 `current.json` 以用户提供的逻辑名为键。用同一逻辑名拉取另一 provider 会切换该名字的 current snapshot，并覆盖 lock 中该名字的记录。
- Hugging Face 非 LFS blob 可以没有远端 SHA-256；osdk 会在下载后计算并锁定，但这不等同于服务端提供的独立摘要。ModelScope 则要求 API manifest 给出合法 SHA-256。
- metadata 在线请求失败时可以回退到 stale cache。自定义 endpoint 必须实现所选 provider 的真实 API；仅兼容文件 host 或替换域名并不足够。
- GitHub backend 的自动 asset 评分是启发式；命名含糊或一个 release 含多个相似产物时应使用显式 asset 规则或可信静态 catalog。
- Rust 是委托型 backend，toolchain 由隔离 rustup 管理，不享受普通 archive backend 的逐文件 CAS 去重。Maven、Gradle、Kotlin 当前使用内置单版本 catalog，并非完整远端版本索引。
