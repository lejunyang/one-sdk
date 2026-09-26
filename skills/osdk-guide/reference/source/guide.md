# `osdk source`

管理工具、模型 provider 和 osdk 自身的下载源。

| 子命令 | 参数 | 语义 |
| --- | --- | --- |
| `list <TOOL>` | 工具或特殊源名 | 列出候选及最近测速 |
| `test <TOOL>` | `--model <REPO>` 可选 | 立即探测并按速度展示；模型源需目标仓库 |
| `add <TOOL>` | `--id`、`--download-url` 必填；`--index-url`、`--forward-credentials` 可选 | 新增自定义源 |
| `remove <TOOL> <ID>` | — | 删除自定义源 |
| `pin <TOOL> <ID>` | — | 设为优先尝试源，不禁止回退 |
| `unpin <TOOL>` | — | 取消固定 |

```bash
osdk source list node
osdk source test huggingface --model Qwen/Qwen2.5-7B-Instruct
osdk source add cargo:ripgrep --id corp \
  --download-url https://mirror.example/index/ \
  --index-url sparse+https://mirror.example/index/
osdk source pin node tuna
osdk source unpin node
```

特殊源名：`self`（自升级）和 `go-modules`（GOPROXY）。只有明确设置
`--forward-credentials` 才向自定义端点发送 provider 凭据。持久格式见
`reference/configuration/sources.md`；项目里的来源改写需要 trust。
