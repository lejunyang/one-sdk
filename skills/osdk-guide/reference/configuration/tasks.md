# `[tasks]`

声明 `osdk run <name>` 执行的项目任务。任务默认是 phony；配置通过 trust 后才会执行。

## 任务正文四档

```toml
[tasks]
fmt = "cargo fmt --all"                         # 1. 单行 shell

[tasks.ci]
run = [                                         # 2. 多步骤
  "cargo fmt --all --check",
  { argv = ["cargo", "test", "--workspace"] },
  { cmd = "optional-check", ignore_error = true },
  { tasks = ["lint", "test"] },                # 并行子任务并等待
]

[tasks.release]
file = "scripts/release.ps1"                    # 3. 脚本文件

[tasks.generate-file]
file = "scripts/generate.lua"                   # 4. 文件 Lua（内嵌解释器）

[tasks.generate]
lua = """                                       # 4. 内嵌 Lua
mkdir(join(root, "dist"))
write(join(root, "dist", "version.txt"), "1.0\n")
"""
```

`run`、`file`、`lua` 互斥；`run_windows` 只替换 `run`。完整字段：

| 字段 | 语义 |
| --- | --- |
| `run` | 字符串或步骤数组；串行、失败即停 |
| `run_windows` | Windows 专属的 `run` 替换 |
| `file` | 脚本路径，相对声明它的配置文件；`.lua` 使用内嵌 Lua 运行时 |
| `lua` | 内嵌 Lua 5.4 |
| `run_post` | 正文开始执行后总会跑的收尾步骤，包括正文失败 |
| `description` | `task list` 展示文本 |
| `alias` | 任务别名数组 |
| `depends` | 前置任务，会加入执行图 |
| `wait_for` | 只排序已在执行图里的任务，不主动加入 |
| `env` | 子进程与 Lua host API 的任务环境 |
| `dir` | 工作目录，相对声明配置 |
| `shell` | shell 覆盖，如 `pwsh -Command` |
| `hide` / `quiet` | 隐藏列表项 / 抑制 osdk 进度 |
| `when` | 平台过滤 |
| `timeout` | `30s` / `5m` / `1h` / 秒数；超时终止整棵子进程树 |
| `sources` / `outputs` | 新鲜度输入/输出 glob |
| `freshness` | `mtime`（默认）/ `hash` / `always` |
| `args` / `options` / `flags` | 声明位置参数、命名选项和布尔 flag |

`argv` 步骤里的 `{{name}}` 与 `{{args}}` 按 argv 边界展开；不要把不可信参数插进 shell
字符串。Lua 完整 API、短名、exec capture、fs/path/JSON/TOML 见
`reference/run/lua.md`。

## 参数声明

```toml
[tasks.deploy]
run = [{ argv = ["deploy", "--env", "{{environment}}", "{{args}}"] }]

[[tasks.deploy.args]]
name = "environment"
help = "Target environment"
choices = ["staging", "production"]
default = "staging"               # 有 default 才是可选位置参数

[tasks.deploy.options.replicas]
help = "Replica count"
choices = ["1", "2", "3"]
default = "1"

[tasks.deploy.flags.wait]
help = "Wait for rollout"
```

位置参数按数组顺序解析；可选位置参数后不能再出现必填位置参数。未知 `--flag` 会进入
剩余参数，`--` 会停止 option 解析。shell 字符串不做 `{{name}}` 插值；值仅在 `argv`
步骤中安全替换，或作为 `osdk_arg_<name>` / `osdk_args` 环境变量传入。

## 文件任务

默认扫描配置根下的 `osdk-tasks/` 与 `.osdk-tasks/`：

```text
osdk-tasks/build.ps1         -> build
osdk-tasks/test/units.ps1    -> test:units
osdk-tasks/test/_default.ps1 -> test
```

文件名去扩展名成为任务名，目录用 `:` 分隔，`_default` 代表目录本身。`.lua` 在所有平台
都由 osdk 内嵌解释器执行，不需要系统 Lua、shebang 或执行位；Windows 选择同词干的
`.ps1` / `.bat` / `.cmd` 等可执行变体；Unix 可用 shebang 文件。文件头支持：

```text
#OSDK description="Build artifacts"
#OSDK depends=fetch,lint
```

解析在第一行非注释内容处停止。用 `task.includes` 替换默认搜索目录；也可在
任务对象中用 `file = "scripts/release.ps1"` 显式指向脚本。
