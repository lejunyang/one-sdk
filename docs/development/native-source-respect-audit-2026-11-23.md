# 各 backend 对用户私有源的处理（待修）

基准：2026-11-23 的 `main`（0.0.15，`baa79994`）。只读审查，未改代码。

## 背景

0.0.15 修了全局 `npm:` 安装：用户 `~/.npmrc` 里配了公司内网源这类不认识的 `registry` 时，以前直接拒绝安装（“private or unknown native registry is configured”）；现在照原地址使用，不带凭据，也不换成公共镜像（`package_registry.rs` 的 `native_registry_candidates_with_auth_policy`，`isolated` 分支）。见 `npm-global-auth-audit-2026-10-28.md` 开头的更新说明。

顺着这个问题把其余 backend 过了一遍。只有全局 npm 会直接拒绝；其他几处的问题是**悄悄不用用户配的源**：要么内网环境装不上，要么从公共源装到同名的另一个包，要么把私有包名、模块路径发到外网。用户能在 osdk 的 `[sources]` 里写自定义源绕开，但用户不知道自己配的源被绕开了。

原则（和 npm 那次一致）：用户在生态自己的配置里指定了源，就用这个源；做不到时明确报错，并说明怎么在 osdk 里指定源，不要悄悄换成公共源。

## 发现

“已核对”表示对照源码确认过；其余来自代码阅读，修之前要再核对行号和行为。路径相对 `crates/osdk-core/src/`。

| 优先级 | backend | 现状 | 位置 | 状态 |
| --- | --- | --- | --- | --- |
| 高 | `pypi:`，pip 安装方式 | 总是让 pip 读一份 osdk 自己的空配置（注释写明是为了不让用户的 pip.conf 改掉源）。用户 `pip.conf` 的 `index-url` 让 osdk 不接管源，但 pip 又读不到它，于是退回 pypi.org | `backend/pypi.rs` 约 294–302 | 已核对 |
| 高 | `pypi:`，查最新版本 | osdk 不接管源时，`list_remote_versions` 用 `.unwrap_or(PYPI)`：私有包的版本去 pypi.org 查，有依赖混淆风险 | `backend/pypi.rs` 约 1036–1039 | 已核对 |
| 中 | `pypi:`，uv 安装方式 | `python_index::credential_configuration` 只看 pip.conf、uv.toml、.netrc、`PIP_INDEX_URL`；只设了 `UV_DEFAULT_INDEX` / `UV_INDEX_URL` / `UV_INDEX` / `UV_EXTRA_INDEX_URL`，或 uv.toml 在 `%APPDATA%\uv\`、`$XDG_CONFIG_HOME/uv/`，或设了 `PIP_CONFIG_FILE` 时，不算“用户配了源”，osdk 会用自己的源覆盖 | `python_index.rs` 约 145–178、451–470；`backend/pypi.rs` 约 182、367–380 | 未核对 |
| 高 | `go:` 模块 | 用户的 `GOPROXY` 只是和 proxy.golang.org、goproxy.cn 一起排序的候选之一；`https://corp,direct`、末尾带斜杠、`http://` 会被丢掉（只给警告）；`go env -w` 写的值和 `GOPRIVATE` 不看。查最新版本时按候选逐个去问，私有模块路径会发到公共代理 | `backend/go_package.rs` 约 405–425（已核对：按 sources 逐个查 latest）、459–480、514–548（`GOENV=off`、`GONOPROXY=none`、`GOSUMDB=off`）、732–747 | 部分核对 |
| 中 | go SDK 的 shim 环境 | 没设 `GOPROXY` 环境变量时注入 `proxy.golang.org\|goproxy.cn\|direct`，盖掉用户 `go env -w GOPROXY=…` 写进 Go 环境文件的值。`go mod` 那条路径已经会检查这个文件 | `backend/go.rs` 约 207–208、324–327；可复用 `package_sources.rs` 约 76–99 | 未核对 |
| 中 | `cargo:` | 用私有 `CARGO_HOME`，用户 `~/.cargo/config.toml` 的 `[source.crates-io] replace-with`、私有 registry、网络设置都不读，用 crates.io 或 rsproxy | `backend/cargo_package.rs` 约 110–123、742–765、1207–1216 | 未核对 |
| 中 | `conda:` | 不读 `.condarc` 的 channels、`channel_alias`、`custom_channels`，用 anaconda 加清华、北外、南大镜像 | `backend/conda.rs` 约 105–131、800–806、1172–1178 | 未核对 |
| 低 | `npm:` 项目内安装（synthetic project） | 写自己的 `.npmrc`，不读用户 `~/.npmrc` 的 `registry`；可按 0.0.15 全局安装的规则统一 | `backend/npm_package/mod.rs` 约 1414–1420；`backend/npm_cli.rs` 约 111–118 | 未核对 |
| 低 | `github:` | API 地址写死 `api.github.com`，不支持 GitHub Enterprise；请求会经过 gh-proxy 镜像，私有仓库名会被发到外部。令牌只发给 api.github.com | `backend/github.rs` 约 117–130、609–615；`http/mod.rs` 约 756–761 | 未核对 |
| 低 | rustup | `RUSTUP_DIST_SERVER` 和公共镜像一起排序，不优先 | `backend/rust.rs` 约 348–355；`source/env.rs` 约 208–230 | 未核对 |
| 低 | node、python、java、maven、gradle SDK | 不读 `NODEJS_ORG_MIRROR`、maven `settings.xml`、gradle init script；下载的是固定的官方发行包，风险低 | `backend/node.rs` 约 234、`python.rs` 约 67、`java.rs` 约 193、`jvm_tools.rs` 约 263 | 未核对 |

没有问题的：`http:`（URL 本来就是用户给的）；项目里的 npm/pnpm/yarn/bun/deno 命令（用户配了源或认证就原样透传，不拒绝）；项目 `uv`（尊重 `UV_*` 和原生配置，只有 osdk 自己也设了源且原生配置复杂时才拒绝，并提示 `precedence.python`）。

## 建议的改法

1. **pip**：只在 osdk 自己选了源时才用空配置；用户配了 `index-url` 时把它作为 `PIP_INDEX_URL` 传给 pip。
2. **pypi 查版本**：osdk 不接管源时，去用户的源查（取自 `PIP_INDEX_URL` / `UV_DEFAULT_INDEX` 或配置文件）；拿不到就明确报错，不退回 pypi.org。
3. **uv 的判断**：把上表列的环境变量和配置位置都算作“用户配了源”，osdk 让开（`package_sources.rs` 约 162–167 已经这样做）。
4. **go 模块**：环境变量或 `go env` 文件里有 `GOPROXY` / `GOPRIVATE` 时，只用用户的代理并保留其回退列表（含 `direct`），私有模块路径不发往公共代理。
5. **go SDK shim**：Go 环境文件里已有 `GOPROXY` 时不注入。
6. **cargo**：读用户配置里 `[source.crates-io] replace-with`；指向 sparse registry 时把它作为唯一的源（不带凭据），和 npm 一样。
7. **conda**：读 `.condarc` 的 `channel_alias` / `default_channels` 作为基础地址，至少在忽略时给出警告。
8. **npm 项目内安装**：按 0.0.15 全局安装的规则读 `~/.npmrc` 的 `registry`。
9. **github**：带令牌时不走 gh-proxy；GitHub Enterprise 需要可配置的 API 地址。

每一项都要有两个方向的测试：配了私有源时用它（且不带凭据、不探测公共源），没配时行为不变；参照 `crates/osdk-cli/tests/isolated_cli.rs` 的 `private_native_registry_global_use_installs_from_it_without_credentials`。改完同步两份 README、双语站点和 `skills/`。
