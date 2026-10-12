# 下载源与镜像

osdk 的 SDK/模型下载源与项目依赖 Registry 是两套控制面。本页说明前者：source 命令、环境变量里的镜像、有效来源列表，以及探测、择优与故障转移。npm 兼容 Registry 见[包管理器](../tools/package-managers#registry-预检)。

## Source 命令参考

```text
osdk source list TOOL_OR_PROVIDER
osdk source test TOOL
osdk source test huggingface|modelscope|civitai --model REFERENCE

osdk source add TOOL_OR_PROVIDER
  --id ID
  --download-url URL
  [--index-url URL]
  [--forward-credentials]

osdk source remove TOOL_OR_PROVIDER ID
osdk source pin TOOL_OR_PROVIDER ID
osdk source unpin TOOL_OR_PROVIDER
```

| 命令/参数 | 作用 |
| --- | --- |
| `list TOOL_OR_PROVIDER` | 列出有效来源、类型、URL 和 pin |
| `test TOOL` | 强制重新探测普通 backend 并输出吞吐/TTFB 排名 |
| `test PROVIDER --model ...` | 对 Hugging Face/ModelScope 仓库或精确 Civitai LoRA 版本的 metadata 与文件采样测速 |
| `add ... --id ID --download-url URL` | 添加或替换同 ID 的用户级 custom source |
| `--index-url URL` | metadata/index 与下载根不同时单独指定 |
| `--forward-credentials` | 允许自定义模型 endpoint 接收 provider 凭据 |
| `remove TOOL_OR_PROVIDER ID` | 删除用户级 custom source |
| `pin TOOL_OR_PROVIDER ID` / `unpin TOOL_OR_PROVIDER` | 在用户配置中设置/移除 source pin |

`add`、`remove`、`pin`、`unpin` 都编辑用户 `config.toml`。`--source ID` 是一次性
优先来源并保留其他来源作为回退；同一调用里的工具请求必须使用规范 backend ID，
例如写 `node` 而不是 `nodejs`，否则当前实现不会应用覆盖。`--refresh-sources` 对
`install`、`use`、`upgrade`、`exec` 强制重新探测；对 `model sync`，仅在没有显式
endpoint 或 pin、选择策略为 `auto` 且非 offline 时刷新。当前对 `lock`、`outdated`、
`list-remote` 不生效。
模型 `source test` 缺少 `--model` 会失败，普通工具使用 `--model` 也会失败。

## 环境变量里的镜像

各工具链本身都支持用环境变量指定镜像：rustup 读 `RUSTUP_DIST_SERVER`
（以及 `RUSTUP_UPDATE_ROOT`），go 命令读 `GOPROXY`，npm 系读
`npm_config_registry`、`pnpm_config_registry`、`YARN_REGISTRY`、
`YARN_NPM_REGISTRY_SERVER`、`BUN_CONFIG_REGISTRY` 等。

默认（`mode = "auto"`）下 osdk 会先校验这些值，再把它们**与内置镜像一起**参与
探测并按实测速度择优，而不是无条件采用：

- 校验不通过（不是合法 URL、不是 https、带凭据、带 query 或 fragment）时，
  osdk 打印一条 warning 并忽略该值，继续用内置镜像，而不是静默丢弃；
- 校验通过则作为 id 为 `env` 的候选加入探测，可在 `osdk source list <工具>`
  中看到；若它与某个内置镜像地址相同，则不会重复出现；
- `GOPROXY` 的 `off`、`direct` 以及逗号/竖线分隔的回退列表是合法的 go 设置，
  但不是单一可探测的镜像，因此会被跳过并给出说明。

需要无条件遵循环境变量时（例如公司内网镜像即使较慢也必须使用）用
`--source-mode env`，此时缺失或不合法都会**报错**而不是回退，避免配置错误被
静默忽略。

优先级：显式选择始终高于环境变量。`osdk source pin` 与一次性的 `--source ID`
仍然优先，环境变量只在没有显式选择时参与竞争。

## 有效来源列表

```toml
[sources]
selection = "auto"       # auto|pinned|ordered
mode = "auto"            # auto|env，见下文「环境变量里的镜像」
probe_timeout_ms = 1500        # SDK / 工具来源探测
model_probe_timeout_ms = 8000  # 模型 metadata / 响应头 / 样本各阶段预算
cache_ttl = "6h"

[sources.node]
pin = "corp"
disable = ["tuna"]

[[sources.node.custom]]
id = "corp"
kind = "custom"          # official|mirror|custom
download_url = "https://mirror.example/node/"
index_url = "https://mirror.example/node/index.json"
headers = [["X-Example", "value"]]
forward_credentials = false
priority = 0
enabled = true
```

有效列表等于“内置来源减去 `disable`，再加 `custom`”。custom 的同名 ID 覆盖内置
来源，`enabled=false` 被过滤，较小 `priority` 排在前面。Civitai 内置 `official`（`civitai.com`）与
`official-red`（`civitai.red`）两个官方 source；可用 `source pin` 固定其中一个，auto 模式则按精确版本的实际可达性与速度选择。项目中的 source 设置需要
[显式信任](../config/trust)。

`headers` 是显式 source 配置，与 `forward_credentials` 不同。osdk 自己发起的 metadata
请求和 source probe 只在初始 URL 与该 source 的 index/download URL 同 origin 时附加
这些 header；同源 redirect 保留，第一次跨源 redirect 后永久移除，header 值也不会
明文写入 cache。受管的 npm/pnpm 子进程只会收到 registry 覆盖以及一份 osdk 自有的空配置，
因此 `npm:<package>` 的实际 package fetch 不转发这里的 header。项目包管理器调用可以使用原生
可信配置；全局 npm 工具为了隔离 prefix，会拒绝认证或私有原生配置透传，全局安装请使用
可匿名访问的已配置 Registry。

## 选择、探测与故障转移

| `selection` | 行为 |
| --- | --- |
| `auto` | 读取 TTL 内测速缓存，否则并发探测；以吞吐量为主、TTFB 为惩罚排序 |
| `ordered` | 直接按 `priority` 顺序 |
| `pinned` | 没有具体 `sources.<tool>.pin` 时与 `ordered` 相同 |

具体 pin 会把该来源移到第一位，但其余来源仍保留为失败回退，并非“只允许这一源”。
SDK / 工具来源探测默认预算为 1500 ms。模型探测改用独立的
`model_probe_timeout_ms = 8000`，metadata、响应头、64 KiB 样本读取三个阶段分别
拥有这份预算，并优先选择仓库中最小的非空文件。只要成功拿到响应，就认定来源可达；
样本 body 超时只会让吞吐未知，不再误报 `unreachable`。全部模型来源都失败的结果
不会按常规 6h TTL 缓存，因此短暂网络拥塞不会把所有来源长期钉死。非法 TTL 当前
仍静默回退为 6h。

由 osdk 自身执行的下载走共享流式管线，并非一次请求失败就停止。普通归档保持最多
3 次（等待 400 ms、800 ms）；模型文件默认尝试 6 次，按 1/2/4/8/8 秒指数退避，并
输出可见警告。可通过 `osdk config set` 调整 `sources.model_download_attempts` 与
`sources.model_download_retry_base_ms`；`sources.model_read_timeout_ms`（默认 60000）
限定「无字节进展」时长，连接中途断流超过此值即让该次请求失败并进入上述重试，而不是永久挂起。
管线保留 `.partial` 文件及 ETag /
Last-Modified，重试时用 `Range` + `If-Range` 续传；服务端忽略或返回错误 Range、对象
变化、来源 URL 改变时会安全重头下载。source probe 只负责排序，并不证明某个精确版本
存在；HTTP 200 也可能是镜像错误页。因此 osdk 直接处理的归档、裸二进制和自升级产物会
把下载、checksum/attestation 校验、解包及必需文件检查视为同一个候选尝试，任一步失败
都会清理该候选并继续下一个排序来源；receipt 只记录真正通过完整验证的 URL。模型在某个
来源耗尽尝试后也会继续剩余排序来源；所有来源都耗尽后才终止。

委托型安装器不能统一用“命令失败就重跑”处理，因为安装脚本可能已经产生副作用。osdk
改在安全边界内做目标级验证：Rust 对每个 source 重跑完整 rustup 下载命令；Cargo 在锁定
sparse index 前检查该 crate 精确版本的 `config.json` 下载端点；`pypi:` 用实际项目页而非
固定 `pip` 页探测，并把选中 index 作为 uv/pip 的默认 index；`go:` 把排序候选以 `|`
组成一次原生 `GOPROXY` 回退链。普通项目里的 npm/uv/pip 等命令仍只执行一次，不会在
可能已运行 lifecycle script 后盲目重放。HTTP metadata 在线请求失败后可使用 stale 缓存；
严格 offline 只读已有缓存，缓存产物仍会重新校验和解包。
