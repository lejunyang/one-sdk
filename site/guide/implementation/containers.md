# 容器诊断与原生缓存实现

本页说明 `osdk container doctor` 和 `osdk container cache status` 背后的
只读原生适配器路径。适配器把 Docker Engine、containerd 和 BuildKit 视为不同的
所有者；osdk 不会引入共享容器存储。

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

原始 `CommandSpec`、stdout 和 stderr 都不可序列化。报告只包含类型化状态/能力事实与
脱敏证据。endpoint 构造会去除用户信息、敏感 path/query 与 fragment；命令证据只记录
原生程序和操作目的。构建器名称、containerd namespace、缓存 ID/描述及解析错误都无法
进入稳定 JSON 契约。

## 失败边界

在底层契约可表达时，缺少可执行文件、权限失败、超时、endpoint 不可达、版本过旧、
输出截断、结构化输出无效和命令失败都会保留为不同的类型化状态。原生 stderr 只用于
分类，之后立即丢弃。因此状态查询可以给出有用的机器结果，而不会回显守护进程错误或
凭据。

这些命令的任何路径都不会执行前台命令、写入配置、拉取内容、清理原生数据、启动
构建器或扫描 osdk 的私有存储。
