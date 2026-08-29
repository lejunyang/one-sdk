# 容器运行时与原生缓存

osdk 可以在不修改配置或存储的前提下检查 Docker Engine、containerd 和
Docker Buildx。使用这些命令可以分别回答两个问题：哪个原生运行时可用，以及其
受支持的原生缓存接口报告了多少空间。

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

人类可读输出首先给出选中的运行时和状态。`--json` 输出确定的 schema version 1
对象，其中包含选中报告、自动选择期间尝试的所有运行时报告，以及独立的可选构建器
报告。字段名和枚举值不会随 `--lang` 翻译，因此中英文界面下的自动化契约完全一致。

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

## 配置与优先级

```toml
[containers]
runtime = "auto"
builder = "auto"
platform = "runtime"
probe_timeout_ms = 1500
```

`--runtime` 和 `--builder` 会在本次命令中覆盖生效配置。非敏感选择器也可以通过
`OSDK_CONTAINER_RUNTIME`、`OSDK_CONTAINER_BUILDER` 和
`OSDK_CONTAINER_PLATFORM` 设置。每个捕获式探测使用 `probe_timeout_ms`，同时固定
stdout 和 stderr 各 64 KiB 的上限。

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

这些命令均为只读：不会拉取镜像、清理缓存、改写守护进程配置、启动构建器，也不会
检查实现私有的存储目录。探测与脱敏边界见[容器诊断实现](./implementation/containers)。
