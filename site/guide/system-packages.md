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

## 声明需要哪些系统包

在项目的 `osdk.toml` 里声明，键名格式是 `管理器:包 ID`：

```toml
[syspkg]
managers = ["winget"]     # 只允许 winget 参与；留空表示不限制
no_elevate = false

[syspkg.packages]
"winget:BurntSushi.ripgrep.MSVC" = "latest"
"winget:Microsoft.PowerToys" = "0.101.0"
"winget:Some.MacOnlyTool" = { version = "latest", os = "macos" }
```

管理器前缀是必需的。包 ID 不跨平台通用——winget 的 `PackageIdentifier` 区分大小写且
对应仓库路径，brew 还要再分 formula 和 cask——所以 osdk 不做跨管理器的名称映射，
要求你写明是哪一个。

::: warning `[syspkg]` 需要先信任
这个表能导致在你机器上安装软件，属于会影响执行的项目配置，因此纳入 osdk 既有的信任
流程。未信任时任何 `pkg` 子命令都会拒绝执行并提示你先 `osdk trust`。
:::

::: tip 版本是「期望」，不是「锁定」
`[syspkg.packages]` 里的版本含义是「安装时按这个要求」，不是「把宿主固定在这个版本」。
系统包管理器按自己的节奏更新，`osdk.lock` 也**不覆盖系统包**。已装但版本不同的包，
osdk 会如实报告、**但不会重装**——那会改动你没要求改的东西。
:::

## 查看状态

```bash
osdk pkg status
osdk pkg status --json
osdk pkg status --missing     # 有缺失就退出非 0，供 CI 用
```

```text
System packages
  7zip.7zip            other version    requested 1.0.0-wrong (installed 22.01)
  Git.Git              ok               requested latest (installed 2.46.0)
  Some.MacTool         not for this os  requested latest (installed -)
  This.Is.Absent       missing          requested latest (installed -)
  invalid entry: `broken-no-prefix` needs a manager prefix, for example `winget:broken-no-prefix`
```

五种状态各有不同含义，其中两种容易混淆：

- **other version**：装了，但版本与请求不同。**不算缺失**，`--missing` 不会因它失败。
- **manager unavailable**：管理器本身查不到，因此**无法判断**这个包在不在。这不等于
  「包不存在」——把它当缺失会让你去装一个可能已经装了的东西。

写错的键会作为 `invalid entry` 列出而**不是被忽略**：它代表一个你以为被管理的包，
静默跳过就会让「没有缺失」变成假话。

## 安装缺失的包

```bash
osdk pkg plan                      # 只看要装什么
osdk pkg plan --detailed-exitcode  # 有待办返回 2，无待办返回 0
osdk pkg apply --dry-run
osdk pkg apply --yes               # 唯一会装东西的命令
```

计划会把每个请求都交代清楚——要装的、以及**为什么其余的不装**，不需要你从省略里推断：

```text
Would install:
  This.Is.Absent (latest)
    winget install --id This.Is.Absent --exact --no-upgrade ...

Left alone:
  7zip.7zip     present at another version; the configured version is a wish, not a lock
  Git.Git       already installed
  Some.MacTool  not for this operating system
```

::: warning osdk 不会顺手升级你的包
这一点是实测出来的坑：对**已安装**的包执行 `winget install`，winget 会自动开始升级它。
在装有 Git 2.46.0 的机器上实测，它立刻开始下载 2.55.0.3——而这不是你请求的操作。

所以 osdk 一律附加 `--no-upgrade`。「确保包存在」就只做这件事，不会改动你刻意停留的版本。
:::

一个包安装失败不会中断其余的：它们是彼此独立的请求。所有结果都会列出，只要有失败就
返回非 0。

::: tip 系统包不享有 osdk 级别的产物校验
osdk 对自己下载的 SDK 做哈希与签名校验，但系统包的字节 osdk 完全不接触——下载与校验
由 winget 完成（它会校验 manifest 里声明的安装器哈希）。这条界线值得写明，以免你以为
`osdk pkg apply` 装的东西和 `osdk install` 有同等强度的校验。
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
| osdk 调用 winget 时用哪个源 | 只影响 osdk 发起的那一次调用 | 否 | 已实现，但对 winget 不产生加速（见下） |
| winget 的全局源配置 | 影响机器上所有 winget 使用者 | **是** | **已实现**，需你确认一次 |

中间那层是"装依赖时自动加速"的落点：osdk 调 winget 时会自动选实测最快的**已注册**源，
你手敲 `winget install` 的行为完全不变，也不需要管理员权限。
`mirrors test` 的末尾会明确告诉你它选了哪个源、或者为什么不选：

```text
osdk will pass --source ustc-winget on winget calls it issues itself.
Your own winget commands are unaffected.
```

::: warning winget 的镜像加速不靠 `--source`，而是靠顶替默认源
这一点与直觉相反，是实机验证的结果。

winget 的软件源是一个固定身份的 MSIX 包（`Microsoft.Winget.Source_8wekyb3d8bbwe`），
所以**镜像无法作为一个独立的源与官方源并存**——以管理员身份新增会以
`0x80073D06`（"已安装此程序包的更高版本"）失败。镜像站官方教程之所以让你先
`winget source remove winget` 再 `winget source add winget <镜像地址>`，正是因为
顶替是唯一可行的形状。

顶替之后，镜像**就是**那个名为 `winget` 的默认源，对所有 winget 调用自动生效，
**不需要任何 `--source` 参数**。所以 winget 侧的加速完全由上表第三层达成。

而 `--source` 是排他的：指定一个源就屏蔽其余全部。实测加上 `--source winget` 后，
msstore 独有的包（如 WhatsApp）直接搜不到，且**退出码仍是 0、没有任何报错**。
因此无论那个源指向官方还是镜像，osdk 都会**省略** `--source`，以免让 msstore
的包变成"找不到"。
:::

osdk 也不会把内置的镜像名直接传给 winget：传一个未注册的源名会让整条命令以
`0x8A150012` 失败，把本可成功的安装变成错误。省略参数是唯一安全的降级方式。

只有最后一层会改这台机器的全局配置。它需要你确认一次，因为它影响的不只是 osdk——
此后所有人调 winget 都会看到这个源，而且镜像源拿不到官方源的 `StoreOrigin` 信任标记。
确认过之后 osdk 会自动维护，不再打扰你。

## 应用镜像

```bash
osdk pkg mirrors apply --dry-run          # 只看计划，不改任何东西
osdk pkg mirrors apply --accept-plan <指纹>  # 确认后执行（需管理员）
```

这是 `osdk pkg` 里唯一会改机器状态的命令。它按顺序做四件事：测速 → 排除装不上的
镜像 → 打印完整计划 → 只有带上正确指纹才执行。前三步都是只读的，所以不带
`--accept-plan` 运行永远不会改动宿主。

### 装不上的镜像会被提前拒绝

镜像落后于上游是常态，而 winget 会拒绝安装比本机更旧的包。osdk 在动手之前就检查这件事：

```text
No mirror can be applied right now.
  currently registered source published: Wed, 16 Sep 2026 01:08:41 GMT
  huaweicloud: publish time could not be established, so staleness cannot be ruled out
  ustc: published Tue, 15 Sep 2026 18:52:34 GMT, older than what is installed --
        winget would reject it with 0x80073D06

A mirror lagging behind upstream is common and resolves itself once it
syncs. Nothing was changed.
```

判据是各镜像 `source.msix` 的 `Last-Modified`，用一次 `HEAD` 取得，不下载那 20 MB 的包。
拿不到可信的发布时间就判为不可用——乐观放行恰好会撞上它要防的那次失败。

### 失败会自动回滚

顶替必须先 `remove` 再 `add`，两条命令之间宿主**没有任何软件源**。若 `add` 失败，
osdk 立即执行 `winget source reset` 恢复 winget 自带的源定义，并**如实报告回滚是否成功**：

```text
failed: winget source add --name winget ...
  exit code: -2147009274
  ROLLBACK FAILED: winget may have no package source right now.
  Run `winget source reset --name winget --force` as administrator.
```

回滚也失败时，恢复命令直接给在眼前，不需要你去查。

### 指纹是对「某个宿主状态」的确认

计划里的指纹覆盖执行时机器上注册的那批源。若在你确认之后源发生了变化（另一个管理员、
另一个工具、另一个终端），osdk 会拒绝执行而不是照着一份已经不符的计划动手。

::: tip 退出码可用于脚本判断
`--dry-run` 成功打印计划就是 0。但**请求了 apply 而最终没有应用**（无可用镜像、未确认、
指纹不符、宿主已变）一律返回非 0，所以脚本不会把"什么都没做"读成成功。
:::

## 当前边界

- 三层都已实现。Homebrew 尚未接入。
- 目前只覆盖 winget。Homebrew 在计划内。
- osdk 不代为提权。后续涉及需要管理员权限的操作时，osdk 会打印你需要自己执行的命令，
  而不是尝试提升权限。
