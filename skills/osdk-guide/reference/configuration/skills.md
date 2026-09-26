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

`source` 必填，可为 GitHub 来源或本地路径。GitHub 浮动 ref 在 lock 中解析为 commit 并记录
内容哈希；`sync` 复现，`update` 才重新解析浮动 ref。`agents` 为空时回退
`default_agents`。未知字段硬报错。
