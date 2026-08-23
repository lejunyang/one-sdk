# 可复现锁文件

除 Rust 浮动 channel 外，`osdk.lock` 保存解析后的精确版本和已选择 artifact，使
同一平台可以不重新查询上游版本目录而重建环境。Rust 的 `stable`、`beta`、
`nightly` 等 lock 只保存 rustup channel 名，未来重装可能得到更新 toolchain；需要
不可变重建时请使用明确版本或带日期的 Rust toolchain。Lock 是可复现输入和审计
记录，不是跳过校验的信任凭据。

## 命令与 lock 的交互

```text
osdk lock [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk install|i [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk outdated [TOOL[@VERSION] ...]
osdk upgrade [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk exec (-t|--tool TOOL[@VERSION])... -- COMMAND [ARG ...]
osdk model pull NAME REFERENCE [OPTIONS]
```

| 调用 | 是否读取现有 lock | 是否写 lock |
| --- | --- | --- |
| 无工具且无 `-o` 的 `install` | 是；有当前 host 平台区段就使用其中全部工具 | 否 |
| `install TOOL...` | 否 | 否 |
| `install -o KEY=VALUE`，即使没写工具 | 否 | 否 |
| `lock` | 仅为保留其他平台/模型而载入旧文件 | 是，重建目标平台的完整工具表 |
| `outdated` | 否；重新解析配置或显式请求 | 否 |
| `upgrade` | 否；重新解析配置或显式请求 | 是，重建 host 平台工具表 |
| `exec` | 否 | 否 |
| `model pull` | 不以模型 lock 为输入 | 默认合并 `[models]`；`--no-lock` 禁止 |
| `list`、`current`、`where` | 否 | 否 |

`outdated` 的“当前”列是该 backend 所有已安装版本中的最大值；它检查重新解析出的
精确目标是否已安装，并不表示目录当前激活版本。`upgrade` 安装新的解析结果后刷新 lock。

## 建议工作流

```bash
# 根据项目声明解析，不安装
osdk lock

# 使用当前平台 lock 中的解析结果和 artifact
osdk install

# 查看当前声明重新解析后是否有尚未安装的目标
osdk outdated

# 安装重新解析的目标并刷新 lock
osdk upgrade
```

显式 `osdk install node@20` 始终服从显式请求而不读 lock。要暂时给无参数安装增加
backend 选项，也会绕过 lock；建议先用相同选项重新 `lock`。

## 查找和写入位置

- 读取时，从当前目录向祖先查找最近的 `osdk.lock`。
- 写入时，如果已发现项目配置，则写到该配置同目录。
- 没有项目配置时，复用最近祖先的 lock；仍没有才在当前目录创建。

在特殊嵌套布局中，最近可读 lock 与项目配置决定的写入位置可能不同。建议把
`osdk.toml` 与 `osdk.lock` 放在同一项目根目录。

## Schema 1

```toml
schema = 1

[platforms.linux-x64.tools.node]
request = "20"
version = "20.20.0"

[platforms.linux-x64.tools.node.options]
arch = "x64"
corepack = "false"

[platforms.linux-x64.tools.node.artifact]
url = "https://example/node.tar.gz"
file_name = "node.tar.gz"
checksum = "sha256:..."       # 可省略
subdir = "dist"               # 可省略

[[platforms.linux-x64.tools.node.artifact.evidence]]
# 已验证供应链证据；具体字段由证据类型决定

[models.qwen]
provider = "huggingface"
repository = "Qwen/Qwen2.5-7B-Instruct"
requested_revision = "main"
revision = "immutable-revision"
endpoint = "https://huggingface.co"
variant = "safetensors-fp16" # 可省略

[[models.qwen.files]]
path = "config.json"
size = 123
sha256 = "..."
```

平台键为 `linux-*`、`macos-*`、`windows-*`，架构为 `x64|arm64|x86|arm`；
musl Linux 追加 `-musl`。更新一个平台会保留其他平台和顶层模型记录。内部
`__osdk_*` 选项不写入公开 `options`；artifact 身份单独保存。

Node 的 `lock -o arch=...` 会写入目标架构区段；osdk 没有跨架构“只下载”模式，
随后在不匹配 host 上安装会拒绝。当前 `upgrade -o arch=...` 始终写 host 平台区段，
不要用它生成跨架构 lock。

## 陈旧的项目 lock

osdk 当前不比较 `osdk.toml` 与 `osdk.lock` 的修改时间或内容。只要最近 lock 存在
当前平台区段，无参数 `install` 就完全使用该区段，即使项目配置已改变；区段存在但
工具表为空时也不会回退配置。当前平台区段不存在时才回退到配置发现。

因此，修改项目版本后应显式运行：

```bash
osdk lock       # 只刷新精确解析
# 或
osdk upgrade    # 安装重新解析的版本并刷新 lock
```

损坏的 TOML 或非 schema 1 lock 会直接报错，不会静默回退配置。

## 锁定 artifact 的重装校验边界

lock 可记录实际 URL、文件名、checksum、archive 子目录和 attestation evidence。
无参数安装会把版本改成 lock 中保存的解析结果，并恢复 backend 选项与 artifact 身份。
对 Rust 浮动 channel，这个结果仍是 channel 名而非不可变版本。

当安装目录缺失或不完整、pipeline 实际执行重装时，lock 中存在的 checksum 会对下载或
缓存字节重新计算并比较；启用 attestation 时，lock evidence 只是审计记录，仍需缓存或
在线取得的证明 bundle。若 lock 没有 digest/evidence 且 `require_checksums=false`，安装仍
可能在没有加密完整性校验的情况下继续。普通 `install` 遇到已有 `.osdk-complete` 的版本
会更早复用该安装，只运行 backend 的 post-install 检查，不重新 hash artifact。详情见
[下载源与供应链安全](./sources-security#完整性签名与-attestation)。

本地链接的 Rust toolchain 无法作为可复现 artifact，`osdk lock` 会明确拒绝。

## 不要混淆三种“陈旧”

| 状态 | 当前处理方式 |
| --- | --- |
| 项目 `osdk.lock` 内容落后 | 没有自动 freshness 检测；运行 `lock` 或 `upgrade` 刷新 |
| `<installs>/<tool>/.locks/<version>.lock` 文件残留 | 这是 OS 级排他锁的路径；进程结束会释放锁，空文件存在不表示仍被占用，不按时间删除 |
| 安装目录没有 `.osdk-complete` | 视为上次失败的部分安装；拿到对象锁后删除并重新构建 |

模型快照也使用按 snapshot 区分的 OS 排他锁。`trust list` 所显示的 `stale` 是
配置路径或内容不再匹配，与以上 lock 状态无关。
