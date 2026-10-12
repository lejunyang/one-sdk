# 项目配置信任

```text
osdk [--yes] trust [PATH]
osdk trust list
osdk untrust [PATH]
```

`PATH` 可为配置文件或目录；目录会从该处向上找最近项目配置。

## 哪些键需要信任

判据仍然按**键**，但门是**按命令的作用域**开的：一个命令只会被它实际能碰到、
能让其生效的键挡住。信任的本质是「未经审阅的配置不得做事」，所以从不做事的命令
那里什么也收不到。

四类作用域及其检查的键：

| 作用域 | 到达它的命令 | 检查的键 |
| --- | --- | --- |
| 安装 | `install`、`use`、`upgrade`、`lock`、`self upgrade`；以及所选 task 需要获取缺失 `tools` 时的 `run` | `settings` 的校验开关与 catalog、`sources`、`registries`、`tools.allow_builds` |
| 依赖 | bare `install`、`run`/`exec`（默认）、显式 `deps` | `sources`、`registries`、`[deps]` 内的 index/registry/build/自定义 `run` |
| 运行任务 | `run`（非 `--dry-run`） | `[task]`：其 `shell` 决定每个任务由谁解释 |
| 系统包 | `pkg apply` | `[sys.pkg]` 中**在本机适用**的条目 |
| 容器 | 任何 `container` 操作 | `[container]` 的 runtime/builder/registries |

受管键与各自的原因：

| 键 | 原因 |
| --- | --- |
| `[sources]`、`[registries]` | 改变子进程的下载目的地 |
| `settings` 的 `verify_signatures`、`require_checksums`、`attestations` | 关掉或降级对产物的校验 |
| `settings` 的 `python`、`java` | 二者的 `catalog_url` 决定安装哪份运行时字节 |
| `[tools]` 中显式打开的 `allow_builds` | 唯一让 npm 生命周期脚本得以运行的开关 |
| `[deps.<p>]` 的 index/registry/自定义 `run`/build 开关 | 重定向依赖来源或执行任意命令 |
| `[sys.pkg]`（仅 `pkg apply`） | 装到系统全局、可能提权，且不受 `osdk.lock` 覆盖 |

两个值得注意的收紧：

- **`[sys.pkg]` 只挡 `pkg apply`，且只在有适用条目时。** 一个所有包都声明为
  `os = "linux"` 的配置，在 Mac 上不挡任何命令；管理器在本机不存在的条目（如
  Windows 上的 `apt:` 条目）同样不挡——在这里永远不会执行的包不需要审阅。
  `pkg status`/`plan`/`doctor` 本来就只读，同样不要求信任。
- **`[models]` 在任何命令里都不要求信任，** 包括写了 `endpoint` 的条目。模型
  字节是内容，osdk 从不执行它们；下载仍按锁定摘要校验，换端点无法把内容拉取
  变成代码执行。

**声明安装哪些工具或包本身不需要信任**，`[tools]`、`[alias.tools]` 都不需要，其中的
`npm:`、`github:`、`http:`、`go:`、`cargo:`、`pypi:`、`conda:` 条目也不需要：npm 安装默认
传 `--ignore-scripts`，`http:` 制品缺 `sha256` 直接拒绝，`go:` 以 `CGO_ENABLED=0` 构建。
这和在 `package.json` 里加一行依赖是同一件事——新增包、升降版本都不会要求重新信任。

`settings.node`（只有 `corepack` 一个 bool）和 `settings` 的 npm 选择（
`default_installer`，在 npm 与 pnpm 间二选一，且是最低优先级兜底）同样不需要信任。

两处 fail-closed：**未知的顶层 section**（对会动手的命令）和**未登记的 `settings`
键**都判为需要信任。本 build 无法解释的键，不会因为不认识而放行。

被拒绝时 osdk 会逐条列出命中的键及各自原因，而不是只说"未受信任"。

## 拦截范围：命令作用域决定一切

- **只读查看**：`list`、`current`、`where`、`doctor`、`completions`、`config get`/
  `config list`，以及 `task list` / `task info` / `task deps`。它们只汇报、展示状态，
  不安装、不下载、不执行任何东西，所以即使配置含受管键也不被拒绝。这也正是你
  **决定要不要信任之前**会用的命令——把它们挡住，等于把判断依据和出口一起藏起来。
- **信任管理本身**：`trust`、`untrust`，不读取项目配置。
- **`config set` / `config unset` / `config migrate`**：把一份未信任配置改回正常的手段，挡住它就
  等于用那份配置本身堵死了唯一出口。
- 其余命令按上表的作用域把关。一个命令可以带多个作用域：bare `install` 同时
  到达「安装」与「依赖」，`run`/`exec` 默认同时到达各自作用域与「依赖」；
  `--no-deps` 摘掉依赖作用域，`--dry-run` 让 run 一个作用域都不剩。

**经 shim 分派的工具是另一条线。** `cargo`、`node` 这类命令由 shim 启动，而
shim 只对它自己会走到的键把关（`sources`、`registries` 之类决定子进程从哪拉取
的）。像 `[sys.pkg]`、`[task]` 这种 shim 永远读不到的表，不会影响你在该目
录下正常使用工具。

## 信任身份与失效

```bash
osdk --yes trust                 # 最近项目配置
osdk --yes trust ./osdk.toml     # 指定文件
osdk trust list                  # 列出记录及其状态
osdk untrust                     # 撤销最近项目配置
osdk trust prune --dry-run       # 预览将清理哪些死记录
osdk trust prune                 # 清理配置文件已不存在的记录
```

`trust list` 给出四种状态，它们的正确处置并不相同：

| 状态 | 含义 | 该做什么 |
| --- | --- | --- |
| `active` | 文件在，受管键与批准一致 | 无需处理 |
| `changed` | 文件在，受管键变了 | 审阅后重新 `osdk trust` |
| `missing` | 文件没了，父目录还在 | `osdk trust prune` 可清理 |
| `unreachable` | 父目录也读不到 | 先确认卷是否挂载，**不会**被 prune |

`prune` 只删 `missing`。这一点是刻意的：`changed` 意味着项目还在、只是需要重新过目，
把它删掉会变成日后一句无从解释的「未受信任」；而 `unreachable` 在 Windows 上正是 U 盘、
网络共享或 WSL 挂载点掉线的样子，当垃圾清理会在卷恰好没挂载时删掉有效批准。
`--dry-run` 与实际执行用同一份候选列表，所以预览就是即将发生的事。

持久信任身份由配置的规范路径，与**上表所列受管键**的规范化 TOML 内容的 BLAKE3 共同
决定。哈希只覆盖受管键，因此两道门用的是同一个判据：一处不需要信任的改动，也不会
让已有记录失效。改工具版本、新增依赖、调 `jobs`、加注释、改空白或调整键顺序都不会
使记录变为 `changed`；改动受管键会，移动仓库则使其变为 `missing` 或 `unreachable`。
软链接解析到真实目标。trust store 位于 `$OSDK_CONFIG_DIR/trusted-configs.toml`。

CI 可设置 `OSDK_TRUSTED_CONFIG_PATHS`，值是操作系统路径分隔符连接的已审阅文件或
目录。匹配文件或位于匹配目录下的项目配置会在本次进程中视为 trusted，不写本地
trust store。`trust`/`untrust` 自身只加载用户配置，防止未信任项目影响自己的审批。

`osdk config set` 和 `osdk config unset` 同样在信任检查之前执行。否则出口会被它自己
要撤销的那份配置堵死——`unset` 将无法删掉正导致拒绝的那个键。这两个命令只针对指定
文件里的指定键，不会执行未受信任配置的任何内容。`config get` 和 `config list` 同样
不被拒绝：它们只展示那份配置的合并结果，不做任何事。
