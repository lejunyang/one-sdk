# `osdk skills`

安装、内容寻址保存并链接 `SKILL.md` 包给外部 AI Agent。osdk 不执行 skill 中的脚本。

| 子命令 | 主要参数 | 语义 |
| --- | --- | --- |
| `add <SOURCE>` | `-s/--skill`、`-a/--agent` 可重复；`-g`、`--copy`、`--ref`、`-l/--list`、`--no-lock` | 列出或安装来源中的 skill；无显式/默认 Agent 时先检测、再按需多选；仅多目标目录时选择链接或复制；摘要确认后写盘，`--yes` 跳过交互 |
| `list`（`ls`） | `-g` | 列出锁里的 skill、Agent 和项目中未锁定的 skill 路径 |
| `remove <NAME>`（`rm`） | `-g`、`-a/--agent` 可重复 | 摘除全部或指定 Agent 链接 |
| `sync` | `-g` | 按锁复现当前平台适用的 skill，安装适用且缺锁的声明并写锁。保留跳过的锁条目，替换任何已有但未锁定的 skill 目录前确认 |
| `update [SKILL...]` | `-g` | 重解析 `requested_ref` 或应用声明的新 ref，包括新的 `rev:<commit>`。仅目标 skill 内容哈希变化时重装，仓库其他变更只推进 lock。未变更的 `rev:<commit>` 保持固定，旧裸 commit 迁移为默认分支意图。同时让 Agent 跟随声明的 `agents`（删掉的取消链接、新增的装入、改写 lock） |
| `path <NAME>` | — | 按配置的范围查锁并打印落地目录。未配置时先查项目锁，再查用户锁 |
| `find [QUERY...]`（`search`） | `--owner`、`--limit 1..50` | 匿名搜索 GitHub；限流后才用 token，不访问 skills.sh |
| `use <SOURCE>` | `-s/--skill`、`-a/--agent`、`--ref` | 不安装；打印 prompt 或交互启动 Agent |
| `init [NAME]` | — | 创建不覆盖已有 `SKILL.md` 的模板 |
| `agents` | — | 列支持的 Agent 及 project/global 目录 |

```bash
osdk skills agents
osdk skills find --owner vercel-labs
osdk skills add github:vercel-labs/agent-skills --list
osdk skills add owner/repo -s name -a claude-code
osdk skills sync
osdk skills update name
```

配置 `[skills].scope = "global"` 可默认使用用户级 Agent 目录和用户锁，`-g` 可显式选择该范围。
来源支持 `github:owner/repo`、`owner/repo`、GitHub URL 和本地路径。缺锁声明在打印摘要后
安装，默认单目录复制、多目录自动链接。用 `update` 应用 ref 或 Agent 列表变更。换来源或
技能时，移除锁条目再同步，或显式调用 `add`。配置见
`reference/configuration/skills.md`。

`CI` 为非空值时按 CI 运行，`false`（不区分大小写）和 `0` 除外。CI 或无终端时，
可以传 `--yes` 确认替换未锁定目录，也可以先移走旧目录再同步。
若缺锁声明需要人工选择 Agent，`sync` 会立即退出 1。配置声明的 `agents` 或
`[skills].default_agents`，也可以传 `--yes` 接受推荐目标。

子项目缺少同名锁条目和安装目录时，`list` 和 `remove` 会指出当前 Git 仓库内的候选上级锁条目。
先核对候选条目归哪个项目。子项目有同名的未锁定安装时，两条命令会报告本项目的 Agent 安装路径。
`remove` 需要当前项目的锁条目才能移除技能。

`update` 遵循声明中的 `when`。不点名时，说明平台要求并跳过被排除的技能，保留已有安装和
锁条目。点名更新被排除的技能会说明平台要求并退出 1。
它先为同一技能准备所有 Agent 目标。任何目标激活失败时，保留原安装内容和锁条目。

`--offline` 下，GitHub 技能按锁回放需要已落地的副本。需要下载归档时，即使提交元数据
已缓存，也会报离线缓存缺失。切换到另一个固定 commit 需要联网。

GitHub tarball 必须成功下载并安全解包后才算候选成功；HTTP 200 错页或损坏归档会被清理，
随后尝试下一个 GitHub 下载入口。
