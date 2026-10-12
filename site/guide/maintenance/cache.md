# 缓存与清理

本页说明 osdk 的缓存分层、相关命令，以及删除操作的确认规则。目录位置见[目录与存储](../config/storage)。

## 缓存层与命令

```text
osdk cache dir
osdk cache env
osdk cache clean
osdk prune [--dry-run]
```

| 命令 | 当前行为 |
| --- | --- |
| `cache dir` | 打印缓存根、downloads、CAS 和 downstream 根目录 |
| `cache env` | 打印所有受支持的下游原生缓存环境映射 |
| `cache clean` | 只删除并重建 `<cache>/downloads` |
| `prune --dry-run` | 当前只输出演练提示，不计算或列出候选对象 |
| `prune` | 以 SDK installs 与模型 snapshots 的 manifest 为根，删除无引用 CAS 对象 |

`cache clean` 不删除 CAS、安装、模型、remote/source metadata 或 `<cache>/pkg`。
GC 遇到损坏 manifest 会拒绝继续，防止误删仍在使用的对象。

`osdk container cache status` 是另一条路径：它通过受支持的原生聚合接口查询 Docker
Engine 或 Buildx 构建器自有的存储。详见[容器运行时、Registry 与原生操作](../system/containers)。

通用 shell hook 在用户未设置时还映射
`npm_config_cache`、`PIP_CACHE_DIR`、`UV_CACHE_DIR`、`GOMODCACHE`、`GOCACHE`、
`CARGO_HOME` 和 `GRADLE_USER_HOME`。uv 不读 `pip.conf`，也不认 `PIP_*` 设置，
因此它需要自己的 `UV_CACHE_DIR`（`<cache>/pkg/uv`）；`PIP_CACHE_DIR` 覆盖不到它。
这里没有 Maven `M2_HOME`/`maven.repo.local` 重定向。Rust 的
直接 shim/`exec` 会以 `<data>/cargo` 覆盖通用 `<cache>/pkg/cargo` 映射。
Cargo 开发工具 provider 不会把两者当作构建 cache：每次安装都有 stage 私有的 `HOME`、
`CARGO_HOME`、target 与安装根。只有符合条件的受控 `cargo-binstall` 会从
`<data>/cargo/bin` 发现；临时 source/build workspace 会在发布前删除。
Go 开发工具 provider 则把 module/build cache 固定到 `<cache>/pkg/go-mod` 和
`<cache>/pkg/go-build`；其 `HOME`、`GOPATH`、`GOBIN` 与临时目录属于 stage 私有状态，
发布前会删除。

## 删除与确认

```text
osdk uninstall|rm TOOL@VERSION
osdk cache clean
osdk prune [--dry-run]
osdk model remove NAME
```

`uninstall`、`cache clean` 和非演练 `prune` 需要确认。交互终端显示提示；非交互
环境必须使用 `--yes`、`OSDK_YES=true` 或 `settings.yes=true`，否则失败。
`--quiet` 只关闭进度，不代表同意。`model remove` 当前不要求确认。
对 osdk 自有动态工具，uninstall 会派生完整配置身份，只删除对应指纹化根；相同 backend 和
version 的其他身份仍然保留。
