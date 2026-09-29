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

probe 只对候选排序，不保证目标版本一定存在。osdk 直接下载归档、裸二进制或自升级
产物时，会对每个候选依次执行下载、checksum/attestation 校验、解包和必需文件检查；
任一步失败都会清理该候选并继续后备源。`--offline` 不会换源，但仍校验并解包缓存产物。
Zig 与 Gradle index 中的绝对产物 URL 会按发布相对路径重映射到排序后的 download URL；
Maven/Kotlin 也使用有效 source 列表，而不是固定只用内置首项。

委托型安装不会一概在命令失败后重跑：Cargo 会在选定 sparse index 前探测精确 crate
制品，`pypi:` 会探测实际项目页并把结果传给 uv/pip，`go:` 的全新解析会生成以 `|`
连接的原生 `GOPROXY` 回退链。普通项目依赖命令可能已经执行 lifecycle script，因此
仍只运行一次。

Node 会合并所有可达版本 index；经典 CPython 会合并同一 release tag 的
`SHA256SUMS`；Java 会逐个查询 Foojay-compatible source 及其 checksum detail。这样
“通用探测正常但目标版本 metadata 缺失”也会继续后备源。

`npm:<package>` 的未锁定安装和 `osdk lock` graph-only 阶段会逐 source 重建隔离项目；
后者固定禁用脚本，所以可安全回退。已有原生 lock 的 frozen 重放不会切换来源。
