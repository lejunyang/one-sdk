# `osdk self`

管理 osdk 自身安装。

```bash
osdk self upgrade [--version VERSION] [--dry-run] [--force]
```

| 参数 | 语义 |
| --- | --- |
| `--version <VERSION>` | 安装指定 release；省略取最新 |
| `--dry-run` | 只报告可用版本，不下载替换 |
| `--force` | 当前已是目标版本也重装 |

自身升级的下载由旧进程执行，下载器修复在更新后的下一次运行才生效；
例如 0.0.10 的空进度条问题已在 0.0.12 修复，旧版升级时仍可能显示它。
`--quiet` 隐藏下载进度，不隐藏升级结果。

升级同时替换 `osdk` 与 `osdk-shim`。Linux musl 安装继续选择 musl 产物，不切换到
glibc。下载源走特殊 key `self`：

```bash
osdk source test self
osdk source pin self <SOURCE_ID>
osdk self upgrade --dry-run
```

全局 `--offline`、`--source`、checksum 与 attestation 策略均适用。持久源配置见
`reference/configuration/sources.md`。
