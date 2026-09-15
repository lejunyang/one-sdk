# 系统包管理器

osdk 在自己的 store 里管理 SDK，不管理宿主系统。但 SDK 工具链有时需要一些
osdk 不该拥有的东西——共享库、构建前置依赖——那是宿主自带包管理器的职责。

`osdk pkg` 用来查看宿主上有哪些系统包管理器、它们处于什么状态，以及哪个镜像源更快。
目前它只做检查与测速：不安装、不改配置、不提权。

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

## 测速镜像

```bash
osdk pkg mirrors test
osdk pkg mirrors test --json
```

osdk 会并发拉取每个候选源的索引包，按实测速度排名：

```text
Mirrors for winget
  1. huaweicloud        11.5 MiB/s  ttfb 338ms
  2. ustc                3.6 MiB/s  ttfb 457ms
  3. nju                 1.3 MiB/s  ttfb 444ms
  4. official            1.1 MiB/s  ttfb 620ms

These mirrors carry the package index only. Installers are downloaded
from each vendor's own servers, so switching source speeds up finding
a package, not downloading it.
```

官方源也在候选里。这是有意的：如果没有镜像比它快，你应该知道，而不是被推着去换源。

::: warning winget 镜像只加速"找包"，不加速"下包"
winget 的源是一份 manifest 索引，而 manifest 里的 `InstallerUrl` 指向各软件厂商
自己的服务器。换源之后，搜索和列表会变快，**下载安装包的速度完全不变**。

这一点与 Homebrew 不同：bottle 集中托管，镜像能把两段都加速。所以 osdk 在输出里
如实区分，避免你换完源发现下载依旧慢、以为功能坏了。
:::

测速只发 HTTP 请求，不会修改 winget 的任何配置。离线模式下该命令会直接报错退出，
而不是返回一份没有测过的排名。

未在镜像站帮助页中记录的端点（当前是 nju 与 huaweicloud）在没有实测数据时排序靠后：
它们今天能用，但运营方没有承诺。

## 镜像加速会怎样自动生效

测速本身是只读的，但它不是终点。镜像加速分三层，副作用逐层升级：

| 层 | 作用范围 | 需要管理员？ | 状态 |
| --- | --- | --- | --- |
| osdk 下载 SDK 用哪个源 | 只影响 osdk 自己的 store | 否 | **已自动**，见 [下载源与安全](sources-security.md) |
| osdk 调用 winget 时用哪个源 | 只影响 osdk 发起的那一次调用 | 否 | **已实现** |
| winget 的全局源配置 | 影响机器上所有 winget 使用者 | **是** | 计划中，首次需你确认一次 |

中间那层是"装依赖时自动加速"的落点：osdk 调 winget 时会自动选实测最快的**已注册**源，
你手敲 `winget install` 的行为完全不变，也不需要管理员权限。
`mirrors test` 的末尾会明确告诉你它选了哪个源、或者为什么不选：

```text
osdk will pass --source ustc-winget on winget calls it issues itself.
Your own winget commands are unaffected.
```

::: warning osdk 只会指定镜像源，不会指定官方源
`--source` 是排他的：指定一个源就屏蔽其余全部源。实测在本机加上 `--source winget`
后，msstore 独有的包（如 WhatsApp）会直接搜不到，而且**退出码仍是 0、没有任何报错**。

所以当实测最快的恰好是官方源时，osdk 会**省略** `--source` 而不是显式指定它——
显式指定不会更快（它本就是默认），却会让 msstore 的包变成"找不到"。
:::

镜像必须已经在 winget 里注册过，osdk 才会使用它。osdk 不会把内置的镜像名直接传给
winget：传一个未注册的源名会让整条命令以 `0x8A150012` 失败，把本可成功的安装变成错误。
没有可用镜像时省略参数，是唯一安全的降级方式。

只有最后一层会改这台机器的全局配置。它需要你确认一次，因为它影响的不只是 osdk——
此后所有人调 winget 都会看到这个源，而且镜像源拿不到官方源的 `StoreOrigin` 信任标记。
确认过之后 osdk 会自动维护，不再打扰你。

## 当前边界

- 只读。检测与测速可用；上表后两层尚未实现。
- 目前只覆盖 winget。Homebrew 在计划内。
- osdk 不代为提权。后续涉及需要管理员权限的操作时，osdk 会打印你需要自己执行的命令，
  而不是尝试提升权限。
