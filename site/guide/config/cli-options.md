# 命令行全局参数

以下参数对所有子命令生效，写在子命令之前或之后均可。多数参数还有对应的 `OSDK_*` 环境变量，完整清单见[环境变量](./environment)。

```text
osdk [GLOBAL OPTIONS] <COMMAND> [COMMAND OPTIONS]
```

| 参数 | 环境变量 | 作用 |
| --- | --- | --- |
| `-v`, `--verbose` | `OSDK_LOG` 可另行设置过滤器 | 可重复；`-v`、`-vv`、`-vvv` 分别提高到 info、debug、trace |
| `-q`, `--quiet` | — | 关闭下载/安装进度；不关闭普通结果，也不代表确认删除 |
| `-j N`, `--jobs N` | `OSDK_JOBS` | 最大并发下载/安装数；CLI 的 `0` 不覆盖配置，执行时至少为 1 |
| `--model-jobs N` | `OSDK_MODEL_JOBS` | `model sync` 同时下载的模型数，默认 2；与控制单个模型内文件并发的 `--jobs` 相互独立且相乘 |
| `-y`, `--yes` | `OSDK_YES` | 自动确认卸载、清缓存和实际 GC 等操作 |
| `--source ID` | — | 本次调用把 `ID` 移到来源候选首位并保留回退；工具请求须使用规范 backend ID（如 `node`，不能用 `nodejs`） |
| `--refresh-sources` | — | 为 `install`、`use`、`upgrade`、`exec` 强制重新探测；`model sync` 仅在无显式 endpoint/pin、选择策略为 `auto` 且非 offline 时刷新；当前不影响 `lock`、`outdated`、`list-remote` |
| `--source-mode MODE` | `OSDK_SOURCE_MODE` | `auto`（默认）校验环境变量里的镜像并与内置镜像一同参与探测择优；`env` 只用环境变量指定的镜像，缺失或不合法时报错 |
| `--offline` | `OSDK_OFFLINE` | 禁止网络，只使用缓存的 metadata 与 artifact |
| `--require-checksums` | `OSDK_REQUIRE_CHECKSUMS` | 没有普通 checksum 或可信 attestation 摘要时拒绝 artifact |
| `--attestations POLICY` | `OSDK_ATTESTATIONS` | `off`、`if-available` 或 `required` |
| `--prerelease POLICY` | `OSDK_PRERELEASE` | `never`、`if-explicit` 或 `allow` |
| `--lang LANG` | `OSDK_LANG` | `en` 或 `zh`；同时影响帮助和参数错误 |
| `-h`, `--help` | — | 显示帮助 |
| `-V`, `--version` | — | 显示版本 |

CLI 布尔开关用于开启本次行为，不能用同一个开关把配置中的 `true` 关回 `false`。
例如关闭签名验证只能通过 `OSDK_VERIFY_SIGNATURES=false` 或配置文件完成。

`--download-cache-max-size BYTES|2GiB|unlimited`（环境变量 `OSDK_DOWNLOAD_CACHE_MAX_SIZE`）仅覆盖本次下载副本保留容量；`0` 成功即删，不限制可下载文件大小。配置见[缓存与清理](../maintenance/cache)。
