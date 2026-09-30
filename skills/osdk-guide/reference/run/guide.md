# `osdk run`

执行 `[tasks]` 中的项目任务。

```bash
osdk run <TASK> [--dry-run] [--no-deps] [-- ARGS...]
```

| 参数 | 语义 |
| --- | --- |
| `<TASK>` | 名字、alias 或 monorepo 的 `//path:name` |
| `--dry-run` | 展示解析后的步骤与 freshness 判定，不执行 |
| `--no-deps` | 跳过应用依赖 `[deps]` 自动兑现；不跳过 task 的 `depends` 或 `tools` |
| `-- ARGS...` | 传给任务参数解析器；`--` 停止 osdk 自身选项解析 |

```bash
osdk run ci
osdk run ci --dry-run
osdk run test -- --nocapture
osdk run //packages/ui:build
```

只有 `osdk run <name>`，没有裸 `osdk <name>`。执行前会验证未知依赖和依赖环；
`depends` 按拓扑顺序运行且每项最多一次，`{ tasks = [...] }` 并行并等待全部结束。
`run_post` 只要正文开始过就执行，包括正文失败；依赖失败或 freshness 跳过时不执行。

任务的 `tools` 列表引用 `[tools]` 键。运行前 osdk 会检查整个任务图，只安装尚未就绪的
显式依赖，包括标记为 `lazy = true` 的工具；不会分析 shell 命令来猜依赖。未知或平台
不匹配的工具会在任何安装和任务命令开始前报错。

任务正文可用 shell、argv、脚本文件或 Lua。字段全集见
`reference/configuration/tasks.md`，runner 默认见
`reference/configuration/task.md`，完整 Lua 指引见本目录的 [lua.md](lua.md)。
