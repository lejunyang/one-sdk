# 配置总览

osdk 读取两类 TOML，字段形状相同：

- 项目配置：从当前目录向上查找 `osdk.toml` 或 `.osdk.toml`，适合提交到仓库。
- 用户配置：`$OSDK_CONFIG_DIR/config.toml`，用 `osdk config path` 定位。

优先级：CLI → `OSDK_*` 环境变量 → 项目配置 → 用户配置 → 内置默认。

## 顶层段

命名按配置语义而不是机械追随 CLI 的单复数：`[tools]`、`[tasks]`、`[models]`、
`[skills]`、`[sources]`、`[registries]` 是具名条目的集合；`[task]`、`[container]`、
`[alias]`、`[sys]` 是单个子系统的设置或命名空间。因此 `osdk task` 管理 `[tasks]`
里的一个条目，而 `[task]` 保存所有任务共享的 runner 设置。

门按命令作用域开启，不是按整份文件。各段在哪个作用域被检查：

| 段 | 参考 | 何时检查 |
| --- | --- | --- |
| `[settings]` | [settings.md](settings.md) | 安装作用域，仅校验类字段 |
| `[tools]` | [tools.md](tools.md) | 安装作用域，仅 `allow_builds` |
| `[alias.tools]` | [alias.md](alias.md) | 不检查 |
| `[sources]` | [sources.md](sources.md) | 安装、依赖作用域 |
| `[registries]` | [registries.md](registries.md) | 安装、依赖作用域 |
| `[container]` | [container.md](container.md) | container 作用域 |
| `[sys.pkg]` | [sys.md](sys.md) | 仅 `pkg apply`，且有条目在本机适用 |
| `[tasks]` | [tasks.md](tasks.md) | 不检查（键入 run 即授权） |
| `[task]` | [task.md](task.md) | run 作用域 |
| `[deps]` | [deps.md](deps.md) | 依赖作用域，视字段而定 |
| `[models]` | [models.md](models.md) | 不检查 |
| `[skills]` | [skills.md](skills.md) | 安装作用域，仅自定义 endpoint |

`osdk.lock` 是解析后的不可变结果，不是手写配置。用 `lock`、`install`、`model sync`
和 `skills sync` 维护它。不要手写 osdk 的 data/config 管控目录；源和设置优先通过
`osdk source` / `osdk config` 修改。

旧版 `[aliases]`、`[containers]`、`[task_config]`、`[syspkg]` 与临时的
`[sys.pkg.packages]` 暂时仍可读取；新旧同类段
不能同时出现。用 `osdk config migrate --dry-run` 预览，再运行 `osdk config migrate`
迁移项目配置；加 `--global` 迁移用户配置。命令保留注释和无关段，之后所有 osdk 写操作
只生成新布局。

平台过滤在 `[tools]`、`[tasks]`、`[models]`、`[skills]` 和 `[sys.pkg]` 的包条目中
复用同一组 OS/arch token。前四者放在 `when` 下：

```toml
when = { os = ["linux", "macos"], arch = "arm64" }
```

系统包对象直接使用 `os` / `arch` 字段：

```toml
[sys.pkg]
"apt:gcc" = { version = "latest", os = "linux", arch = ["x86_64", "arm64"] }
```

OS token：`windows`、`macos`（也接受 `darwin`）、`linux`；arch token：
`x86_64`（也接受 `amd64`）、`arm64`（也接受 `aarch64`）。未知维度或 token 硬报错。
