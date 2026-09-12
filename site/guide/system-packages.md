# 系统包管理器

osdk 在自己的 store 里管理 SDK，不管理宿主系统。但 SDK 工具链有时需要一些
osdk 不该拥有的东西——共享库、构建前置依赖——那是宿主自带包管理器的职责。

`osdk pkg` 用来查看宿主上有哪些系统包管理器、它们处于什么状态。目前它只做检查：
运行两条查询命令，不安装、不改配置、不提权。

## 查看宿主的包管理器

```bash
osdk pkg doctor
osdk pkg doctor --json
```

输出示例：

```text
System package managers
  winget: ok
    version: v1.29.290
    sources:
      winget (trusted) https://cdn.winget.microsoft.com/cache
      msstore (trusted) https://storeedgefd.dsx.mp.microsoft.com/v9.0

Nothing to fix.
```

报告会区分三种"不可用"，因为它们该做的事完全不同：

- **not installed**——宿主上本该有却没有，会给出安装指引。
- **not applicable on this platform**——比如 macOS 上没有 winget。这不是问题，
  osdk 不会建议你去装。
- **degraded**——客户端能用，但有部分状态读不到。

当宿主配置了非默认源时，报告会专门标出来，因为这意味着 manifest 来自镜像而非官方源。

## JSON 输出

`--json` 的 schema 带版本号，适合脚本消费：

```json
{
  "schema_version": 1,
  "managers": [
    {
      "manager": "winget",
      "status": "healthy",
      "details": {
        "kind": "winget",
        "version": "v1.29.290",
        "has_non_default_source": false
      },
      "capabilities": { "client": "supported" },
      "probes": [{ "purpose": "version-query", "outcome": "succeeded", "exit_code": 0 }]
    }
  ]
}
```

::: tip JSON 输出与界面语言无关
包管理器自己的表格输出是本地化的——中文 Windows 上 `winget list` 的列头是
「名称 / ID / 版本」。osdk 的 JSON 不受此影响：键名和取值在任何界面语言下都一样，
两次运行的输出逐字节一致。
:::

## 三条源链路不要混淆

osdk 里有三处都叫"源"，管的是不同的东西：

| 命令 | 管什么 |
| --- | --- |
| `osdk source` | osdk 下载 SDK 时用哪个源 |
| `osdk registry` | 项目依赖从哪个 registry 拉 |
| `osdk pkg` | 宿主包管理器的源与镜像（当前为只读查看） |

## 当前边界

- 只读。安装、镜像配置写入等能力尚未提供。
- 目前只检查 winget。Homebrew 在计划内。
- osdk 不代为提权。后续涉及需要管理员权限的操作时，osdk 会打印你需要自己执行的命令，
  而不是尝试提升权限。
