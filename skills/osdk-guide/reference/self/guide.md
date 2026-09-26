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

升级同时替换 `osdk` 与 `osdk-shim`。Linux musl 安装继续选择 musl 产物，不切换到
glibc。下载源走特殊 key `self`：

```bash
osdk source test self
osdk source pin self <SOURCE_ID>
osdk self upgrade --dry-run
```

全局 `--offline`、`--source`、checksum 与 attestation 策略均适用。持久源配置见
`reference/configuration/sources.md`。
