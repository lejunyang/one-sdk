# 配置总览

osdk 读取两类 TOML，字段形状相同：

- 项目配置：从当前目录向上查找 `osdk.toml` 或 `.osdk.toml`，适合提交到仓库。
- 用户配置：`$OSDK_CONFIG_DIR/config.toml`，用 `osdk config path` 定位。

优先级：CLI → `OSDK_*` 环境变量 → 项目配置 → 用户配置 → 内置默认。

## 顶层段

| 段 | 参考 | 信任 |
| --- | --- | --- |
| `[settings]` | [settings.md](settings.md) | 通常不需要 |
| `[tools]` | [tools.md](tools.md) | 仅声明工具不需要 |
| `[aliases]` | [aliases.md](aliases.md) | 不需要 |
| `[sources]` | [sources.md](sources.md) | 改写来源时需要 |
| `[registries]` | [registries.md](registries.md) | 需要 |
| `[containers]` | [containers.md](containers.md) | 镜像改写时需要 |
| `[syspkg]` | [syspkg.md](syspkg.md) | 需要 |
| `[tasks]` | [tasks.md](tasks.md) | 运行时需要 |
| `[task_config]` | [task-config.md](task-config.md) | 需要 |
| `[deps]` | [deps.md](deps.md) | 视字段而定 |
| `[models]` | [models.md](models.md) | 自定义 endpoint 需要 |
| `[skills]` | [skills.md](skills.md) | 自定义 endpoint 需要 |

`osdk.lock` 是解析后的不可变结果，不是手写配置。用 `lock`、`install`、`model sync`
和 `skills sync` 维护它。不要手写 osdk 的 data/config 管控目录；源和设置优先通过
`osdk source` / `osdk config` 修改。

平台过滤在 `[tools]`、`[tasks]`、`[models]`、`[skills]` 和 `[syspkg.packages]`
复用同一词汇：

```toml
when = { os = ["linux", "macos"], arch = "arm64" }
```

OS token：`windows`、`macos`（也接受 `darwin`）、`linux`；arch token：
`x86_64`（也接受 `amd64`）、`arm64`（也接受 `aarch64`）。未知维度或 token 硬报错。
