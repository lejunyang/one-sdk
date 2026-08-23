# 模型快照

osdk 把 Hugging Face 与 ModelScope 仓库作为多文件、不可变快照管理。模型文件
进入与 SDK 共用的 BLAKE3 CAS，但解析、manifest、当前快照和环境适配均是独立流程。

## 命令参考

```text
osdk model pull NAME REFERENCE
  [--endpoint URL]
  [--forward-credentials]
  [--include GLOB]...
  [--exclude GLOB]...
  [--variant LABEL]
  [--no-lock]

osdk model list
osdk model path NAME
osdk model verify NAME
osdk model remove NAME

osdk model env enable [huggingface|modelscope] [--force]
osdk model env disable [huggingface|modelscope]
osdk model env list
```

| `pull` 参数 | 作用 |
| --- | --- |
| `NAME` | 本地逻辑名；只允许 ASCII 字母、数字、`.`、`_`、`-` |
| `REFERENCE` | `PROVIDER:owner/repo@revision` |
| `--endpoint URL` | 覆盖 provider endpoint；优先于环境变量和 source 选择 |
| `--forward-credentials` | 允许这个显式自定义 endpoint 接收 provider token |
| `--include GLOB` | 可重复；至少匹配一个 include 时才下载 |
| `--exclude GLOB` | 可重复；在 include 结果上继续排除 |
| `--variant LABEL` | 记录到快照 identity、manifest 和 lock 的标签；**不会自动筛文件** |
| `--no-lock` | 不更新最近项目位置的 `osdk.lock` |

其他命令没有可选参数。`list` 显示每个逻辑名的当前快照；`path` 输出当前路径；
`verify` 校验当前快照所有文件；`remove` 删除该逻辑名的全部快照并立即执行 CAS GC，
当前不会请求确认。

## Provider 引用

```text
hf:owner/repo@revision
huggingface:owner/repo@revision
hugging-face:owner/repo@revision

ms:owner/repo@revision
modelscope:owner/repo@revision
model-scope:owner/repo@revision
```

省略 revision 时，Hugging Face 默认 `main`，ModelScope 默认 `master`。repository
必须正好是 `owner/name` 两段，每段只允许 ASCII 字母、数字、`.`、`_`、`-`。

```bash
osdk model pull qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main
osdk model pull qwen25-ms ms:Qwen/Qwen2.5-7B-Instruct@master
osdk model pull qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main \
  --include '*.json' --include '*.safetensors' \
  --exclude 'original/*' --variant safetensors-fp16
```

Hugging Face 将 branch/tag 解析为不可变 commit SHA。ModelScope 文件 API 没有等价
commit 时，osdk 以请求 revision 和排序后的文件路径、大小、SHA-256 manifest 生成
`revision+manifest-<16 hex>` identity。远端文件路径必须是安全相对路径。

## 下载、校验与本地布局

文件按 `settings.jobs` 并发下载，支持 Range/ETag 续传。上游提供 SHA-256 时强制
校验；未提供时仍计算并记录本地 SHA-256。`model verify` 同时检查 CAS BLAKE3 与
manifest SHA-256。快照和 `current.json` 都通过同目录临时路径再 rename 发布；这不
保证 fsync 持久性、跨平台替换原子性或不同 snapshot writer 之间的事务隔离。

```text
<data>/models/<name>/
├── current.json
├── .locks/<snapshot>.lock
└── snapshots/<snapshot>/
    ├── .osdk-model.json
    ├── .osdk-manifest.json
    ├── .osdk-complete
    └── downloaded files...
```

`--offline` 下，metadata 与所有所选文件都必须已经缓存；满足时可重建已删除的物化
快照。自动 source failover 只发生在同一 provider 内，不会把 Hugging Face 仓库
隐式转换为 ModelScope 仓库。

## Lockfile

默认 `pull` 在 `osdk.lock` 顶层 `[models.<name>]` 记录：

- provider、repository、请求 revision 与不可变 revision；
- 实际 endpoint 与可选 variant；
- 每个文件的路径、大小与 SHA-256。

token、cookie、临时签名下载 URL 和 ETag 不写入项目 lock。模型更新只合并同名
模型项并保留平台工具区段。当前普通 `osdk install` 不从 `[models]` 自动拉取模型；
模型重建仍使用 `model pull`。完整 schema 见[可复现锁文件](./lockfiles)。

## Endpoint 与凭据

解析优先级：

```text
--endpoint
> HF_ENDPOINT / MODELSCOPE_ENDPOINT / MODELSCOPE_DOMAIN
> source pin、测速排名和内置 endpoint
```

Token 读取顺序：

| Provider | 环境变量，左侧优先 |
| --- | --- |
| Hugging Face | `OSDK_HF_TOKEN`、`HF_TOKEN`、`HUGGING_FACE_HUB_TOKEN` |
| ModelScope | `OSDK_MODELSCOPE_TOKEN`、`MODELSCOPE_API_TOKEN` |

官方端点 `https://huggingface.co`、`https://modelscope.cn`、
`https://www.modelscope.ai` 可接收对应凭据。自定义 source 或 `--endpoint` 默认匿名，
只有 `--forward-credentials` 或 source 的 `forward_credentials = true` 才转发。
ModelScope 会同时使用 Bearer header 与 `m_session_id` cookie。

模型 source 命令与 SDK 相同，但测试时必须指定仓库：

```text
osdk source list huggingface|modelscope
osdk source test huggingface|modelscope --model owner/repo[@revision]
osdk source add huggingface|modelscope --id ID --download-url URL
  [--index-url URL] [--forward-credentials]
osdk source remove huggingface|modelscope ID
osdk source pin huggingface|modelscope ID
osdk source unpin huggingface|modelscope
```

探测会先解析目标仓库 metadata，再对一个真实文件做最多 1 MiB 的 Range 下载；
匿名与带凭据模式使用不同缓存键。更多 source 规则见[下载源与供应链安全](./sources-security)。

## 全局模型环境

先把 activation 放入 shell 初始化文件，再启用 provider adapter：

```bash
eval "$(osdk activate bash)"
osdk model env enable                       # 两个 provider
osdk model env enable huggingface
osdk model env enable modelscope --force
osdk model env list
osdk model env disable huggingface
osdk model env disable                      # 两个 provider
```

`enable`/`disable` 只接受可选 provider；`--force` 只属于 `enable`，表示覆盖用户已有
provider 变量。开关写用户全局配置，项目配置不能改变 `env`/`env_force`。已激活 shell
在下一次 prompt 刷新，新 activation 立即应用；`deactivate` 会恢复捕获的原值。

Hugging Face adapter 导出：

```text
HF_ENDPOINT
HF_HOME=<cache>/pkg/models/huggingface
HF_HUB_CACHE=<...>/hub
HF_XET_CACHE=<...>/xet
HF_ASSETS_CACHE=<...>/assets
HF_HUB_OFFLINE=1                    # 仅 osdk offline 时
```

ModelScope adapter 导出：

```text
MODELSCOPE_ENDPOINT
MODELSCOPE_CACHE=<cache>/pkg/models/modelscope
```

ModelScope 客户端没有等价的全局 offline 变量，osdk 不会虚构
`MODELSCOPE_OFFLINE`。当 osdk 管理一个不允许转发凭据的自定义端点时，还会清空
相关 token、禁用 Hugging Face 隐式 token，并使用隔离的匿名 HOME，避免持久凭据泄露。
