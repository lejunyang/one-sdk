# 容器运行时、Registry 与原生操作

osdk 可以在不修改配置或存储的前提下检查 Docker Engine、containerd 和
Docker Buildx；也可以匿名测试 OCI Registry 及其已配置 mirror，并生成只读原生 mirror
plan；还可以把镜像拉取交给一个选中的原生 runtime，并在批准前预览严格限定范围的原生
清理。检查与规划保持只读；pull 和经批准的 prune 只修改解析出的原生控制面。

## 诊断运行时与构建器

```bash
osdk container doctor
osdk container doctor --runtime docker
osdk container doctor --runtime containerd
osdk container doctor --runtime docker --builder team-builder
osdk container doctor --json
```

默认运行时来自生效的 `[containers].runtime` 配置。选择 `auto` 时，osdk 依次
探测 Docker 和 containerd，再按诊断状态选择：健康、降级、仅客户端、权限不足、
不可达、版本不受支持、未安装。状态相同时优先 Docker。选择依据是实际探测状态，
而不是仅检查二进制文件是否存在。

Buildx 会单独报告，因为构建器并不是选中的运行时。自动模式和显式 Docker 诊断会
检查配置或当前 Buildx 构建器。显式 containerd 诊断默认跳过 Buildx，只有提供
`--builder NAME` 时才检查。构建器检查从不使用 `--bootstrap`，因此不会启动已停止的
构建器。

人类可读输出首先给出选中的运行时和状态，随后展示同一次探测已取得的类型化事实。
Docker 报告版本、context 类型、daemon 平台、rootless/Desktop 状态和有序 mirror origin；
containerd 报告版本与 Registry 配置状态；Buildx 报告 driver，以及仅以序号标识的节点
状态、版本、脱敏 endpoint origin 和平台。

`--json` 输出确定的 schema version 2 对象，其中包含选中报告、自动选择期间尝试的所有
运行时报告，以及独立的可选构建器报告。它不包含 context、builder、节点名称、containerd
namespace 与配置路径，也不包含 endpoint path/query。字段名和枚举值不会随 `--lang` 翻译。

## 检查原生缓存用量

```bash
osdk container cache status
osdk container cache status --runtime docker
osdk container cache status --runtime buildkit
osdk container cache status --runtime buildkit --builder team-builder
osdk container cache status --runtime containerd --json
```

`docker` 使用 Docker 的聚合 `system df` 接口。`buildkit` 使用选中的 Buildx
构建器及其正式 JSON `du` 接口，因此要求 Buildx 0.28 或更高版本。`containerd`
返回结构化的 `unsupported` 结果：containerd 没有与上述接口等价的单一稳定聚合
缓存状态契约。osdk 不会通过扫描 containerd 的私有 content、snapshot 或 metadata
存储来模拟一个结果。
`--builder NAME` 与 doctor 一样会覆盖缓存状态命令的配置构建器；它只影响 BuildKit
查询，对其他运行时会被忽略。

`auto` 使用与 `doctor` 相同的运行时状态选择。如果选中 Docker，则查询 Docker
Engine；如果选中 containerd，则返回其明确的“不支持”结果。要专门查看构建缓存，
请使用 `--runtime buildkit`。

缓存 JSON 使用原生缓存 schema version 1，只包含类型化分类、数量、字节总量、可回收
字节数、状态、所有者和已脱敏的命令证据。原生对象 ID、描述、构建器名称、命令原始
输出、凭据和私有路径都不会进入输出。

## 使用选中的原生 runtime 拉取镜像

```text
osdk container pull IMAGE
  [--runtime auto|docker|containerd]
  [--platform OS/ARCH[/VARIANT]]
  [--address ADDRESS --namespace NAMESPACE]
```

```bash
# 使用生效的容器 runtime 和 platform。
osdk container pull ubuntu:24.04

# 同时固定所有者与请求的镜像平台。
osdk container pull ghcr.io/example/tool:1.0 \
  --runtime containerd --platform linux/amd64 \
  --address unix:///run/containerd/containerd.sock --namespace default
```

Runtime 和 platform 默认来自生效的 `[containers]` 配置。使用 `--runtime auto` 时，osdk
对 Docker 和 containerd 执行一次有界只读解析并确定地选中一个所有者，然后只启动一次
原生前台 pull。原生命令一旦启动便不会回退到另一个 runtime，因此认证、网络或拉取失败
都会由实际运行的所有者报告。

`--offline` 会在解析 runtime 或启动原生命令前拒绝 pull。

显式选择 `--runtime containerd` 时必须同时提供 `--address` 与 `--namespace`；这两个 selector
始终要求成对出现。使用 `--runtime auto` 时，只有 containerd 胜出才要求二者，Docker 无需
它们即可继续。

原生子进程继承 stdio，osdk 会等待它结束。Docker 收到直接的 `docker image pull`；
containerd 收到直接的 `ctr --address ADDRESS --namespace NAMESPACE images pull`。osdk 返回
子进程的直接退出码；Unix 上若子进程由信号终止，则规范化为 `128 + signal`。osdk 不会自行
下载镜像 layer、创建 OCI content store，也不会在 runtime 之间复制镜像。

## 预览并执行限定范围的原生清理

```text
osdk container prune
  --runtime docker|buildkit|containerd
  --scope images|build-cache
  [--context NAME]
  [--builder NAME]
  [--execute]
  [--accept-preview SHA256_ID]
```

只支持两个 runtime/scope 组合：

| Runtime 与 scope | 精确目标 | 清理边界 |
| --- | --- | --- |
| `--runtime docker --scope images` | osdk 发现的 Docker context；`--context NAME` 用于选择发现目标并供展示 | 仅 dangling image；执行要求可通过本地 Unix socket 或 Windows named pipe 直接寻址，且不依赖 context TLS 材料 |
| `--runtime buildkit --scope build-cache` | 来自 `--builder`、生效配置或当前选择的 Buildx builder | 仅预览该 builder 的未使用 build cache |

虽然通用语法仍要求 `--scope`，但 containerd 没有任何可接受的 scope 组合。任一 scope 下，
不带 selector、也不带执行参数的请求都会返回类型化“不支持”；containerd 与 `--context`、
`--builder`、`--execute` 或 `--accept-preview` 的组合都会被拒绝。Docker build cache、
BuildKit image 等交叉组合同样会被拒绝。命令没有 `all` 或 `system` scope，也不会清理
container、volume、network、osdk CAS 或 runtime 的实现私有存储。`--context` 只适用于
Docker，`--builder` 只适用于 BuildKit。

默认调用只执行有界只读发现并打印预览。审阅时应确认精确目标与 warning：

```bash
osdk container prune --runtime docker --scope images --context desktop-linux
osdk container prune --runtime buildkit --scope build-cache --builder team-builder
```

预览会给出确定的 `sha256:` ID，并绑定操作所有者、scope、精确 context 或 builder、warning，
以及 Docker endpoint 或 Buildx driver/node endpoint 拓扑的敏感信息安全指纹。Docker 执行通过
`docker --host` 使用发现时捕获的精确原始本地 endpoint；context 名仅用于展示。Remote/SSH/TCP/TLS
context 与 Docker Desktop 目标会被拒绝，因为直接 `--host` 无法安全复现其 context 连接行为。要应用同一份
预览，必须同时加入两个执行 gate，再确认执行提示：

```bash
osdk container prune --runtime docker --scope images \
  --context desktop-linux --execute \
  --accept-preview sha256:PREVIEW_ID

```

只有 `--execute` 不够，只有 preview ID 而没有 `--execute` 也不够。若某个 ID 所属预览的
owner、scope、target、endpoint/拓扑指纹或 warning 与当前预览不同，该预览即已语义陈旧并会被拒绝；其他不匹配
ID 同样会被拒绝。此时应重新生成并审阅预览。绑定字段不变时会得到相同的确定 ID。全局
`--yes` 只回答最终执行提示，不能替代上述任一执行 gate。
确认后 Docker 直接使用已捕获的 endpoint 值，不再根据 context 名查询目标。BuildKit 不支持
执行，因为唯一可用的执行句柄是可变 builder 名称，无法原子固定。

## 配置与优先级

```toml
[containers]
runtime = "auto"
builder = "auto"
platform = "runtime"
probe_timeout_ms = 1500

[containers.registries."docker.io"]
mirrors = [
  "https://mirror-one.example/",
  "https://mirror-two.example/",
]
anonymous_only = true
resolve = "mirror"         # upstream（默认）|mirror
```

`--runtime` 和 `--builder` 会在本次命令中覆盖生效配置。非敏感选择器也可以通过
`OSDK_CONTAINER_RUNTIME`、`OSDK_CONTAINER_BUILDER` 和
`OSDK_CONTAINER_PLATFORM` 设置。每个捕获式探测使用 `probe_timeout_ms`，同时固定
stdout 和 stderr 各 64 KiB 的上限。

Registry key 是可带端口的规范 host name，不是 URL。Mirror 值必须是 HTTPS URL，不能
包含 credentials、query 或 fragment；加载时补尾部 `/`，去重但保留首次出现的配置顺序。
项目级 `[containers]` 需要显式信任，并整段替换低优先级配置。

`anonymous_only=true` 是 policy 默认值。Registry 测试无论该设置为何都保持匿名。原生
Docker/containerd/BuildKit 配置无法保证 runtime 永远不附加自己的凭据，因此
`anonymous_only=true` 的 plan 会报告 `anonymous-only-not-enforced` warning。
`resolve=upstream` 表示在 runtime 能表达时由 origin 解析 tag；`resolve=mirror` 允许 mirror
解析 tag。

## 测试 OCI Registry 与 mirror

```text
osdk container registry test REGISTRY
  [--image IMAGE]
  [--platform OS/ARCH[/VARIANT]]
  [--json]
```

```bash
# 只测试 upstream API；不要求 registry policy。
osdk container registry test docker.io

# 校验 tag 或 digest、选择 image index 平台，并测试 Range。
osdk container registry test docker.io \
  --image ubuntu:24.04 --platform linux/amd64

osdk container registry test ghcr.io \
  --image ghcr.io/example/tool@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --platform linux/arm64 --json
```

`REGISTRY` 是可带端口的 host。命令始终测试其规范 HTTPS upstream；`docker.io` 的传输
host 是 `registry-1.docker.io`。没有匹配的 `[containers.registries.<registry>]` 时，命令
只测试 upstream；有 policy 时，按配置顺序依次测试 mirror，不按延迟重新排序。未指定
image 时，对 upstream 与每个 mirror 调用 `/v2/`。指定 `--image` 时，其 registry 必须与
`REGISTRY` 一致；osdk 先解析并验证 upstream manifest，再让每个 mirror 按该不可变 digest
返回内容，不会在 mirror 上重新解析可变 tag。

`--image` 接受 tag 或 digest；digest selector 必须与返回的 manifest 字节一致。对于 image
index，`--platform` 必须唯一选中一个 child，并校验 descriptor digest 和 size。CLI 参数
覆盖显式 `[containers].platform`；两处都未提供平台时，遇到 index 会报告
`platform-not-found`。验证 image manifest 后，osdk 会对最小 layer 发送最多 16 KiB 的有界
Range 请求并校验精确 `Content-Range`；若 sample 已包含整个小 layer，还会验证 layer
digest。该命令不会拉取或保存完整 image。

诊断保持匿名：不读取原生 credential store、cookie、client certificate 或环境中的
Registry 凭据，并禁用 proxy。`401 Bearer` challenge 只有在 realm 为 HTTPS 且同 origin 时
才可获取匿名 pull token；challenge 如果带 scope，必须恰好等于
`repository:<repository>:pull`。Token 绑定签发 origin，upstream token 绝不会发送给
mirror。Redirect 只会在 HTTPS、同 origin 且仍在 request/redirect budget 内时手动跟随。

单次请求 timeout 是 `min(probe_timeout_ms, 60s)`，总 deadline 是
`min(单次 timeout × 12, 5 分钟)`；默认 1500 ms 对应每次 1.5 秒、总计 18 秒。一次诊断
最多 12 个请求，每条请求链最多 3 次 redirect，并限制 API/token/manifest body，只读取
最多 16 KiB 的 layer sample。`--offline` 会在构造网络 transport 前拒绝该命令。
`resolve` 与 `anonymous_only` 不会放宽这条命令：upstream 始终是权威来源，测试始终匿名。

人类输出支持中英文并以结论开头。`--json` 输出 Registry report schema version 1，包含
类型化 API、manifest、blob-range 和有序 mirror 检查；会显示 image、platform、digest、
字节数、Registry origin 与请求数，但绝不显示 token、原始响应 header 或 response body。
带 path prefix 的已配置 mirror 会在 `/v2/...` 前保留该 prefix；诊断 report 有意只显示
origin。

## 规划原生 mirror 配置

```text
osdk container mirrors plan REGISTRY
  --runtime docker|containerd|buildkit
  [--builder NAME]
  [--native-config PATH]
  [--containerd-main-config PATH]
  [--json]
```

位置参数 Registry 必须存在匹配的 `[containers.registries.<registry>]` policy。
`--runtime` 必填，没有 `auto`；每次调用只规划该 Registry 与该原生控制面，不会把其他
已配置 Registry 合并进 plan。`--builder` 只允许配合 `--runtime buildkit`；BuildKit 未提供
该参数时使用生效的 `[containers].builder`。

`--native-config` 指向精确的有界输入/目标：Docker daemon JSON、containerd 的
`<config_path>/<registry>/hosts.toml`，或所选 BuildKit builder 的 `buildkitd.toml`。省略时，
本来可在本机执行的 plan 会降级为 `manual-only`，且没有生成 candidate。containerd 的
`--containerd-main-config` 只在发现的配置还没有有效 Registry `config_path` 时可用，用来
指定需要补该 path 的主配置 TOML；已经存在 `config_path` 时传入该参数会报错。

```bash
osdk container mirrors plan ghcr.io --runtime containerd \
  --native-config /etc/containerd/certs.d/ghcr.io/hosts.toml --json

osdk container mirrors plan docker.io --runtime buildkit \
  --builder team-builder --native-config ./buildkitd.toml --json
```

不同 runtime 的边界不会被抹平：

| Runtime | Plan 边界 | 带 path prefix 的 mirror |
| --- | --- | --- |
| Docker | 只支持 `docker.io`。本地/rootless target 在 `resolve=mirror` 且指定 daemon JSON 时可为 `ready`；Moby 无法分离 origin resolution 与 mirror transfer，所以 `resolve=upstream` 为 `manual-only`。所描述的变更需要重启 daemon 才生效。 | 拒绝，因为 Moby `registry-mirrors` 只接受 origin URL。 |
| containerd | 在 `hosts.toml` 中按精确 Registry namespace 规划。`resolve=upstream` 给 mirror `pull` capability，`resolve=mirror` 给 `pull, resolve`；永不添加 `push`。in-memory candidate 保留其他无关 host/TLS 条目。 | 完整保留为 host table URL，并作为 containerd 在其后追加 `/v2/...` 的 base prefix；因此 osdk 不会推断 `override_path`。 |
| BuildKit | Docker driver 为 `unsupported`，因为它使用 Engine policy。本地 `docker-container` builder 在 `resolve=mirror` 且指定 TOML 时可为 `ready`，并报告 `recreate-builder`。Kubernetes、remote、cloud 和 upstream-resolution 场景为 `manual-only`。 | 在不序列化的 candidate TOML 中去掉 `https://`，写成 `host[:port]/path` 并保留 path。 |

人类输出先打印确定的 `sha256:` `plan_id`，再显示 applicability、change/candidate 数量、
activation requirement 和 warning。`--json` 输出 mirror-plan schema version 1。ID 绑定规范
target、policy、input/candidate fingerprint、语义变更、privilege、activation、validation
step 与 warning；这些语义变化会得到不同 ID。

Plan JSON 是操作 metadata，不是匿名报告。它会序列化规范绝对原生配置路径、所选 Buildx
builder 名和 containerd namespace/config path。对于 mirror，只输出脱敏 origin 与
`has_path_prefix`；精确 prefix 会替换为脱敏标记。分享前应先审阅。精确 prefix 本身永不
序列化：policy fingerprint 始终绑定它；存在 candidate 时，其隐藏 bytes 与 candidate
fingerprint 也会绑定它。JSON 保存 input state、size、SHA-256 fingerprint 与 candidate
size/format/fingerprint，但不包含现有原生配置内容或生成的 candidate bytes。

`mirrors plan` 只执行有界 no-follow 读取和原生发现，不会写文件、提权、重启 daemon、
重建 builder 或应用 plan；该命令没有 apply 选项。

## 状态处理建议

| 状态 | 含义 | 常见处理 |
| --- | --- | --- |
| `healthy` | 客户端和所选守护进程/构建器都已响应 | 无需处理 |
| `degraded` | 只获得了部分预期的类型化数据 | 检查原生服务后重试 |
| `client-only` | CLI 存在，但未确认守护进程/构建器 | 启动或选择目标服务 |
| `permission-denied` | endpoint 存在，但当前用户无法检查 | 修复原生 socket/context 权限 |
| `unreachable` | 所选 endpoint 未在有界时间内响应 | 检查守护进程、context、socket 或网络 |
| `unsupported-version` | 原生 CLI 早于 osdk 所需的机器可读契约 | 升级原生工具 |
| `not-installed` | 无法启动所需可执行文件 | 安装工具或选择其他运行时 |

缓存状态使用一组更具体的状态：

| 缓存状态 | 含义 |
| --- | --- |
| `available` | 已成功解析聚合记录和字节总量 |
| `not-installed` | 缺少所需的 Docker 或 Buildx 可执行文件 |
| `permission-denied` | 原生缓存接口拒绝当前用户访问 |
| `unreachable` | 原生服务报告连接失败 |
| `timed-out` | 有界缓存查询超过 `probe_timeout_ms` |
| `unsupported` | 不存在安全的聚合接口；containerd 当前返回此状态 |
| `unsupported-version` | 原生 CLI 过旧，不支持所需机器格式 |
| `output-truncated` | 达到固定的捕获上限 |
| `invalid-output` | 成功输出不符合类型化聚合 schema |
| `command-failed` | 原生命令失败，且无法归入更具体的状态 |

Doctor、cache status、registry test、mirror plan 与 prune preview 均为只读。
`container pull` 与经批准的 `container prune` 是本页说明的两条直接原生修改路径；两者都
不会改写 daemon 配置、启动或重建 builder、重启 daemon，也不会检查实现私有的存储目录。
Registry 测试只执行上文所述有界 metadata 与 Range 读取。选择、启动、plan、preview 与
披露边界见[原生容器诊断与操作实现](./implementation/containers)。
