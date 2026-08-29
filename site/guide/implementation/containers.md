# 容器诊断、Registry 测试与 Mirror Plan 实现

本页说明 `osdk container doctor`、`osdk container cache status`、
`osdk container registry test` 与 `osdk container mirrors plan` 背后的只读适配器路径。
适配器把 Docker Engine、containerd 和 BuildKit 视为不同所有者；这些命令既不会引入
共享容器存储，也不会修改原生配置。

## CLI 编排与选择

命令层读取生效的 `ContainersConfig`，应用显式 CLI 运行时/构建器选择，再把
`probe_timeout_ms` 转为 `CaptureLimits`，并为两个捕获流分别设置固定 64 KiB 上限。
所有子进程都经过可注入的 `CommandRunner`；生产环境使用 `SystemCommandRunner`，
测试则提供确定的结果，并检查命令顺序与限制。

显式选择 Docker 或 containerd 时只调用对应运行时适配器。自动选择按固定顺序探测
Docker 与 containerd，并按类型化 `DiagnosticStatus` 排序：健康、降级、仅客户端、
权限不足、不可达、版本不受支持、未安装；同状态时固定选择 Docker。这样不会把一个
只安装了客户端但不可用的环境排在可响应的守护进程之前。schema-v1 doctor 包装对象会
保留两份尝试报告，而人类可读输出首先给出选中结论。

Buildx 始终是独立的可选报告。自动选择和 Docker 选择会检查它；显式 containerd
默认跳过，除非调用方传入 `--builder`。命名选择器在调用前完成验证，且不会进入序列化
证据。

## 只读探测

Docker 适配器按顺序使用受支持的 CLI 格式：

```text
docker version --format '<json-template>'
docker context inspect
docker info --format '<json-template>'
```

containerd 适配器运行 `containerd --version`、`ctr --address ... --namespace ...
version`，并且只对本地 endpoint 执行 `containerd config dump`。显式 address 和
namespace 以参数传递，不从原生工具的环境变量中隐式推断。

BuildKit 适配器使用 `docker buildx version`、机器可读的 `buildx ls`，再对列表中
精确选中的结果执行 `buildx inspect`。它刻意不加 `--bootstrap`，所以检查不会启动
构建器。诊断的最低版本分别为 Docker 19.3、containerd 1.6 和 Buildx 0.10。

## 匿名 OCI Registry 诊断

CLI 从位置参数 Registry 构造 HTTPS upstream；逻辑名 `docker.io` 使用传输 host
`registry-1.docker.io`。`registry test` 的 policy 可省略：没有 policy 时只测试 upstream；
有匹配 policy 时按配置顺序加入 mirror。只测 API 时在 upstream 和每个 mirror 上请求
`/v2/`；指定 image 时先解析并验证 upstream，再按解析出的 digest 请求每个 mirror，不会
重新解析可变 tag。Image registry 必须与位置参数 Registry 相同。

[`RegistryEndpoint`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/container/registry.rs)
只接受 HTTPS，拒绝 userinfo、query、fragment、percent encoding 与 parent component。
Mirror path prefix 会在 `/v2/...` 前保留，而序列化诊断只显示 origin。生产 transport 禁用
自动 redirect、proxy、gzip、cookie、client certificate 与环境 Registry credential。Redirect
只会在同一 HTTPS origin 且仍在固定 budget 内时手动跟随。

`401 Bearer` challenge 只有在 realm 为同 origin HTTPS 时才能请求匿名 pull token；可选
scope 必须恰好是 `repository:<repository>:pull`。Token 请求本身不带 authorization，结果
绑定签发 origin。Upstream 和 mirror 独立认证，token 不会跨 origin。

CLI 从 `probe_timeout_ms` 派生 timeout：单请求为 `min(probe_timeout_ms, 60s)`，总 timeout
为 `min(单请求 × 12, 5 分钟)`。协议还限制整次运行最多 12 个请求、每条请求链最多 3 次
redirect，API body 8 KiB、匿名 token 64 KiB、manifest 2 MiB、通用 body 4 MiB、layer Range sample
16 KiB。Offline 模式在构造 transport 前失败。

指定 image 时，对允许的 OCI/Docker manifest 精确字节计算 SHA-256，并校验显式 digest、
已有 `Docker-Content-Digest` 和所选 child descriptor 的 digest/size。Index 必须对请求平台
恰好选中一个 descriptor。最小 layer 接受精确有界 `Range` 采样；小 layer 被完整采样时
还会校验 digest。不会拉取或保存完整 image。

`RegistryDiagnosticReport` schema 1 序列化类型化 API、manifest、Range 和有序 mirror 结果，
以及安全 origin、请求 image/platform、digest、字节数与请求数。Token、原始 header、
response body、cookie 和原生 credential 都不能进入报告。

## 原生 Mirror 规划

CLI 要求一个已配置的位置参数 Registry 和一个显式 `docker|containerd|buildkit` runtime。
缺少 policy 时在原生发现前失败；不存在 auto，也不会把其他 Registry 合并进 plan。
`--builder` 只允许用于 BuildKit。原生发现使用 `probe_timeout_ms`，stdout/stderr 捕获上限
各为 64 KiB。

三个 planner 保留各自的原生边界：

- Docker 只支持 `docker.io`。Moby `registry-mirrors` 只接受 origin，因此带 path prefix 的 mirror 会被拒绝而非截断。只有指定精确 daemon JSON 且使用 mirror resolution 时，本地/rootless target 才可能 `ready`；Moby 无法分离 resolution 与 transfer，所以 `resolve=upstream` 为 `manual-only`。activation 为 `restart-daemon`。
- containerd 针对精确 `<config_path>/<registry>/hosts.toml`。带 prefix 的 mirror 仍是 containerd 在其后追加 `/v2/...` 的 base URL；osdk 不推断 `override_path`，因为后者表示配置 path 已是 API root。`resolve=upstream` 赋予 `pull`，`resolve=mirror` 赋予 `pull, resolve`，永不添加 `push`。in-memory candidate 保留其他 host/TLS/custom entry。没有有效 `config_path` 时，显式 hosts path 与 `--containerd-main-config` 可描述两处变更，但 plan 为 `manual-only` 并要求 daemon restart。
- BuildKit 的 Docker driver 为 `unsupported`，因为它使用 Engine policy。本地 `docker-container` builder 提供 `buildkitd.toml` 时，在 mirror resolution 下可为 `ready` 并报告 `recreate-builder`；remote/Kubernetes/cloud/unknown target 和 upstream-resolution separation 为 `manual-only`。带 prefix mirror 在 candidate TOML 中写作 `host[:port]/path`。

省略 `--native-config` 时，本来能生成本地 candidate 的 planner 会记录
`native-config-path-required`，不生成 candidate fingerprint，并把 `ready` 降为
`manual-only`。Snapshot 上限 4 MiB；最终 component 使用 no-follow 打开，读取期间检测变化，
并能表达缺失的目标文件。

## Plan 身份与披露边界

[`MirrorPlan`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/container/plan.rs)
使用 schema version 1。Input/candidate 按 path 排序，`plan_id` 是除 ID 自身外全部语义字段
canonical JSON 的 SHA-256，包括 target/policy、input/candidate fingerprint、change、
privilege、activation、validation step 与 warning。人类输出先打印 ID，`--json` 输出完整
语义 plan。

Plan JSON 是操作数据而非匿名报告。它可能包含规范绝对原生配置路径、所选 Buildx builder
名、containerd namespace/config path 和脱敏原生 endpoint 身份。Mirror change 只显示
origin 与 `has_path_prefix`；精确 prefix 替换为脱敏标记。policy fingerprint 始终绑定精确
prefix，生成 candidate 时其隐藏 bytes 与 fingerprint 也会绑定它。分享 plan JSON 前应先
审阅。

`NativeConfigSnapshot` 内容和生成的 `NativeConfigCandidate` bytes 不可序列化；JSON 只保留
path、state/format、size 与 SHA-256/metadata fingerprint。CLI 丢弃 in-memory candidate
bundle，只打印 plan，也没有 apply 选项。因此规划只执行有界读取与发现，绝不会写文件、
提权、重启 daemon 或重建 builder。

## 原生缓存所有权

Docker 缓存状态把 `docker system df --format '<json-template>'` 解析为封闭分类：镜像、
容器、本地卷和构建缓存。BuildKit 缓存状态先确认 Buildx 0.28 或更高版本，再解析
`docker buildx du --format=json`；需要时绑定到已验证的构建器名称。总量会检查溢出，
也会拒绝活跃数量或可回收字节超过总量的关系。

containerd 不执行缓存命令，直接返回类型化的 `unsupported` 状态。containerd 提供
多个依赖 namespace 的 content、image、snapshot 与 CRI 视图，但没有单一的受支持
聚合接口。遍历 `/var/lib/containerd`、Docker 根目录、BuildKit 状态或任何原生私有
存储，会使 osdk 绑定实现细节并可能跨越权限边界，因此此路径永远不会这样做。

## 序列化与脱敏

`DiagnosticReport` 和 `NativeCacheStatus` 是封闭的 schema version 1 契约。
有序 map/set 与已排序缓存记录保证重复 JSON 输出确定一致。JSON 字段名和枚举值永不
本地化；人类可读标签只在选择完成后通过中英文 catalog 生成。

原始 `CommandSpec`、stdout 和 stderr 都不可序列化。doctor 与 cache 报告只包含类型化
状态/能力事实与脱敏证据。endpoint 构造会去除用户信息、敏感 path/query 与 fragment；命令证据只记录
原生程序和操作目的。构建器名称、containerd namespace、缓存 ID/描述及解析错误都无法
进入这些诊断/cache JSON 契约。Mirror plan 使用上文说明的另一套操作披露边界。

## 失败边界

在底层契约可表达时，缺少可执行文件、权限失败、超时、endpoint 不可达、版本过旧、
输出截断、结构化输出无效和命令失败都会保留为不同的类型化状态。原生 stderr 只用于
分类，之后立即丢弃。因此状态查询可以给出有用的机器结果，而不会回显守护进程错误或
凭据。

这些命令的任何路径都不会执行前台命令、写入配置、拉取完整 image、清理原生数据、
启动/重建 builder、重启 daemon 或扫描 osdk 私有存储。Registry 测试只执行上述有界
metadata 与 Range 读取。
