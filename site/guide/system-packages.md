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

### Linux 包也能写进 `[syspkg.packages]`

```toml
[syspkg]
managers = ["apt"]

[syspkg.packages]
"apt:libssl-dev" = "latest"
"apk:build-base" = "latest"
"pacman:base-devel" = "latest"
"dnf:openssl-devel" = "latest"
```

`osdk pkg status` 会逐个查询它们，用的是各家文档化的只读接口，不需要 sudo。

::: warning 「查不到」和「没装」是两回事
如果宿主上根本没有 apt（比如在 Fedora 上写了 `apt:` 条目），osdk 报的是
**manager unavailable** 而不是 **missing**。

这个区分不是措辞讲究：把「问不到」当成「没装」，会让 `--missing` 在 CI 里误报，
也会让你去装一个可能早就装好的包。只有管理器确实回答了「没有这个包」才算 missing。
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

## Linux 的包管理器：代为安装，但只装缺的

在 Linux 上，`osdk pkg doctor` 会额外报告 apt / apk / pacman / dnf，`pkg apply` 也会
代为安装 `[syspkg.packages]` 里缺失的包。

```text
Linux package managers
  apt: present, rollback log only
  pacman: present, rollback manual downgrade from cache only
    this distribution supports only full-system upgrades; run `pacman -Syu` yourself
```

**代为安装的范围是刻意收窄的**：只安装声明了而宿主没有的包，不升级、不删除。
三条约束决定了这个边界：

1. **变更是全局的且需要 root。** apt 官方手册写明 `full-upgrade`「**会删除已安装的包**，如果这是整体升级系统所必需的」。所以 osdk 只调 `install`，从不调 `upgrade` 或 `full-upgrade`。
2. **Arch 官方声明部分升级不受支持。** Wiki 原文：「**never** run `pacman -Sy`；**always** use `pacman -Syu`」。而"只装我需要的那个包"恰恰就是部分升级。**所以 pacman 是唯一的例外：osdk 只打印命令，不代为执行**，因为这条限制来自发行版的官方立场，与 osdk 能不能提权无关。
3. **失败恢复能力差异极大。** dnf 有原子的 `history undo`；pacman 只能从 cache 手工降级；apt 只有日志、没有 undo。**跨发行版的统一抽象无法承诺一致的恢复语义**，所以 doctor 会把每家的回滚能力如实列出——装之前值得看一眼。

::: warning 已装但版本不同的包不会被重装
`[syspkg.packages]` 里的版本是**安装时的期望，不是锁**。宿主上已有其他版本时
osdk 会如实报告 `version differs` 并跳过，而不是为了对齐而改动一台你没要求改的机器。
:::

::: tip 检测全程只读，不需要 sudo
版本查询用的是各家文档化的只读接口：`dpkg-query -W -f=`、`apk info -e -v`、
`pacman -Q`、`rpm -q --qf`。都指定了显式的输出格式，因此不依赖可能变化的默认格式，
也不解析本地化表格。
:::

::: warning zypper 暂不在列
不是因为它不重要，而是**尚未取证**。现有线索反而指向它可能与 apt **不**同构
（openSUSE 通过 snapper 集成 btrfs 快照，可提供文件系统级回滚；zypper 有成文的退出码表）。
把它标为"与 apt 相同"会是一个未经验证的断言，所以先留空。
:::

### 提权：四种情况，绝不挂起

Linux 的包管理器都需要 root。osdk 按下面四种情况决定怎么办，顺序即判定顺序：

| 情况 | osdk 的行为 |
| --- | --- |
| **已是 root**（容器、CI） | 直接执行，不调用 sudo——那里 sudo 可能根本没装，也不需要 |
| **配置禁止提权**（`no_elevate = true`） | 不执行，打印命令让你自己跑 |
| **有免密 sudo** | 用 `sudo --non-interactive`，不会弹提示 |
| **交互终端** | 正常 `sudo`，像平时一样提示输密码 |
| **无终端且无免密 sudo** | **拒绝并打印完整命令**，绝不等一个没人会输的密码 |

最后一条是这套策略存在的理由：在没有 TTY 的 CI 里挂起等密码，会把整个 job 的超时耗光
才说话——**卡住比失败更糟**。

注意 `no_elevate` 禁止的是「提权」这个动作，不是「需要 root 的工作」。已经是 root 时
它不生效，因为那时根本没有提权发生。

无论哪种情况，完整命令行都会在执行前记录。
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

### Linux 上的镜像：自动生效，且不碰系统文件

在 Debian 或 Ubuntu 上安装 apt 包时，osdk 会自动走镜像——**不需要配置，也不需要先跑
别的命令**：

```text
$ osdk pkg apply --yes
Using the aliyun mirror for this run; no system file is modified.
Fetching the package index from it...
...
```

这和 winget 侧是两种机制，差别值得说清楚：

| | winget | apt |
| --- | --- | --- |
| 作用范围 | 机器全局的默认源 | **仅这一次调用** |
| 是否改系统状态 | 是，需确认与回滚 | **否** |
| 加速什么 | 只加速找包 | **找包和下包都加速** |

apt 允许把源、索引和缓存全部指向一个临时目录，因此加速不需要你点头，也不留痕迹。
实测确认：`/var/lib/apt/lists` 条目数不变、`/var/cache/apt/pkgcache.bin` 的 md5 不变、
`/etc/apt` 没有任何文件被写，运行结束后临时目录自动删除。

::: tip 拿不到镜像时会照常安装，而不是失败
识别不了的发行版、连不上的镜像，都会退回宿主自己的源并继续。镜像是优化，
不是前置条件——为了一个加速失败而拒绝安装是本末倒置。
:::

::: warning pacman 与 dnf 不做镜像
不是遗漏。这两家的镜像配置是一个 mirrorlist 文件，并有各自的排序工具
（`reflector`、`dnf-plugin-fastestmirror`），**没有 apt 那种"只影响单次调用"的覆盖方式**。
做一个半吊子的版本比明说不做更糟。
:::

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
