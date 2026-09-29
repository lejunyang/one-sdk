---
name: osdk-guide
description: >-
  osdk（one-sdk）CLI 与配置的分区使用指引。用于安装、选择、锁定、升级或卸载工具，
  运行项目任务与内嵌 Lua，管理应用依赖、模型、Agent skills、Android/Rust/Node/Python
  专属工作流、下载源、Registry、容器、宿主包、缓存、shell 集成、信任和诊断；也用于
  编写或排查 osdk.toml、.osdk.toml、config.toml 与 osdk.lock。仅覆盖 osdk 本身，
  不替代被管理语言或工具的使用文档。
---

# osdk 使用指引

把本页当路由表。先识别一级命令或 TOML 顶层段，只读取直接对应的 reference；不要先
加载整套文档。实际二进制版本的参数面以 `osdk <command> --help` 为最终权威。

## 使用流程

1. 先运行 `osdk --version`，需要精确参数时再运行对应层级的 `--help`。
2. 从下面的命令或配置索引读取相关 reference。
3. 写配置前确认作用域：项目 `osdk.toml` / `.osdk.toml`，或用户
   `$OSDK_CONFIG_DIR/config.toml`。
4. 执行会改系统、删除数据或改下载来源的命令前，读取对应 reference 的安全边界。
5. 在 one-sdk 仓库开发 CI 时，以 `osdk task list` 为检查清单，不从 workflow 手抄命令。

GitHub Actions 中安装 osdk、恢复缓存并初始化项目环境时，读取
[GitHub Actions](reference/github-actions/guide.md)。

## 先记住的边界

- 使用 `osdk run <name>` 执行任务；不存在裸 `osdk <name>`。
- 使用 `install` 安装工具，使用 `deps` 安装项目自身依赖，使用 `model sync` 下载模型。
- 动态工具使用 `npm:`、`cargo:`、`go:`、`conda:`、`pypi:`、`github:`、`http:` 前缀。
- CLI → 环境变量 → 项目配置 → 用户配置 → 内置默认，优先级依次降低。
- 需要执行代码或改变字节来源的项目配置先用 `osdk trust`；安全声明不会无故要求信任。
- `skills/` 是给外部 AI Agent 读取的内容；它不是 osdk 从 data/config 的 `plugins`
  目录加载的声明式 backend。

通用参数见 [全局参数](reference/global-options/guide.md)，工具请求语法见
[工具请求与 backend 选项](reference/tool-requests/guide.md)。

## 一级命令索引

### 工具生命周期与复现

| 一级命令 | 读取 |
| --- | --- |
| `install` / `i` | [reference/install/guide.md](reference/install/guide.md) |
| `use` / `u` | [reference/use/guide.md](reference/use/guide.md) |
| `uninstall` / `rm` | [reference/uninstall/guide.md](reference/uninstall/guide.md) |
| `list` / `ls` | [reference/list/guide.md](reference/list/guide.md) |
| `list-remote` / `lsr` | [reference/list-remote/guide.md](reference/list-remote/guide.md) |
| `current` | [reference/current/guide.md](reference/current/guide.md) |
| `where` | [reference/where/guide.md](reference/where/guide.md) |
| `reshim` | [reference/reshim/guide.md](reference/reshim/guide.md) |
| `lock` | [reference/lock/guide.md](reference/lock/guide.md) |
| `outdated` | [reference/outdated/guide.md](reference/outdated/guide.md) |
| `upgrade` | [reference/upgrade/guide.md](reference/upgrade/guide.md) |
| `exec` | [reference/exec/guide.md](reference/exec/guide.md) |

### 项目任务与依赖

| 一级命令 | 读取 |
| --- | --- |
| `run` | [reference/run/guide.md](reference/run/guide.md)；Lua 直接读 [reference/run/lua.md](reference/run/lua.md) |
| `task` | [reference/task/guide.md](reference/task/guide.md) |
| `deps` | [reference/deps/guide.md](reference/deps/guide.md) |

### 平台与生态工作流

| 一级命令 | 读取 |
| --- | --- |
| `node` | [reference/node/guide.md](reference/node/guide.md) |
| `python` | [reference/python/guide.md](reference/python/guide.md) |
| `rust` | [reference/rust/guide.md](reference/rust/guide.md) |
| `android` | [reference/android/guide.md](reference/android/guide.md) |
| `model` | [reference/model/guide.md](reference/model/guide.md) |
| `skills` | [reference/skills/guide.md](reference/skills/guide.md) |
| `container` | [reference/container/guide.md](reference/container/guide.md) |
| `pkg` | [reference/pkg/guide.md](reference/pkg/guide.md) |

### 配置、源、安全与维护

| 一级命令 | 读取 |
| --- | --- |
| `source` | [reference/source/guide.md](reference/source/guide.md) |
| `registry` | [reference/registry/guide.md](reference/registry/guide.md) |
| `config` | [reference/config/guide.md](reference/config/guide.md) |
| `trust` | [reference/trust/guide.md](reference/trust/guide.md) |
| `untrust` | [reference/untrust/guide.md](reference/untrust/guide.md) |
| `cache` | [reference/cache/guide.md](reference/cache/guide.md) |
| `prune` | [reference/prune/guide.md](reference/prune/guide.md) |
| `doctor` | [reference/doctor/guide.md](reference/doctor/guide.md) |
| `self` | [reference/self/guide.md](reference/self/guide.md) |

### Shell 与帮助

| 一级命令 | 读取 |
| --- | --- |
| `activate` | [reference/activate/guide.md](reference/activate/guide.md) |
| `deactivate` | [reference/deactivate/guide.md](reference/deactivate/guide.md) |
| `completions` | [reference/completions/guide.md](reference/completions/guide.md) |
| `hook-env`（隐藏内部命令） | [reference/hook-env/guide.md](reference/hook-env/guide.md) |
| `help`（Clap 自动入口） | [reference/help/guide.md](reference/help/guide.md) |
| `alias` | [reference/alias/guide.md](reference/alias/guide.md) |

## 配置段索引

先读 [配置总览](reference/configuration/overview.md)，再只读取涉及的顶层段：

| 配置段 | 读取 |
| --- | --- |
| `[settings]` | [reference/configuration/settings.md](reference/configuration/settings.md) |
| `[tools]` | [reference/configuration/tools.md](reference/configuration/tools.md) |
| `[alias.tools]` | [reference/configuration/alias.md](reference/configuration/alias.md) |
| `[sources]` | [reference/configuration/sources.md](reference/configuration/sources.md) |
| `[registries]` | [reference/configuration/registries.md](reference/configuration/registries.md) |
| `[container]` | [reference/configuration/container.md](reference/configuration/container.md) |
| `[sys.pkg]` | [reference/configuration/sys.md](reference/configuration/sys.md) |
| `[tasks]` | [reference/configuration/tasks.md](reference/configuration/tasks.md)；Lua 另见 [reference/run/lua.md](reference/run/lua.md) |
| `[task]` | [reference/configuration/task.md](reference/configuration/task.md) |
| `[deps]` | [reference/configuration/deps.md](reference/configuration/deps.md) |
| `[models]` | [reference/configuration/models.md](reference/configuration/models.md) |
| `[skills]` | [reference/configuration/skills.md](reference/configuration/skills.md) |

不要根据本页摘要猜未列出的参数。对状态变化命令，先用 `--dry-run` / plan / preview
（若支持）并读取对应 reference；没有预览能力时精确限定工具、模型、skill 或路径。
