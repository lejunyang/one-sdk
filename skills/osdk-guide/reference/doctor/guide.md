# `osdk doctor`

诊断目录、下载源、文件系统链接能力、shim、代理和已安装内容。

```bash
osdk doctor [--verify] [--tool TOOL]
```

| 参数 | 语义 |
| --- | --- |
| `--verify` | 重新哈希安装文件，检测工具自更新、手工修改或不完整恢复；可能较慢 |
| `--tool <TOOL>` | 只验证一个工具；要求同时给 `--verify` |

```bash
osdk doctor
osdk doctor --verify --tool node
```

若 verify 报告漂移，用 `osdk install <tool> --force` 重建，不要只 reshim。代理诊断会
对凭据脱敏。实际 link mode 来自 `[settings].link_mode` 与当前文件系统能力。
