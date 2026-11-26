# `osdk skills`

安装、内容寻址保存并链接 `SKILL.md` 包给外部 AI Agent。osdk 不执行 skill 中的脚本。

| 子命令 | 主要参数 | 语义 |
| --- | --- | --- |
| `add <SOURCE>` | `-s/--skill`、`-a/--agent` 可重复；`-g`、`--copy`、`--ref`、`-l/--list`、`--no-lock` | 列出或安装来源中的 skill；无显式/默认 Agent 时先检测、再按需多选；仅多目标目录时选择链接或复制；摘要确认后写盘，`--yes` 跳过交互 |
| `list`（`ls`） | `-g` | 从项目或用户 lock 列已安装 skill 与 Agent |
| `remove <NAME>`（`rm`） | `-g`、`-a/--agent` 可重复 | 摘除全部或指定 Agent 链接 |
| `sync` | `-g` | 精确复现已锁定的 skill，按 `osdk.toml` 安装适用且缺锁的声明并写锁 |
| `update [SKILL...]` | `-g` | 重解析 `requested_ref`；仅目标 skill 内容哈希变化时重装，仓库其他变更只推进 lock；仅显式 `rev:<commit>` 固定不动，旧裸 commit 迁移为默认分支意图；同时让 Agent 跟随声明的 `agents`（删掉的取消链接、新增的装入、改写 lock） |
| `path <NAME>` | — | 打印内容寻址落地目录 |
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

来源支持 `github:owner/repo`、`owner/repo`、GitHub URL 和本地路径。缺锁声明在打印摘要后
安装，默认单目录复制、多目录自动链接。用 `update` 应用 ref 或 Agent 列表变更。换来源或
技能时，移除锁条目再同步，或显式调用 `add`。配置见
`reference/configuration/skills.md`。

GitHub tarball 必须成功下载并安全解包后才算候选成功；HTTP 200 错页或损坏归档会被清理，
随后尝试下一个 GitHub 下载入口。
