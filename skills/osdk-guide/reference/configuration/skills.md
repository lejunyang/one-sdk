# `[skills]`

声明要安装给外部 AI Agent 的 skill。osdk 只下载、校验、内容寻址存储和链接，自己不执行
skill 内脚本。

```toml
[skills]
default_agents = ["claude-code", "codex"]
scope = "project"                    # project / global
link_mode = "symlink"                # 可选，覆盖 skill 的落地方式

[skills.web-design]
source = "github:vercel-labs/agent-skills"
skill = "web-design-guidelines"
ref = "branch:main"
agents = ["claude-code"]
when = { os = ["linux", "macos"] }
# endpoint = "https://..."           # 自定义来源，需要 trust
```

`source` 必填，可为 GitHub 来源或本地路径。GitHub ref 在 lock 中解析为 commit 并记录内容
哈希；`sync` 始终复现该不可变 lock，`update` 重新解析分支/标签或默认分支。只有显式
`ref = "rev:<commit>"` 才在 `update` 时永久固定；已有项目保存的裸 40 位 commit 视为一次
安装快照，更新时继续跟随默认分支。`agents` 为空时回退 `default_agents`；两者都未指定时
先检测已安装 Agent：一个结果自动选中，多个或零个结果在交互终端多选，`--yes` 接受检测结果
（零个时选择全部），其他非交互调用报错。`link_mode` 未配置且未传 `--copy` 时，仅多个实际目标
目录会选择链接或复制；单一目录直接复制。安装摘要确认后才写盘。未知字段硬报错。
