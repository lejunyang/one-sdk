# osdk 剩余能力缺口与修复路线

日期：2026-08-16

项目：`github.com/lejunyang/one-sdk`

关联文档：

- [SDK manager 市场与可靠性审计](./sdk-manager-audit-2026-08-15.md)
- [首轮整改报告](./remediation-report-2026-08-15.md)

## 目的与边界

首轮整改已经完成并交付了并发安装、断点续传、离线模式、隔离 rustup、
锁文件、升级、一次性执行、别名、声明式后端、校验策略和 GitHub Artifact
Attestations 等能力。但“首轮选定整改项完成”不等于原审计列出的所有能力缺口
已经关闭。

本文只记录当前仍未完整实现的事项，并为每项给出：

1. 当前问题和用户影响；
2. 推荐的实现方案；
3. 可借鉴的开源 SDK 管理器做法；
4. 可验证的完成标准。

优先级定义：

- **P0**：行为与配置不一致、安全边界或可靠性问题；
- **P1**：高频 SDK 管理能力；
- **P2**：生态覆盖和高级能力；
- **P3**：长期安全增强或依赖上游的事项。

## 总览

| 优先级 | 事项 | 当前状态 |
| --- | --- | --- |
| P0 | `--yes` 没有实际消费方 | 参数已存在，但没有确认流程 |
| P0 | 项目配置缺少显式信任模型 | 当前配置能力较安全，但扩展后会形成风险 |
| P0 | 通用 GitHub backend 的 source 语义不完整 | source 可展示，下载链路未统一消费 |
| P1 | Node `package.json` 解析、架构覆盖和 Corepack | 未实现 |
| P1 | Python 多实现、多变体和可刷新 catalog | 仅稳定 CPython 默认变体 |
| P1 | Java 离线 catalog、JRE 和 JVM 工具生态 | 仅 Foojay 在线 JDK |
| P1 | Rust 独立 component/target/override 管理 | 安装参数可用，独立生命周期命令缺失 |
| P1 | `packageManager` 自动识别和独立 npm | 未实现 |
| P1 | GitHub backend 可配置资产选择 | 仅启发式自动匹配 |
| P2 | Deno/Bun prerelease、canary、nightly | 默认只暴露稳定版本 |
| P2 | 统一 backend contract 和故障矩阵测试 | 部分覆盖 |
| P2 | Windows 运行时行为测试 | 可交叉编译，运行时覆盖不足 |
| P3 | 完整 Rekor transparency-log 证明 | 受 `sigstore` 上游能力限制 |

## P0：全局行为与安全

### 1. `--yes` 没有实际消费方

#### 问题

CLI 和配置中已经存在 `--yes`、`OSDK_YES` 和 `settings.yes`，但当前没有任何
命令发起交互确认。这会产生两个问题：

- 用户会误以为 `--yes` 能改变卸载、清理或覆盖行为；
- 未来新增确认流程时，不同命令可能各自实现，造成 CI 卡住或语义不一致。

#### 修复方案

建立统一的确认接口，例如 `Prompt` trait：

- 交互终端中显示本地化问题；
- `--yes` 直接返回同意；
- 非 TTY 且未提供 `--yes` 时明确失败，不能无限等待；
- `--quiet` 不应隐式等价于同意；
- 第一批接入 `cache clean`、真实删除型 `prune` 和高风险覆盖场景；
- 如果短期内没有任何需要确认的动作，先删除公开的 `--yes`，避免提供空开关。

#### 开源实现参考

- SDKMAN 在安装、切换默认版本等流程中统一处理交互确认；
- mise 的 CI/非交互模式会改变提示行为，并提供明确的自动确认选项；
- uv 对需要确认的破坏性操作使用统一 CLI 约定，自动化环境不会静默等待输入。

#### 验收标准

- TTY、非 TTY、`--yes`、`OSDK_YES` 分别有 subprocess 测试；
- 中英文提示和错误一致；
- 任一命令不得直接读取 stdin 绕过统一接口。

### 2. 项目配置缺少显式信任模型

#### 问题

当前 `osdk.toml` 主要保存工具版本和普通设置，本身不会执行脚本，因此即时风险
有限。但 osdk 已支持用户目录声明式 backend，未来若项目配置增加环境变量模板、
hook、插件或可执行逻辑，进入陌生仓库就可能影响下载来源、PATH 或执行内容。

如果等到配置具备执行能力后再补信任机制，兼容性和迁移成本会更高。

#### 修复方案

- 将配置字段划分为“安全数据”和“需信任能力”；
- 安全版本 pin 可直接读取；
- hook、项目内插件、自定义可执行文件、危险 URL 模板等必须先信任；
- 信任记录使用配置文件规范化内容的哈希，而不是只记录路径；
- 文件变化后自动失效；
- 增加 `osdk trust [path]`、`osdk untrust [path]`、`osdk trust list`；
- 非 TTY 环境遇到未信任配置时失败，除非配置显式白名单；
- 提供 `OSDK_TRUSTED_CONFIG_PATHS` 或用户配置白名单。

#### 开源实现参考

- mise 在解析可能执行代码或修改环境的 `mise.toml` 前检查信任；
- `mise trust` / `mise untrust` 记录用户决定；
- mise 允许配置 `trusted_config_paths`，CI 也有明确的 trust 行为。

参考：<https://mise.jdx.dev/cli/trust.html>

#### 验收标准

- 仅版本 pin 的配置不要求信任；
- 危险字段在未信任时不会被部分执行；
- 文件修改后原信任失效；
- 软链接、路径穿越和仓库移动有测试。

### 3. 通用 GitHub backend 的 source 语义不完整

#### 问题

`github:owner/repo` 会显示 GitHub direct 和 gh-proxy 两个 source，但当前代码的：

- Releases API；
- release asset；
- checksum/signature sidecar；
- attestation API 和 `bundle_url`

没有全部通过同一 source 排序与 failover 模型。用户执行 `source pin` 或
`source add` 后，显示结果和真实网络行为可能不一致。

#### 修复方案

- 为 GitHub source 定义三类端点：`api_base`、`download_base`、`raw_base`；
- source 排序同时驱动 API、asset、raw/sidecar 和 attestation 获取；
- direct 请求可携带 `GITHUB_TOKEN`；
- 第三方代理默认不转发 token，避免凭据泄露；
- 每个原始 URL 都通过统一 `rewrite_github_url(source, url)` 生成候选 URL；
- 缓存 key 使用原始资源身份，避免 direct/proxy 生成两份逻辑缓存；
- API、Raw、Release、attestation 分别有本地 mock failover 测试。

#### 开源实现参考

- mise 的 URL replacement 可对 URL 使用正则替换，统一应用下载代理；
- aqua 把元数据、asset、checksum 和 provenance 都放进 registry 描述；
- gh-proxy 支持完整 GitHub URL 代理，包括 `api.github.com`、
  `raw.githubusercontent.com` 和 release assets。

参考：

- <https://mise.en.dev/url-replacements.html>
- <https://gh-proxy.com/>

#### 验收标准

- `source pin github:owner/repo ghproxy` 后不再尝试 direct；
- direct 失败时 API、raw 和 asset 都能回退代理；
- token 只发送给明确受信的 GitHub host；
- offline 和 stale cache 行为保持不变。

## P1：核心 SDK 管理能力

### 4. Node 项目解析、架构覆盖、Corepack 和全局包迁移

#### 问题

Node backend 已支持 `.nvmrc`、`.node-version`、别名和 `osdk exec`，但仍缺少：

- `package.json#engines.node` / `devEngines.runtime` 解析；
- 手动选择 x64、arm64 等目标架构；
- 安装后自动执行 `corepack enable`；
- 从旧 Node 版本迁移全局 npm 包；
- 对 shell 目录切换和 Corepack 的真实跨平台运行测试。

#### 修复方案

1. **项目解析**
   - 在找不到更高优先级版本文件时读取 `package.json`；
   - 支持 `engines.node` 和明确的 `devEngines.runtime`；
   - 使用 semver range 选出最高匹配版本；
   - 明确冲突优先级：`osdk.toml` > `.tool-versions` > `.nvmrc` >
     `.node-version` > `package.json`。
2. **架构覆盖**
   - 增加 `-o arch=x64|arm64|x86`；
   - 锁文件写入目标平台，而非误写 host platform；
   - 禁止无法在当前 host 执行的组合，除非仅下载模式。
3. **Corepack**
   - 增加 `settings.node.corepack` 和安装参数；
   - 安装完成后使用该 Node 自带的 Corepack，不调用用户全局 Node。
4. **全局包迁移**
   - 提供显式 `osdk node migrate-packages --from <version> --to <version>`；
   - 默认只生成计划，`--apply` 后执行；
   - 排除 npm 自身和不可移植的 native addon。

#### 开源实现参考

- fnm 的 `--resolve-engines` 读取 `package.json#engines.node`；
- fnm 的 `--arch` 覆盖目标架构；
- fnm 的 `--corepack-enabled` 在每个新 Node 安装后运行 Corepack；
- nvm 提供 `--reinstall-packages-from` 迁移全局包。

参考：<https://github.com/Schniz/fnm/blob/master/docs/commands.md>

#### 验收标准

- 覆盖精确版本、范围、无效 range 和多文件优先级；
- Linux/macOS/Windows 都验证 Corepack shim；
- 锁文件能区分 host 与显式目标架构；
- 迁移命令有 dry-run 和失败回滚。

### 5. Python 多实现、多变体和可刷新 catalog

#### 问题

当前 Python backend 使用内置 PBS 索引并默认选择稳定、非 free-threaded 的
CPython `install_only` 资产。仍缺少：

- PyPy、GraalPy、Pyodide；
- CPython free-threaded、debug 等变体；
- prerelease 控制；
- 远程可刷新且可签名验证的完整 catalog；
- `python find` 一类解释器发现命令。

#### 修复方案

- 把请求模型扩展为
  `python@<implementation>-<version>+<variant>`，同时支持简写；
- catalog 记录 implementation、version、variant、platform、URL、checksum；
- 内置 known-good catalog，在线刷新写入版本化缓存；
- catalog 使用签名或固定 digest 验证，失败回退内置版本；
- 增加 `--prerelease=never|if-explicit|allow`；
- 增加 `osdk python find`，按 managed、PATH、系统解释器分层输出；
- 锁文件完整记录 implementation 和 variant。

#### 开源实现参考

- uv 支持 CPython、PyPy、Pyodide 和 GraalPy；
- uv 使用结构化 Python 下载元数据，不依赖运行时猜资产名；
- uv 的 `python find/list/install/upgrade` 将发现、安装和升级分开；
- mise 的 Python backend 兼容 `.python-version` 并支持多实现插件。

参考：<https://docs.astral.sh/uv/concepts/python-versions/>

#### 验收标准

- 每种实现至少有一个离线 fixture；
- free-threaded 与普通 CPython 可并存；
- prerelease 不会被 `latest` 意外选中；
- catalog 更新失败不破坏旧缓存。

### 6. Java 离线 catalog、JRE 和 JVM 工具生态

#### 问题

当前 Java backend 通过 Foojay Disco 在线查询多个 JDK distribution，并能使用
vendor checksum，但：

- 只能安装 JDK，不能请求 JRE；
- 空缓存离线时不能解析版本；
- Maven、Gradle、Kotlin 等 JVM 工具没有统一候选模型；
- Foojay 是单一元数据服务。

#### 修复方案

- 增加 `-o package-type=jdk|jre`，锁文件保存 package type；
- 定期生成各 distribution 的稳定版本 catalog 并嵌入发行版；
- 在线 Foojay 结果与内置 catalog 合并，stale cache 优先于失败；
- 将 Maven、Gradle、Kotlin 等实现为独立 backend，不与 Java 版本混在一个 id；
- 为 Foojay metadata 增加可配置镜像或静态 catalog URL；
- 对 vendor redirect、GitHub asset 和 checksum detail 分别做 failover 测试。

#### 开源实现参考

- SDKMAN 以 candidate 为单位管理 Java、Maven、Gradle、Kotlin、Ant 等工具；
- SDKMAN offline mode 仍能列出和切换已安装版本；
- mise 将 Java runtime 和 Maven/Gradle 等工具作为独立 backend 管理。

参考：

- <https://sdkman.io/sdks/>
- <https://sdkman.io/usage/>

#### 验收标准

- JDK/JRE 同版本可区分安装；
- 空缓存离线至少能解析内置 LTS catalog；
- Foojay 不可用时已缓存安装仍正常；
- 每个新增 JVM 工具有独立 checksum 和 smoke test。

### 7. Rust 独立 component、target、override 和 linked toolchain 管理

#### 问题

osdk 已能隔离 bootstrap rustup，并可在安装时通过 `-o components=...`、
`-o targets=...` 传入初始组件和目标。但缺少：

- 安装后的 component/target 增删；
- `check` / update 状态；
- directory override 管理；
- linked/custom toolchain；
- 对 rustup 状态与 osdk marker 的完整一致性修复。

#### 修复方案

- 新增 `osdk rust component add/remove/list`；
- 新增 `osdk rust target add/remove/list`；
- 新增 `osdk rust check`，解析隔离 rustup 的更新状态；
- 使用 osdk 项目配置替代隐式 rustup override；如需兼容，提供显式
  `osdk rust override import/export`；
- 新增 `osdk rust toolchain link <name> <path>`，锁文件禁止把本地 link 当成
  可复现远程 artifact；
- 所有 rustup 子命令必须注入隔离 `CARGO_HOME` / `RUSTUP_HOME`。

#### 开源实现参考

- rustup 原生提供 component、target、override、toolchain link 和 update check；
- rustup 的工具链选择优先级覆盖显式 `+toolchain`、directory override、
  `rust-toolchain.toml` 和 default toolchain；
- mise 通过项目配置统一其他语言和 Rust 的版本选择。

#### 验收标准

- 不访问用户真实 rustup home；
- add/remove 幂等；
- linked toolchain 不进入远程 lock artifact；
- Windows GNU/MSVC target 都有编译覆盖。

### 8. `packageManager` 自动识别和独立 npm

#### 问题

pnpm 和 Yarn 已有独立 backend，npm 仍随 Node 安装。osdk 不读取
`package.json#packageManager`，因此进入项目时不能自动选择 npm/pnpm/Yarn 的
精确版本；也不能在同一 Node 版本上独立切换 npm。

#### 修复方案

- 解析 `packageManager: "pnpm@9.15.0"` 等 Corepack 规范字段；
- 解析 `devEngines.packageManager`，并定义与 `osdk.toml` 的优先级；
- 自动把 Node 作为 pnpm/Yarn/npm 执行依赖；
- 新增 npm backend：从 npm registry 获取 `npm` 包并验证 SRI；
- npm 安装目录独立于 Node，但 shim 的 PATH 保证目标 Node 在前；
- 对不带版本、URL/hash 后缀和非法 package manager 值明确报错。

#### 开源实现参考

- Corepack 读取 `packageManager` 并按项目选择包管理器版本；
- proto 的 Node 工具链插件同时管理 Node、npm、pnpm 和 Yarn；
- mise 的 idiomatic file 支持可从 `package.json` 选择 Yarn 等工具。

#### 验收标准

- npm、pnpm、Yarn 三类字段都可解析；
- 离线锁定后不再访问 registry；
- npm 版本可独立于 Node 升级和卸载；
- package manager shim 不调用用户全局 Node。

### 9. GitHub backend 的可配置资产选择

#### 问题

当前 `github:owner/repo` 根据文件名启发式选择一个最匹配 host 的 asset。对于以下
release 容易选错或无法表达：

- 一个 release 中包含多个 CLI；
- 非标准 OS/arch 命名；
- 需要从归档子目录选多个文件；
- 安装后需要改名；
- 只想下载特定 platform；
- release 超过 API 第一页的 30 条；
- 企业内部使用静态 catalog，不允许访问 GitHub API。

#### 修复方案

- 分页读取 Releases API，直到匹配到版本或达到上限；
- backend options 增加：
  - `asset-regex`；
  - `asset-template`；
  - `bin` / `bins`；
  - `rename`；
  - `strip-components`；
  - `os` / `arch` / `libc`；
  - `catalog-url`；
- 显式规则优先于启发式匹配；
- 静态 catalog 包含 tag、asset URL、checksum 和 platform；
- 锁文件保存最终规则和 asset identity；
- 多文件安装必须原子提交，任一文件失败则不生成 complete marker。

#### 开源实现参考

- aqua registry 用模板描述 asset、format、files、replacements、checksum 和
  platform overrides；
- aqua 支持 GitHub Artifact Attestations、Cosign 和 checksum asset；
- ubi 提供 `--matching-regex`、`--rename-exe` 和解压选择；
- mise 的 ubi backend 暴露 matching regex，URL replacements 可统一改写代理。

参考：

- <https://aquaproj.github.io/docs/reference/registry-config/overrides/>
- <https://aquaproj.github.io/docs/reference/registry-config/checksum/>
- <https://github.com/itochan/ubi>

#### 验收标准

- 有多 asset release fixture；
- 分页场景可找到第 2 页版本；
- regex 0 命中和多命中都明确失败；
- rename 和多 binary 在 Windows 扩展名下行为一致；
- static catalog 模式完全不访问 GitHub API。

## P2：生态覆盖与测试

### 10. Deno/Bun prerelease、canary 和 nightly

#### 问题

当前 npm packument 已包含版本数据，但 osdk 主动过滤 prerelease，仅暴露稳定版。
用户无法明确请求 Deno canary、Bun canary/nightly 或其他预发布通道。

#### 修复方案

- 为版本信息保留 npm dist-tag 和 semver prerelease；
- 默认 `latest` 仍只选择稳定版；
- 显式 `@canary`、`@nightly`、`@beta` 解析 dist-tag；
- 显式 prerelease 精确版本始终允许；
- 增加全局 prerelease policy，行为与 Python/GitHub backend 一致；
- 锁文件固定最终精确版本，不能只保存 channel。

#### 开源实现参考

- mise/asdf 插件通常将上游 channel 或 dist-tag 映射为动态版本；
- rustup 将 stable、beta、nightly 作为一等 channel；
- uv 对 prerelease 有显式策略，而不是让 `latest` 隐式选择。

#### 验收标准

- 默认列表不混入 prerelease；
- 显式 channel 可解析且锁定为精确版本；
- channel 消失时 locked install 仍可离线复现。

### 11. 统一 backend contract 与网络故障矩阵

#### 问题

当前已有 107 项 Rust 测试、CLI subprocess 测试和所有内置 backend 的定时 live
smoke，但仍缺少可复用的 backend 合约。不同 backend 对同一种错误可能表现不同，
而在线 smoke 也不适合稳定覆盖故障。

仍需覆盖：

- `list -> resolve -> install -> execute -> uninstall`；
- 403、429、5xx、timeout、连接中断；
- stale mirror、畸形 metadata、缺 checksum；
- 并发安装同一版本、锁竞争、失败清理；
- 跨文件系统 copy fallback；
- corrupt manifest 和缓存污染；
- shim stdin/stdout/stderr、退出码、递归和冲突。

#### 修复方案

- 建立 `BackendContract` fixture harness；
- 每个 backend 使用同一组本地 HTTP 场景；
- 网络注入脚本可精确控制状态码、分块和连接中断；
- 将 live smoke 只用于发现上游漂移，不承担主要正确性证明；
- 对安装目录、cache receipt、lock 和 complete marker 做统一断言。

#### 开源实现参考

- rustup 使用本地 fake distribution server 和 CLI golden tests；
- uv 使用 snapshot integration tests 覆盖 Python install/find/list/pin/uninstall；
- SDKMAN 使用 WireMock 和行为驱动测试模拟服务故障；
- mise 测试下载续传、清理、锁、并发和 Windows 行为；
- proto 有 per-SDK 端到端脚本和 lock/plugin/shim 集成测试。

#### 验收标准

- 9 个内置 backend 全部通过统一 contract；
- 每种网络错误有确定错误类型；
- 失败后不存在 complete marker 或半成品 install；
- CI 不依赖公网即可覆盖全部故障。

### 12. Windows 运行时行为测试

#### 问题

Linux 上的 `x86_64-pc-windows-gnu` Clippy 已能检查所有 Windows cfg 和测试 target，
Windows runner 也运行 Rust 测试。但以下行为仍缺少专门的端到端断言：

- `.cmd` 和 Git Bash wrapper；
- junction/symlink 权限回退；
- PowerShell activation/deactivation 实际执行；
- NTFS volume 判断；
- Windows 进程退出码、stdin/stdout/stderr；
- 路径空格、非 ASCII 和长路径。

#### 修复方案

- 在 `windows-latest` 增加 PowerShell smoke 脚本；
- 使用临时 HOME 和全部 `OSDK_*` 目录；
- 安装本地 fixture backend，不访问公网；
- 分别从 PowerShell、cmd 和 Git Bash 调用 shim；
- 在可创建 symlink 与不可创建 symlink 两种模式下验证回退；
- 保留 Linux cross-Clippy，作为 Windows runner 之前的快速门禁。

#### 开源实现参考

- fnm 在 Linux、macOS、Windows 运行 alias、目录切换、exec 和 Corepack 测试；
- mise 在 Windows 覆盖 shim 和安装行为；
- rustup 使用平台专属 CLI 测试验证代理和工具链选择。

#### 验收标准

- Windows runtime smoke 不访问用户状态或公网；
- 三种 shell 均验证；
- 路径包含空格和中文；
- Linux cross-Clippy 与 Windows runtime test 都是必需检查。

## P3：上游安全能力

### 13. 完整 Rekor transparency-log 证明

#### 问题

当前 GitHub Artifact Attestation 验证已经检查：

- Fulcio certificate chain 和 SCT；
- GitHub Actions OIDC issuer 与仓库；
- artifact signature；
- DSSE subject digest；
- Rekor body consistency；
- signing time。

但上游 `sigstore` 0.14 verifier 尚未验证 Rekor Merkle inclusion proof 和 Signed
Entry Timestamp（SET），因此不能宣称完成透明日志证明。

#### 修复方案

- 跟踪上游 `sigstore` Rust crate 的 inclusion proof / SET 支持；
- 升级前保留当前限制说明；
- 上游支持后新增离线 proof fixture；
- `required` policy 在 proof 缺失或无效时失败；
- 如长期无上游支持，再评估引入独立 Rekor proof verifier，避免自行实现密码学。

#### 开源实现参考

- aqua 支持 GitHub Artifact Attestations、Cosign 和 SLSA provenance；
- Sigstore/Cosign 将 Rekor transparency log 作为完整供应链验证的一部分；
- GitHub CLI 的 attestation verify 可作为行为对照，但不应作为 osdk 的运行时依赖。

#### 验收标准

- 有有效、缺 proof、篡改 proof、错误 SET 四类 fixture；
- offline verification 不访问 Rekor；
- 文档明确区分签名有效与 transparency proof 完整。

## 推荐实施顺序

### 阶段 A：行为一致性和安全

1. GitHub 全链路 source/failover；
2. 统一 prompt/`--yes`；
3. trust 数据模型和危险字段边界；
4. 网络故障 matrix 基础设施。

### 阶段 B：项目自动发现

1. Node `engines.node`；
2. `packageManager` 和独立 npm；
3. prerelease policy；
4. Node Corepack。

### 阶段 C：生态深度

1. Python 多实现和 catalog；
2. Java JRE/offline catalog/JVM 工具；
3. Rust component/target/override；
4. GitHub asset rules 和 static catalog。

### 阶段 D：平台和长期安全

1. Windows runtime matrix；
2. 全 backend contract；
3. Rekor inclusion proof / SET。

## 完成定义

一项能力只有同时满足以下条件才可从本文移除：

- 功能代码和错误语义完成；
- 窄测试与回归测试完成；
- `README.md`、`site/guide/`、`site/en/guide/` 已同步 review；
- 离线、锁文件和 source 行为已评估；
- Windows/macOS/Linux 影响已评估；
- 使用临时 `HOME`、`OSDK_*`、`CARGO_HOME`、`RUSTUP_HOME` 和构建目录验证；
- 以独立 Git commit 交付。

## 竞品资料索引

- mise configuration：<https://mise.jdx.dev/configuration.html>
- mise trust：<https://mise.jdx.dev/cli/trust.html>
- mise URL replacements：<https://mise.en.dev/url-replacements.html>
- fnm commands：<https://github.com/Schniz/fnm/blob/master/docs/commands.md>
- uv Python versions：<https://docs.astral.sh/uv/concepts/python-versions/>
- SDKMAN usage：<https://sdkman.io/usage/>
- SDKMAN candidates：<https://sdkman.io/sdks/>
- rustup：<https://rust-lang.github.io/rustup/>
- aqua registry overrides：
  <https://aquaproj.github.io/docs/reference/registry-config/overrides/>
- aqua checksum：
  <https://aquaproj.github.io/docs/reference/registry-config/checksum/>
- ubi：<https://github.com/itochan/ubi>
- proto：<https://github.com/moonrepo/proto>
- gh-proxy：<https://gh-proxy.com/>
