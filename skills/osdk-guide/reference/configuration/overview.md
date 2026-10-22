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
| `[tasks]` | [tasks.md](tasks.md) | 正文不检查；缺失的 `tools` 需要安装时追加安装作用域 |
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

显式继承：子项目 `osdk.toml` 可设置 `extends = "../../osdk.toml"`。目录嵌套不会自动继承。优先级为 CLI/环境变量、子文件字段、显式继承的祖先字段、用户配置、默认值。映射按键合并（包括 env），标量与列表替换，`false`、`[]` 都是有效覆盖。继承的依赖 sources/outputs/dir 与任务 dir/file 路径属于声明它的文件。发现 roots/projects 留在根目录，已发现的子配置不能重定义整个目录。继承子项目以根项目声明为默认，显式 provider 字段优先；独立子配置自行声明 provider。根/子目录调用依赖使用同一目录与有效 provider/工具选项。继承链中每个可执行或改变来源的配置都单独检查信任，信任子文件不代表信任父文件。自身循环、非祖先引用及越过仓库边界均报错。`[task].roots` 仍单独负责聚合；继承的 runner 默认值转换到具体子任务。
