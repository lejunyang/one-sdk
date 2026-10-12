# 离线与预发布

本页说明如何只用本地缓存工作，以及 osdk 如何对待预发布版本。

## 离线模式

```bash
osdk --offline install bun@1.3.14
osdk --offline install                    # 可结合当前平台 lock
osdk --offline model sync qwen
```

`--offline` 或 `OSDK_OFFLINE=true` 严格禁止网络：

- metadata、SDK archive、模型 metadata 和所选文件必须已缓存；
- 自动 source probe 被跳过；`source test` 及 SDK 安装类命令中的 `--refresh-sources`
  会失败，`model sync` 不会刷新，本就不支持该参数的命令仍忽略它；
- 缺少缓存时明确报错，不会偷偷联网；
- 对支持通用 artifact receipt 的 backend，lock 中的 artifact URL/checksum 可支持离线
  重装；pipeline 实际重装且有 checksum 时重新校验字节，已有完整 GitHub 安装仅在 receipt
  的锁定文件名/checksum 与动态选项身份都匹配时复用；
- `npm:<package>` 不使用通用 artifact URL。schema 4 `osdk.lock` 延续 schema 3 的 npm
  metadata 模型，只保存 scope、installer 与可选原生 lock 身份，而不携带依赖图；仅靠这些
  metadata 不能冷恢复依赖图。已有完整
  安装也只在记录的选项匹配时复用；支持原生 lock 重放的操作还需要安装器拥有的 lock 与
  已预热 cache/store。旧 lock schema 2 graph sidecar 仅用于兼容读取；
- `cargo:` 工具只有在来源、selector、选项、平台和精确受管 Rust 身份都匹配时，才能
  复用已有完整安装。全新离线安装或修复不受支持，因为 Cargo 原生 lock 与 `osdk.lock`
  都不包含完整 source graph；
- `go:` 工具也只能复用身份精确匹配的完整安装。其 schema 4 lock 会记录选中的 Go
  proxy、发现的 module root、公开构建选项与精确受管 Go 身份，但不记录全新离线构建所需
  的传递 module graph；
- `attestations=required` 还要求按 artifact SHA-256 缓存的证明 bundle，lock evidence
  不能代替重新验证。

`OSDK_OFFLINE` 只控制 osdk 自己及其 hook 所管理的兼容环境；项目子进程是否完全
离线仍取决于下游工具的原生参数。

## 预发布版本

```bash
osdk install bun@canary
osdk install deno@beta
osdk install github:owner/repo@1.2.0-beta.1
osdk --prerelease allow install bun@latest
osdk --prerelease never install bun@canary
```

| 策略 | 行为 |
| --- | --- |
| `never` | 拒绝预发布，包括显式精确版本或通道 |
| `if-explicit` | 默认；仅显式预发布版本或 `canary|nightly|beta` 通道可选预发布 |
| `allow` | `latest`、前缀和 range 也可隐式选择预发布 |

该策略用于支持预发布感知的 Python、Bun、Deno 和 GitHub backend。
`list-remote` 当前仍只显示稳定版本。lock 会同时保存原始请求与精确解析版本。
Cargo Registry 解析使用自己的固定规则：始终排除 yanked release，`latest` 与数字前缀
只选择稳定版本，只有精确 Cargo selector 可以选择明确的未 yanked 预发布版本。
