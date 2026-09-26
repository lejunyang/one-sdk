# 全局参数

用法统一为：

```bash
osdk [全局参数] <一级命令> [命令参数]
```

全局参数可放在一级命令前；具体命令参数仍以对应 command reference 和
`osdk <command> --help` 为准。

| 参数 | 环境变量 | 语义 |
| --- | --- | --- |
| `-v, --verbose...` | — | 增加日志详细度，可重复 |
| `-q, --quiet` | — | 抑制 osdk 自己的进度输出，不吞子进程输出 |
| `-j, --jobs <N>` | `OSDK_JOBS` | 单个工具或模型内部的最大并发下载/安装数 |
| `--model-jobs <N>` | `OSDK_MODEL_JOBS` | `model sync` 同时处理的模型数，默认 2；与 `--jobs` 相乘 |
| `-y, --yes` | — | 对确认提示回答“是”；不会绕过要求精确指纹的安全门 |
| `--source <ID>` | — | 本次调用优先使用指定下载源 |
| `--refresh-sources` | — | 忽略源测速缓存并重新探测 |
| `--source-mode <auto\|env>` | `OSDK_SOURCE_MODE` | `auto` 排序环境镜像与内置源；`env` 原样遵循环境镜像 |
| `--offline` | `OSDK_OFFLINE` | 禁止网络，只用本地缓存与锁定信息 |
| `--require-checksums` | `OSDK_REQUIRE_CHECKSUMS` | 拒绝没有可验证 checksum 的产物 |
| `--attestations <off\|if-available\|required>` | `OSDK_ATTESTATIONS` | GitHub artifact attestation 策略 |
| `--prerelease <never\|if-explicit\|allow>` | `OSDK_PRERELEASE` | 预发布版本选择策略 |
| `--lang <en\|zh>` | `OSDK_LANG` | 覆盖本次输出语言 |

优先级为：CLI 参数 → 环境变量 → 项目配置 → 用户配置 → 内置默认。`--yes`
只确认普通交互；`pkg mirrors apply`、`container mirrors apply`、`container prune`
等绑定状态的操作仍要求命令自己的 plan/preview 指纹。
