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
- `pin` 是优先尝试，不是禁用其他回退源。
- probe 成功只影响排序；osdk 直连产物在校验、解包或必需文件检查失败时仍会换下一个源。
- 委托安装器使用安全的目标级策略：Rust 逐源执行 rustup，Cargo 预检精确 crate URL，
  `pypi:` 探测实际项目页，`go:` 在全新解析时生成 `|` 分隔的 GOPROXY 回退链。
- `forward_credentials` 只对明确允许的自定义端点转发 provider 凭据。
- 特殊 key：`self` 管 osdk 自升级，`go-modules` 管 `GOPROXY`。

优先用 `osdk source add/remove/pin/unpin` 修改，避免手工写错 URL 或凭据策略。
