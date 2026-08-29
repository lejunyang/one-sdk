# 原生容器诊断与操作实现

本页说明 `osdk container doctor`、`osdk container cache status`、
`osdk container registry test` 与 `osdk container mirrors plan` 背后的只读适配器路径，
以及 `osdk container mirrors apply`、`osdk container pull` 和 `osdk container prune` 背后的
受控修改路径。适配器把
Docker Engine、containerd 和 BuildKit 视为不同所有者；osdk 不会引入共享 OCI 存储，也
不会静默改变操作所属的原生控制面。

## CLI 编排与选择

命令层读取生效的 `ContainersConfig`，应用显式 CLI 运行时/构建器选择，再把
`probe_timeout_ms` 转为 `CaptureLimits`，并为两个捕获流分别设置固定 64 KiB 上限。
所有子进程都经过可注入的 `CommandRunner`；生产环境使用 `SystemCommandRunner`，
测试则提供确定的结果，并检查命令顺序与限制。

显式选择 Docker 或 containerd 时只调用对应运行时适配器。自动选择按固定顺序探测
Docker 与 containerd，并按类型化 `DiagnosticStatus` 排序：健康、降级、仅客户端、
权限不足、不可达、版本不受支持、未安装；同状态时固定选择 Docker。这样不会把一个
只安装了客户端但不可用的环境排在可响应的守护进程之前。schema-v2 doctor 包装对象会
保留两份尝试报告以及各 runtime 的封闭 tagged details，而人类可读输出首先给出选中结论。

Buildx 始终是独立的可选报告。自动选择和 Docker 选择会检查它；显式 containerd
默认跳过，除非调用方传入 `--builder`。命名选择器在调用前完成验证，且不会进入序列化
证据。

details 投影只复用同一次探测的数据，不会启动额外进程。Docker 仅暴露类型化版本、
context 类型、daemon 平台、rootless/Desktop 与只含 origin 的 mirror；containerd 暴露
版本和配置状态，但不暴露 namespace 或配置路径；BuildKit 用稳定序号代替 builder/node
名称，校验平台 token，并把 endpoint 缩减为 scheme/host/port origin。缺失事实保持缺失，
不会被推断补齐。

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

## 直接启动镜像 pull

`container pull` 读取生效的容器 runtime 与 platform，其中显式 `--runtime` 和
`--platform` 优先。`--address` 与 `--namespace` 共同组成显式 containerd target。显式
Docker 或 containerd 只选择对应 adapter。`auto` 使用确定的诊断排序对 Docker 与
containerd 执行一次有界只读解析，并在构造任何修改命令前冻结选中的所有者。显式
containerd 立即要求这对参数；auto 只在 containerd 胜出时要求它们，因此 Docker 胜出时
无需 containerd selector 即可启动。

选中 adapter 只构造一个直接前台命令：

```text
docker image pull [--platform PLATFORM] IMAGE
ctr --address ADDRESS --namespace NAMESPACE images pull [--platform PLATFORM] IMAGE
```

子进程继承 stdio，而不是使用有界捕获路径，osdk 会等待它结束。进程结果为子进程的直接
退出码；Unix 上若由信号终止，则规范化为 `128 + signal`。命令启动后就是唯一一次尝试：
osdk 不会在 Docker 失败后改用 containerd 重试，反之亦然。解析阶段有界且只读，但前台命令
本身遵循原生客户端正常的 pull 生命周期。

Offline 模式会在解析和构造前台命令前失败。

Pull 路径不通过 osdk 传输 Registry 数据，不在 osdk CAS 中创建 image manifest 或 layer
对象，也不存在 osdk OCI store。Platform 只转发给选中的原生客户端；所有权、认证、内容
验证、unpack 与本地镜像可见性都由该客户端负责。

## 匿名 OCI Registry 诊断

CLI 从位置参数 Registry 构造 HTTPS upstream；逻辑名 `docker.io` 使用传输 host
`registry-1.docker.io`。Docker Hub 没有显式 policy 时使用 Google `mirror.gcr.io` 和
DaoCloud `docker.m.daocloud.io` 两个内置候选；显式 policy 完整覆盖内置值。其他 Registry
没有 policy 时只测试 upstream。只测 API 时在 upstream 和每个 mirror 上请求
`/v2/`；指定 image 时先解析并验证 upstream，再按解析出的 digest 请求每个 mirror，不会
重新解析可变 tag。Image registry 必须与位置参数 Registry 相同。

[`RegistryEndpoint`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/container/registry.rs)
只接受 HTTPS，拒绝 userinfo、query、fragment、percent encoding 与 parent component。
Mirror path prefix 会在 `/v2/...` 前保留，而序列化诊断只显示 origin。生产 transport 禁用
自动 redirect、proxy、gzip、cookie、client certificate 与环境 Registry credential。Redirect
只会在同一 HTTPS origin 且仍在固定 budget 内时手动跟随。

`401 Bearer` challenge 只有在 realm 为同 origin HTTPS 时才能请求匿名 pull token；另有
`registry-1.docker.io -> auth.docker.io` 和 `docker.m.daocloud.io -> m.daocloud.io` 两个
最小跨 origin allowlist。可选
scope 必须恰好是 `repository:<repository>:pull`。Token 请求本身不带 authorization，结果
绑定签发 origin。Upstream 和 mirror 独立认证，token 不会跨 origin。

CLI 从 `probe_timeout_ms` 派生 timeout：单请求为 `min(probe_timeout_ms, 60s)`，总 timeout
为 `min(单请求 × 48, 5 分钟)`。协议还限制整次运行默认 48、硬上限 64 个请求，每条请求链最多 3 次
redirect，API body 8 KiB、匿名 token 64 KiB、manifest 2 MiB、通用 body 4 MiB、layer Range sample
16 KiB。Offline 模式在构造 transport 前失败。

指定 image 时，对允许的 OCI/Docker manifest 精确字节计算 SHA-256，并校验显式 digest、
已有 `Docker-Content-Digest` 和所选 child descriptor 的 digest/size。Index 必须对请求平台
恰好选中一个 descriptor。最小 layer 接受精确有界 `Range` 采样；小 layer 被完整采样时
还会校验 digest。不会拉取或保存完整 image。

`RegistryDiagnosticReport` schema 2 序列化类型化 API、manifest、Range、每个 mirror 的
总耗时/排名和推荐顺序，以及安全 origin、请求 image/platform、digest、字节数与请求数。
测速先用 upstream 固定 tag 的 digest；每个 mirror 必须返回相同 Manifest，再对同一 layer
做有界 Range 请求，只有等价且返回有效样本者会进入按耗时排序的推荐。Token、原始 header、
response body、cookie 和原生 credential 都不能进入报告。

## 原生 Mirror 规划

CLI 要求一个已配置的位置参数 Registry（Docker Hub 可使用内置 policy）和一个显式
`docker|containerd|buildkit` runtime。缺少 policy 时在原生发现前失败；不存在 auto，也不会
把其他 Registry 合并进 plan。
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
path、state/format、size 与 SHA-256/metadata fingerprint。`mirrors plan` 丢弃 in-memory
candidate bundle，只打印 plan，因此始终只读。

`mirrors apply` 在同一进程内保留 candidate，且只接受 `ready`、恰好一个 candidate、
自认证 ID 有效的 plan。交互确认后再执行一次原生发现和 snapshot，要求新 plan ID 与展示给
用户的 ID 一致；随后获取 osdk apply lock，并在锁内再次以 no-follow 方式捕获输入，比较完整
metadata/content fingerprint。写入前重新解析完整 JSON/TOML，在同目录创建唯一临时文件，
写入、保留权限、flush/sync 后再次检查输入，再原子替换并同步父目录（Unix）。替换前失败会
清理临时文件且不覆盖目标；替换后的父目录同步失败会原样报告。无人值守 `--yes` 额外要求
`--accept-plan` 等于本次新生成的 ID；`--dry-run` 输出 ID 但不提示、不写入。该路径不提权、
不重启 daemon、不重建 builder。

## 原生缓存所有权

Docker 缓存状态把 `docker system df --format '<json-template>'` 解析为封闭分类：镜像、
容器、本地卷和构建缓存。BuildKit 缓存状态先确认 Buildx 0.28 或更高版本，再解析
`docker buildx du --format=json`；需要时绑定到已验证的构建器名称。总量会检查溢出，
也会拒绝活跃数量或可回收字节超过总量的关系。

containerd 不执行缓存命令，直接返回类型化的 `unsupported` 状态。containerd 提供
多个依赖 namespace 的 content、image、snapshot 与 CRI 视图，但没有单一的受支持
聚合接口。遍历 `/var/lib/containerd`、Docker 根目录、BuildKit 状态或任何原生私有
存储，会使 osdk 绑定实现细节并可能跨越权限边界，因此此路径永远不会这样做。

## 绑定预览的原生 prune

Prune 使用封闭的 runtime/scope 矩阵，只有 `docker + images` 与
`buildkit + build-cache` 合法。通用 CLI 语法仍要求 containerd 提供 `--scope`，但两个 scope
都不是可接受组合。不带 selector 或执行参数时返回类型化“不支持”；containerd 与
`--context`、`--builder`、`--execute` 或 `--accept-preview` 的组合会被拒绝。Docker/BuildKit
交叉组合也会在执行前失败。不存在通用 `all`/`system` scope，也没有任何路径会清理
container、volume、network、osdk store 或原生私有存储。

预览阶段执行有界只读发现，并解析出唯一精确的所有者目标：

- Docker 解析一个精确 context 用于展示，并在私有字段中保留其原始 endpoint。执行命令恰好是
  `docker --host ENDPOINT image prune --force`，因此随后对 context 名的重定向无法改变目标。
  只有不依赖 context TLS 材料的本地 Unix socket 或 Windows named pipe 可执行；remote、SSH、
  TCP/TLS、Docker Desktop 及信息不完整的目标均 fail closed。原生 scope 仅包含 dangling image。
- BuildKit 解析一个精确 Buildx builder 用于预览。显式 `--builder NAME` 优先；否则发现过程
  依次使用生效配置与当前 builder。因为原生 CLI 只提供可变 builder 名称而没有不可变 handle，
  所以不支持执行。

默认路径在渲染 schema version 2 预览后停止。确定的 SHA-256 ID 绑定 owner、scope、解析出的 target、warning，
以及完整 Docker endpoint 或 Buildx driver 加排序后 node 名/endpoint 拓扑的敏感信息安全指纹。
原始 endpoint 在诊断脱敏前即被哈希，绝不进入序列化输出。执行路径根据当前发现重新计算预览，并要求同时提供 `--execute` 和完全匹配的
`--accept-preview`。因此同名 context 或 builder 被重定向到另一 endpoint、driver/node 拓扑变化、
当前 context/builder 变化、显式 selector 不同、scope/owner 变化、
warning 变化，或者 ID 来自任何绑定字段不同的预览，都会在启动原生 prune 前 fail closed。
这种发生绑定变化的预览属于语义陈旧。ID 不包含时间字段；绑定字段不变时会重新得到相同 ID。

对于可执行的 Docker 预览，ID 匹配后只会进入执行确认阶段：仍需接受普通确认 prompt，也可用
全局 `--yes` 回答该提示；它不能替代 `--execute` 或 `--accept-preview`。之后传给唯一一次原生
prune 启动的是已经验证的 endpoint 值，而不是可变 context 名称。

## 序列化与脱敏

`DiagnosticReport` 是封闭的 schema version 2 契约；`NativeCacheStatus` 仍为 schema
version 1。Registry report schema version 2 包含实时耗时，因此其结构与排序规则稳定，
但字节级 JSON 不会跨运行保持相同。
有序 map/set 与已排序缓存记录保证其他重复 JSON 输出确定一致。Pull 选择与 prune preview 也会
在原生启动前使用规范类型化输入；prune preview 身份刻意包含精确 target 与 warning。JSON
字段名和枚举值永不本地化；人类可读标签只在选择完成后通过中英文 catalog 生成。

原始 `CommandSpec`、stdout 和 stderr 都不可序列化。doctor 与 cache 报告只包含类型化
状态/能力事实与脱敏证据。endpoint 构造会去除用户信息、敏感 path/query 与 fragment；命令证据只记录
原生程序和操作目的。构建器名称、containerd namespace、缓存 ID/描述及解析错误都无法
进入这些诊断/cache JSON 契约。Mirror plan 使用上文说明的另一套操作披露边界。

## 失败边界

在底层契约可表达时，缺少可执行文件、权限失败、超时、endpoint 不可达、版本过旧、
输出截断、结构化输出无效和命令失败都会保留为不同的类型化状态。原生 stderr 只用于
分类，之后立即丢弃。因此状态查询可以给出有用的机器结果，而不会回显守护进程错误或
凭据。

Doctor、cache status、Registry 测试、mirror plan、mirror apply dry-run 与 prune preview
都不会修改原生状态。Pull、经批准的 prune 和经批准的 mirror apply 是刻意限定的例外：
pull/prune 都只启动一个选中的原生命令且不回退，mirror apply 只写精确验证过的配置目标。
这里没有任何路径会启动/重建 builder、重启 daemon、扫描 osdk 私有存储或实现私有 OCI
store。Registry 测试只执行上述有界 metadata 与 Range 读取。
