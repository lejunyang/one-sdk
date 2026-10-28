# 全局 npm 登录认证误拦截修复与 Backend 对照

基准：2026-10-28 已拉取并快进到 `origin/main` 的 `3139626`。
工作分支：`fix/npm-global-auth-isolation`。开始时工作区干净；`.agents/skills` 不存在。
仅本地实现、测试和提交，不推送、发布、合并或部署。

## 复现与根因

用户执行 `osdk use -g npm:@xai-official/grok`，因为用户 npmrc 中存在认证信息，
收到 `cannot safely isolate global npm install: registry preflight passed through`。
本次没有读取用户真实 npmrc、token 或 Codex 记忆，也没有安装 Grok。

`package_registry::plan` 服务于原生包管理器命令：遇到认证时返回 `PassThrough`，
意思是保留原生配置、不注入匿名镜像。`global_npm_use/install.rs` 却把除 offline
之外的所有 `PassThrough` 都视为致命错误。全局安装的子进程本来就使用 `env_clear`
以及空的 user/global npmrc；因此仅存在将被隔离的登录凭据，不足以证明公共包不能安装。
npm 和 pnpm 两条全局路径共用此判断，均受影响。

## 修复边界

- 新增只供 npm/pnpm 隔离安装使用的 `plan_isolated`；普通 `plan` 行为不变。
- 被清空的环境和被替换的用户/global npmrc 中只有认证信息时，允许匿名安装。
  不提取、复制或转发凭据，不删用户配置，不放宽安装进程隔离。
- 已知公共原生 registry（npmjs/npmmirror）的显式 URL 保持原值，不因忽略认证改用镜像。
- 私有/未知 registry、scope 路由、带凭据的 URL、不可读配置、配置重定向、TLS 和
  原生代理策略仍保守拒绝。受管 Node 中的 npmrc 认证也不忽略，因为它未必属于替换的层。
- 原先显式 osdk registry 覆盖的优先级和 offline 规则不变。
  这不是私有包认证支持；默认公共源匿名访问失败时仍应报错。
- 新增 helper 不改 `Backend` trait、不增加依赖、不改 shim/hook-env 或 inventory 算法。
  复用原有受管 npmrc 列表，避免为原生命令增加一次目录扫描。

## 其他 Backend 审查

| 范围 | 实现与结论 |
| --- | --- |
| 全局 `npm:`，npm/pnpm 安装器 | 已确认同一个误拦截，均已修复并用真实 CLI + 本地 mock 安装验证。 |
| `npm:` synthetic-project 安装 | `backend/native_npm.rs` 已清空环境并使用自有配置；不调用该全局 fatal preflight，没有发现同类拦截。 |
| 项目 `use` / `deps`，`exec` / shim 中 npm、pnpm、Yarn Classic/Berry、Bun、Deno | `package_sources.rs` 对认证 `PassThrough` 保留原生策略，不转成全局隔离错误；不应套用匿名安装例外。 |
| `pypi:` / uv、pip | `python_index.rs` 遇到认证不选择镜像；`pypi::resolved_index` 对 `PassThrough` 返回 `None`，安装进程保留原生环境。这是预期防止覆盖私有索引，不是阻断安装。osdk 自己的 metadata 查询不等于原生认证能力，不能据此保证所有私有索引解析均可用。 |
| `cargo:` Registry / Git | `cargo_package.rs::command_env` 使用私有 HOME/CARGO_HOME，provider 清空环境，source 要求 canonical sparse HTTPS（Git 另有 canonical HTTPS 限制）。没有检查用户凭据存在就报本次错误；不能把“不导入用户认证”误报成完整私有 registry/Git 支持。 |
| `go:` | `go_package.rs` 清空 provider 环境，GOENV=off，使用声明的 GOPROXY，拒绝 credential-bearing proxy 和不受支持的自定义 header。custom source 不回退到公开默认源以避免泄露私有 module path。这些是预期边界。 |
| `conda:` | 自己查询/求解 channel repodata 并按摘要下载；channel 参数只接收合法名称，不导入用户 conda 认证文件，也没有 npm auth preflight。自定义服务的认证能力不等于原生 conda。 |
| `github:` | `http/mod.rs` 独立处理 OSDK_GITHUB_TOKEN/GITHUB_TOKEN/GH_TOKEN，仅对官方 API host 发 token，代理不获 token；无该 npm 拦截。私有 release 下载授权未做在线验证。 |
| `http:` | 刻意限制为公开 HTTPS + 必需摘要，不支持凭据 URL、自定义认证 header/跨源下载 redirect；属于已有产品限制。 |
| 固定 Node/npm/pnpm/Yarn/Bun/Deno/Python/Go/Rust/Java/Maven/Gradle/Kotlin/Zig/Android，以及声明式 backend | 通过各自 metadata/artifact/provider 路径获取工具，不把用户 npmrc 的认证转换为该全局安装错误。npm registry metadata 与“运行原生 npm”不是同一层；未做每个供应商的在线认证测试。 |

结论是“未在其余受查路径确认同类误拦截”，不是“所有私有认证场景都已支持”。
`Source.headers` 用于 osdk 自己的请求，仍不能据此推断会传入原生 npm/pnpm package fetch。

## 回归与验证

测试与构建的 HOME、OSDK_*、CARGO_HOME、RUSTUP_HOME、target 均在临时目录。
仅下载编译/验证工具和锁定依赖；所有新用例使用合成凭据和 loopback registry。

- `package_registry::tests::isolated_*`：npm/pnpm 的文件/环境认证、无关主机认证、匿名请求、普通命令透传；
  auth 与 private/scope/TLS/proxy/config-location/不可读文件共存时继续拒绝；公共原生源不漂移。
- `isolated_cli::auth_only_global_use_keeps_credentials_out_of_both_installers`：两个安装器实际
  完成 synthetic scoped package 安装，检查 `.osdk-install.json`、隔离文件和全部测试产物；
  用户 npmrc 保持原内容，子进程认证变量为空，文件/环境凭据未持久化。
- `unsafe_native_policy_fails_before_public_metadata_probe_or_installer_spawn`：私有源和 TLS
  策略在任何公共 metadata 请求/installer spawn 之前失败。
- 故障注入：只将 CLI 调回旧 `plan`，新增端到端测试立即以原始认证错误失败；随后恢复修复。
- 按 `osdk task list` 和 `osdk.toml` 的命名检查执行 fmt-check、clippy（`-D warnings`）、
  完整 workspace test 和 `./scripts/windows-wine-tests.sh`。没有用交叉编译替代 Wine 运行。
- `site` 对应的 Bun 1.4.3 / VitePress 生产构建通过；检查了生成的中英文 npm 页面正文。
  两份 README、双语 guide/internals、skills 使用指引已同步；没有新增站点页面，因此导航和
  redirects 经检查无需变更。

真实 Grok 安装和用户 npmrc 的其他策略未检查；因此只承诺修复已复现的“认证本身”误拦截。

最终结果：Linux workspace 2,155 passed / 0 failed / 5 ignored；Windows GNU Wine
2,045 passed / 0 failed / 5 ignored。完整 registry 专项 31 passed。fmt-check、严格 Clippy、
VitePress 生产构建均通过；ignored 是套件原有的外网/现场检查，不计入已执行验证。
详细运行日志保存在本机 `/tmp/osdk-auth-fix/`，包括 `workspace-final.log`、
`wine-final.log`、`registry-final.log`、`clippy-final.log`、`site-build.log` 和 `mutation.log`。
