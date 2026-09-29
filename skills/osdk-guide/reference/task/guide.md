# `osdk task`

查看和编辑项目任务定义，不直接替代 `osdk run`。

| 子命令 | 参数 | 语义 |
| --- | --- | --- |
| `list`（别名 `ls`） | `--hidden` | 列出任务；默认隐藏 `hide=true` |
| `info <TASK>` | 任务名/alias | 显示合并、过滤后的定义 |
| `deps <TASK>` | 任务名/alias | 打印依赖执行顺序 |
| `add <NAME>` | `-r/--run <CMD>` 必填可重复；`-d/--desc`；`--depends` 可重复 | 写入项目配置 |
| `rm <NAME>` | 任务名 | 从项目配置删除 |
| `edit <NAME>` | 任务名 | 用 `$EDITOR` 定位并打开配置 |

```bash
osdk task list --hidden
osdk task info ci
osdk task deps ci
osdk task add lint -r "cargo fmt --check" -r "cargo clippy" -d "Lint" --depends build
osdk task edit lint
osdk task rm lint
```

`task list/info/deps` 是只读查询；执行使用 `osdk run <name>`。配置字段见
`reference/configuration/tasks.md` 和 `reference/configuration/task.md`。
