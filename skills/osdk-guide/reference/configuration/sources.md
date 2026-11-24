# `[sources]`

控制 SDK、工具、模型和 osdk 自身的下载源选择。

```toml
[sources]
selection = "auto"          # auto / pinned / ordered
mode = "auto"
probe_timeout_ms = 1500
cache_ttl = "6h"
model_probe_timeout_ms = 8000
model_download_attempts = 6
model_download_retry_base_ms = 1000
model_jobs = 2
model_read_timeout_ms = 60000

[sources.node]
pin = "tuna"
disable = ["slow-source"]

[[sources.node.custom]]
id = "corp"
download_url = "https://mirror.example/node/"
index_url = "https://mirror.example/node/index.json"
forward_credentials = false

[sources.huggingface]
env = true
env_force = false
```

- `mode = "auto"`：环境镜像与内置候选一起探测排序。
- `mode = "env"`：只遵循环境镜像；缺失或不可用即失败。
- `pin` 是优先尝试，不是禁用其他回退源。对 Cargo/Conda/npm 原生来源覆盖，显式 pin
  （内置或自定义）优先于原生配置，并仅使用所选源，避免把私有包名送到无关源。
  `source list` 始终展示可管理的 osdk 候选；`source test` 使用安装的有效候选。
  用 `osdk source pin <tool> <id>` 持久固定，`source unpin <tool>` 恢复原生选择。
  再安装同版本时会核对产物来源；同源复用，换源重新安装，失败保留原安装。
- probe 成功只影响排序；osdk 直连产物在校验、解包或必需文件检查失败时仍会换下一个源。
- 委托安装器使用安全的目标级策略：Rust 逐源执行 rustup，Cargo 预检精确 crate URL，
  `pypi:` 探测实际项目页，`go:` 在全新解析时生成 `|` 分隔的 GOPROXY 回退链。
- `cargo:<crate>` 从 `$CARGO_HOME/config.toml`（默认 `~/.cargo/config.toml`）读取
  crates.io 替换源。匿名 sparse HTTPS 源独占版本查询和安装，不导入登录凭据。
  旧文件名 `config` 优先，支持 source.registry 和 registries.index 替换目标。
  USTC/TUNA/GitHub rust-lang/rsproxy 的已知公开 git 索引告警后沿用默认源；未知Git、
  目录、认证源仍报错。受管 shell 和 `osdk exec --tool rust@…` 内的 osdk 仍读取用户原来的
  `CARGO_HOME`（默认 `~/.cargo`）。锁回放要求本机配置同一 index，
  缺失/不同报错并要求配置或重锁。
- `conda:<包名>` 遵循 `CONDARC`、`~/.condarc` 等配置的 `channel_alias`、
  `default_channels`、`custom_channels`，这些字段才启用原生来源。文件 `channels`
  不改变工具安装；局部配置未定义的普通频道仍用公开 alias。`CONDA_CHANNEL_ALIAS`、
  `CONDA_DEFAULT_CHANNELS`、`CONDA_CUSTOM_CHANNELS` 优先于文件；原生模式下
  `CONDA_CHANNELS` 选择频道，工具 `channels` 选项优先。默认列表合并去重，映射按名覆盖。
  相关合并指令告警后按普通优先级处理，无关指令不拦。匿名 HTTP(S) 原生频道独占查询、
  求解与下载。复杂路由或认证地址明确报错；可用自定义源加 `--source <id>` 覆盖。
- `github:owner/repo` 设置 `OSDK_GITHUB_TOKEN`、`GITHUB_TOKEN` 或 `GH_TOKEN` 时
  默认绕过内置镜像，保留显式 custom relay 和非 GitHub catalog/artifact 地址。
  带令牌时，官方 `github` pin/`--source github` 只走官方查询及下载；显式 relay 只走所选 relay。
  令牌只发官方 HTTPS API，不发给非 GitHub 主机。GitHub Enterprise 自定义 API 未支持。
- `forward_credentials` 只对明确允许的自定义端点转发 provider 凭据。
- 特殊 key：`self` 管 osdk 自升级，`go-modules` 管 `GOPROXY`。

优先用 `osdk source add/remove/pin/unpin` 修改，避免手工写错 URL 或凭据策略。
