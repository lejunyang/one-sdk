# `osdk skills`

安装、内容寻址保存并链接 `SKILL.md` 包给外部 AI Agent。osdk 不执行 skill 中的脚本。

| 子命令 | 主要参数 | 语义 |
| --- | --- | --- |
| `add <SOURCE>` | `-s/--skill`、`-a/--agent` 可重复；`-g`、`--copy`、`--ref`、`-l/--list`、`--no-lock` | 列出或安装来源中的 skill；交互终端缺少默认 Agent 时多选 Agent，并选择链接或复制 |
| `list`（`ls`） | `-g` | 从项目或用户 lock 列已安装 skill 与 Agent |
| `remove <NAME>`（`rm`） | `-g`、`-a/--agent` 可重复 | 摘除全部或指定 Agent 链接 |
| `sync` | `-g` | 按 lock 的 commit 与内容哈希复现 |
| `update [SKILL...]` | `-g` | 重解析分支/标签或默认分支并更新 lock；仅显式 `rev:<commit>` 固定不动，旧版写入的裸 40 位 commit 视为安装快照 |
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

来源支持 `github:owner/repo`、`owner/repo`、GitHub URL 和本地路径。默认链接，`--copy`
改为复制；拒绝覆盖非 osdk 放置的真实目录。配置见
`reference/configuration/skills.md`。
