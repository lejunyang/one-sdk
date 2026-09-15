# osdk 系统包管理器集成设计（winget / Homebrew）

日期：2026-09-12

状态：**设计提案，尚未实现。** 本文不描述当前仓库已有的行为，`crates/` 下没有任何对应代码。

上游事实核查基准日：2026-09-12。所有对 winget、Homebrew、mise 的行为断言都附来源；无法从官方来源确证的一律标注「待验证」并给出验证方法，**不得在实现时当作既定事实使用**。

配套可视化：[`system-package-managers-2026-09-12.visual.html`](./system-package-managers-2026-09-12.visual.html)（架构图、能力对比矩阵、镜像探测流程、体积影响、分阶段路线）。

---

## 执行摘要

用户诉求是「给 osdk 增加类似 mise bootstrap packages 的包管理器功能，初期适配 winget 与 Homebrew，要有镜像加速检测，最好能有安装目录控制」。核查之后，有四个结论需要前置说明，因为它们决定了整个设计的形状：

**一、安装目录控制在这条路上基本拿不到，这不是实现难度问题。** Homebrew 的 prefix 事实上固定为 `/opt/homebrew`（Apple Silicon）、`/usr/local`（Intel）、`/home/linuxbrew/.linuxbrew`（Linux）；装到别处会让 bottle 失效退化为源码编译，官方对此的定性是「耗时很长、容易失败、**且不受支持**」，并在 Support Tiers 中把非默认 prefix 降为 Tier 2/3——Tier 3 的后果包括「运行时打印响亮的配置警告」「只影响这些配置的 issue 可能不予回复」「功能可能为了受支持配置的利益而**故意退化**」。winget 侧 `--location` 的官方措辞本身就是「Location to install to (**if supported**)」，只有 portable 类型有完整文档化的目录模型。**用户的预感是对的，而且比预想的更彻底：这个能力不该由 osdk 去争取。** 详见 §4。

**二、反过来，镜像加速的空间比预想的大，但两侧性质完全不同。** winget 的国内镜像是**存在**的（USTC 有官方帮助页，NJU 与华为云路径可用），这一点与「winget 没有国内源」的流行印象相反。但 winget 镜像只镜像 manifest 索引，**不镜像安装包本身**——manifest 里的 `InstallerUrl` 指向上游原始地址，所以换源只加速「找包」不加速「下包」。Homebrew 则因为 bottle 集中托管在 ghcr.io，镜像能真正加速二进制产物。这个结构性差异必须诚实地反映在 osdk 的能力说明里，否则用户会以为换了 winget 源就能解决下载慢。详见 §5。

**三、能力不应该扩展 `Backend` trait。** 项目 AGENTS.md 记录的那条踩坑经验在这里正面命中：`Registry::new()` 会把全部 backend 实例化成 `Arc<dyn Backend>`，trait 上每新增一个方法都进 vtable、链接器无法裁剪，这曾让 shim 白背 5.15 MB。而仓库里已经有一个现成的、正确的先例——`container/` 子系统管理 Docker/containerd/BuildKit 这些宿主设施，它**刻意不实现 `Backend` trait**。系统包管理器与容器运行时是同一类东西（宿主上已存在的、osdk 不拥有的设施），应当复用同一套架构姿态。至于「不如干脆自实现 Homebrew」这条岔路，已做实测否决：**即便按最便宜的可行组合**（`oci-client` 升到与 osdk 对齐的 0.17、签名 shell out、复用已在图中的 `resolvo`），对 `osdk` 仍净增 **+1.56 MB（+13.1%）**，是 10% 红线的 1.31 倍；纯 Rust 签名则为 +2.90 MB（+24.3%）。详见 §6 与 §6.4。

**四、定位应当是「宿主设施的协调者」，而不是「又一种 backend」。** mise 在这件事上做过一次完整的架构选型并留下了决策记录：社区最初提议做成 `[tools]` 里的 `winget:` backend，维护者最初同意，随后被说服改为挂到 bootstrap 体系，理由是「这更适合挂到 `mise bootstrap` 系统，像其他宿主包管理器（apt、dnf、brew）那样」，最终 tool backend 的 PR 未被采纳。osdk 应当直接采纳这个已被验证的结论，不必重走一遍。详见 §3。

**五、Linux 发行版包管理器（apt / apk / pacman）应当降一级，不与 winget/Homebrew 同列。** 它们管理全局系统状态、变更一律需 root、且与发行版自身的升级生命周期耦合——Arch 官方明确声明**部分升级不受支持**（「never run `pacman -Sy`; always use `pacman -Syu`」），而「只装我需要的那一个包」恰恰就是部分升级。加上三家失败恢复能力差异极大（dnf 有事务撤销、pacman 只能靠 cache 手工降级、apt 只有日志无 undo），跨发行版的统一抽象无法承诺一致语义。**结论是只做「只读检测 + 镜像建议 + 打印可复制的命令」，不代为执行。** 详见 §7。

由此得到的设计主线是：**osdk 对系统包管理器只做发现、状态查询、镜像探测与配置建议、以及显式 apply；不接管安装目录、不纳入 shim 体系、不对其做可复现性承诺。** 需要真正的目录控制与可复现安装时，答案是走 osdk 自有的下载安装通道（现有 backend 体系），而不是把系统包管理器硬掰成那个样子。

---

## 1. 问题陈述与非目标

### 1.1 要解决的真实问题

osdk 现有的 13 个 backend 覆盖的是**语言运行时与开发工具链**——它们的共同特征是自包含归档、可多版本并存、可被 shim 精确路由。但有一类依赖不具备这些特征，且现有体系结构上无法覆盖：

- **共享库与构建期依赖**：`libssl-dev`、`pkg-config`、`cmake`、`openssl`。它们要参与系统链接器与 `pkg-config` 搜索路径，装在 osdk 的 `installs/<tool>/<version>/` 下并通过 shim 暴露是没有意义的——没人「执行」libssl。
- **宿主级 GUI 应用**：`firefox`、`visual-studio-code`。它们需要被系统的应用启动器发现。
- **一次性的机器初始化**：新机器上先要有 `git`、`curl`、`7zip` 这类基础工具，osdk 自己才好开展工作。

mise 对这条边界的表述值得原样引用，因为它把「为什么不能用普通 backend 解决」讲清楚了：「共享库包——postgres、ffmpeg、imagemagick、php——**根本无法由 mise 的按项目 backend（如 `aqua:` 或 `github:`）提供服务：它们的 bottle 是针对固定安装路径和共享依赖树构建的。把它们装在 Homebrew 的规范 prefix 上，正是它们能工作的原因。**」
来源：https://mise.jdx.dev/bootstrap/packages/brew.html

### 1.2 非目标（明确不做）

这一节的每一条都是**主动放弃**，不是「暂未实现」：

1. **不接管系统包的安装目录。** 理由见 §4，这是外部约束而非能力不足。
2. **不为系统包生成 shim。** 系统包管理器装的东西由宿主的 PATH 机制暴露；给它们生成 shim 会造成两套 PATH 语义互相覆盖，且无法回答「shim 指向哪个版本」——系统包管理器通常只允许一个版本存在。
3. **不把系统包纳入 `osdk.lock` 的可复现性承诺。** 详见 §10。Homebrew 是 rolling release，官方明确「不支持安装任意旧版本」且「`brew bundle` 不会、也永远不会有 lock file 的概念」；winget 的历史版本依赖上游 URL 存活。在这种基底上声称「锁定」是欺骗性的。
4. **不做隐式安装。** 任何系统包的安装都必须由用户显式触发。osdk 可以在检测到缺失时给出提示，但不能在 `osdk install` 的过程中顺手装系统包——那会在用户不知情的情况下触发 UAC 或 sudo。
5. **不代理或重新实现包管理器本身。** 尤其不重走 mise 自实现 Homebrew bottle pour 的路（§3.3 解释为什么）。
6. **初期不覆盖 Linux 包管理器（apt/dnf/pacman）与 Scoop/Chocolatey。** 架构上要为它们留位置，但首版只做 winget 与 Homebrew。

---

## 2. 外部事实基线

本节只列**直接影响设计决策**的事实。完整核查（含 winget 退出码全表、Homebrew 七级锁版本工具阶梯、各镜像站实测状态码）见配套可视化的能力矩阵，以及本节各条目附带的来源 URL。

### 2.1 winget：自动化友好度低于直觉

| 维度 | 事实 | 对设计的影响 |
| --- | --- | --- |
| 机器可读输出 | **没有通用 `--output json`**。JSON 面只有三条：`winget export`（写文件，非 stdout）、`winget dscv3`（DSC v3 资源）、`Microsoft.WinGet.Client` PowerShell 模块。维护者在 issue #5051 明确表态「不要用 WinGet 二进制，任何要对接 WinGet 的应用都推荐使用 PowerShell 模块或 COM API」 | 解析 CLI 文本输出是**下策但初期唯一可移植的选择**；必须把解析层隔离并视为脆弱面（§7.3） |
| stdout 污染 | issue #6054（2026-02，winget v1.12.470）报告 `list`/`upgrade`/`export` 因 spinner 帧写入 stdout 导致 JSON 被破坏；报告者称 `--disable-interactivity` 无法抑制。`--no-progress` 存在但未出现在 Learn 各子命令选项表中 | 任何调用都必须带 `--no-progress`，且解析器要能容忍前导控制序列 |
| 退出码 | HRESULT 体系（如 `0x8A150061` / `-1978335135` = `PACKAGE_ALREADY_INSTALLED`），**不是 0/1**。且有正值状态码（如 `0x0A150202`） | 不能用「非零即失败」判定 |
| 幂等性 | **「已安装」是用错误码表达的**：`PACKAGE_ALREADY_INSTALLED`、`INSTALL_ALREADY_INSTALLED`、`INSTALL_DOWNGRADE`、`UPGRADE_VERSION_NOT_NEWER` | 幂等必须靠显式白名单退出码实现（§8.4） |
| 权限 | 提升会话下不显示 UAC 提示；反向存在 `INSTALLER_PROHIBITS_ELEVATION`（不能在管理员上下文运行）与 `ADMIN_CONTEXT_ACTION_PROHIBITED`。`source add/remove/reset` **需要管理员** | osdk 不应尝试自行提升；换源操作要预先告知需要管理员（§8） |
| SYSTEM 上下文 | 官方明确「WinGet CLI 在 system context 下不受支持」 | CI/服务账户场景要给出明确的不支持提示而非神秘失败 |
| 可用性门槛 | Windows 10 1809 (17763)+；**首次用户登录前不可用**（需 Store 异步注册）；版本 < 1.6.3482 因退役 CDN 可能**无任何输出** | doctor 必须能区分「未安装」「版本过旧」「未注册」三种状态 |

来源：https://learn.microsoft.com/en-us/windows/package-manager/winget/ ，https://learn.microsoft.com/en-us/windows/package-manager/winget/install ，https://github.com/microsoft/winget-cli/blob/master/doc/windows/package-manager/winget/returnCodes.md ，https://github.com/microsoft/winget-cli/blob/master/doc/Settings.md ，https://github.com/microsoft/winget-cli/issues/5051 ，https://github.com/microsoft/winget-cli/issues/6054 ，https://learn.microsoft.com/en-us/windows/package-manager/winget/source ，https://learn.microsoft.com/en-us/windows/package-manager/winget/troubleshooting

### 2.2 Homebrew：自动化友好度高于 winget，但版本控制能力更弱

| 维度 | 事实 | 对设计的影响 |
| --- | --- | --- |
| 机器可读输出 | `brew info --json` 对 formula 默认 **v1**，需 `--json=v2` 才含 cask；`brew outdated --json` 的 **v1 已 deprecated 但仍是默认值**；`tap-info --json` 只接受 v1 | 必须显式指定 `--json=v2`，绝不依赖默认值 |
| schema 稳定性 | 官方：「字段可能在不递增 schema 的情况下按需添加。任何重大的破坏性变更都会导致 schema 版本变化」，且「schema 本身目前除了生成它的 `formula.rb` 代码之外没有文档」 | 解析必须容忍新增字段（宽松反序列化），不得用 `deny_unknown_fields` |
| 免装查询通道 | `formulae.brew.sh` 提供有文档的 JSON API，等同 `brew info --json=v1` 且**无需本地安装 Homebrew** | 可在 Homebrew 未安装时仍提供「这个包存在吗」的查询与镜像探测（§5.3） |
| 退出码 | **无编号化退出码表**（与 winget 相反）。官方仅零散文档化三处：`brew --prefix --installed <formula>`（未装则失败）、`brew bundle check`、cleanup 的 exit 1 | `--prefix --installed` 是官方文档化的布尔探测入口，应优先使用 |
| 幂等性 | `brew bundle install` 对已装且最新**明确是 no-op**；裸 `brew install` 走 `opoo`（warning）路径 | 比 winget 好得多，但 `brew install` 的退出码未文档化（**待验证 V-2**） |
| 版本固定 | Homebrew 是 rolling release，官方「不支持安装任意旧版本」。`brew pin` 有三条硬 Cons：pin 期间收不到安全更新、**被 pin 的 formula 会阻塞其他 formula 的升级**、被 pin 的 cask 仍可能自行更新 | 不能把 `brew pin` 当作版本锁定原语（§9） |
| sudo | **Homebrew 拒绝以 sudo 运行**。sudo 只出现在初始安装、以及 cask 的 `pkg`/`installer script` 产物 | osdk 绝不能用 sudo 调用 brew；cask 的密码提示要透传给用户而非吞掉 |
| 平台门槛 | 要求 Apple Silicon + macOS Sonoma (14)+；**Intel 为 Tier 3**；Catalina 及更早完全不可用。CLT 边界：源码构建必需，但 **cask 与 bottle 可无开发者工具安装**——**成立前提是 Homebrew 在 arm64 上用 ruby-macho 的 `MachO.codesign!` 而非 `codesign`**（`codesign` 需要 CLT，见 §6.4.7） | doctor 要能报告 Tier 状态 |

来源：https://docs.brew.sh/Manpage ，https://docs.brew.sh/Querying-Brew ，https://docs.brew.sh/FAQ ，https://docs.brew.sh/Versions ，https://docs.brew.sh/Support-Tiers ，https://docs.brew.sh/Installation ，https://docs.brew.sh/Brew-Bundle-and-Brewfile ，https://formulae.brew.sh/api/formula/node.json

### 2.3 包 ID 命名空间不存在可算法推导的映射

| 软件 | winget ID | Homebrew | 差异性质 |
| --- | --- | --- | --- |
| VS Code | `Microsoft.VisualStudioCode` | `visual-studio-code`（**cask**） | publisher 前缀 vs 无前缀；formula/cask 分野 |
| Node.js LTS | `OpenJS.NodeJS.LTS`（另有 `OpenJS.NodeJS`、`OpenJS.NodeJS.22`） | `node`，版本线用 `node@<N>` | winget 用独立 ID 表达 LTS，brew 用独立 formula 表达版本 |
| PowerShell | `Microsoft.PowerShell` | `powershell`（cask） | 同上 |

winget 的 `PackageIdentifier` **区分大小写**且须匹配仓库目录结构；brew 侧还需额外判断是 formula 还是 cask（决定要不要 `--cask`），这个判断在 winget 侧没有对应概念。**结论：不做跨平台包名自动映射**，配置里按平台分别声明（§8.2）。

来源：https://learn.microsoft.com/en-us/windows/package-manager/package/manifest ，https://formulae.brew.sh/api/formula/node.json ，https://github.com/microsoft/winget-pkgs/issues/193118

---

## 3. mise bootstrap packages 的做法：借鉴什么，避开什么

用户点名要参照 mise。核查下来，mise 在这件事上的**架构决策**值得直接采纳，但它的**实现深度**恰恰是 osdk 应当避开的。

### 3.1 应当借鉴：架构分离，且有决策记录

mise 把系统包放在独立的 `[bootstrap.packages]` 而非 `[tools]`。官方表述：「宿主包**有意与 `[tools]` 分离**：它们不按项目做版本 pin、不获得 shims、并由平台的包管理器在项目之外管理。……把它们用于共享库、构建依赖和宿主 GUI 应用，**而不是用于项目开发工具——那些属于 `[tools]`**。」

更有价值的是决策过程有据可查：discussion #8311 中社区提议做成 `[tools]` 里的 `winget:` backend（`"winget:Amazon.AWSCLI" = "latest"`），维护者 jdx 最初回「seems fine to me」；随后 sargunv 提出「这更适合挂到 `mise bootstrap` 系统，像其他宿主包管理器（apt、dnf、brew）那样，而不是做成 tool backend」，jdx 回「yeah I agree」。对应的 tool backend 实现 PR #8856（用 `winget show --versions` 列版本、`winget install --location` 装入 mise 管理目录）最终**未被采纳**。

**这正是 osdk 面临的同一个选择**，而且 osdk 有更强的理由走同一条路——§6 的 vtable 体积约束在 mise 那里并不存在。

来源：https://mise.jdx.dev/bootstrap/packages/ ，https://github.com/jdx/mise/discussions/8311 ，https://github.com/jdx/mise/pull/8856

### 3.2 应当借鉴：几条具体的行为约定

| mise 的做法 | 为什么值得抄 |
| --- | --- |
| **「永不隐式安装」**：「mise 从不隐式安装系统包。`mise install` 在缺包时打印一次性提示，但**只有 `mise bootstrap packages apply` 会安装任何东西**」 | 系统包安装会触发 UAC/sudo，隐式触发是对用户机器的越权 |
| **每次执行前记录完整命令行**：「in every case, the full command line is logged before it runs」 | 与 osdk `container/` 子系统的 `redact.rs` 姿态一致；用户必须能看到 osdk 到底替他跑了什么 |
| **sudo 边界四档**：已 root 则直跑；交互式终端正常提示；**非交互且无免密 sudo 则报错并打印你需要手工运行的确切命令——「it never hangs waiting for a password」**；`sudo = false` 完全禁止 | 「永不挂起等密码」是 CI 场景的硬要求 |
| **pin 能力诚实分档**：明说 apk/apt/dnf 支持 pin，而 AUR/pacman/brew/brew-cask/flatpak/mas **无法安装 pin，被 pin 的条目会被跳过并给出警告** | 宁可跳过并警告，也不假装支持 |
| **CI 语义**：`status --missing` 缺包则 exit 1；`plan --detailed-exitcode` 三态（0 无变更 / 2 有变更 / 1 规划失败或存在 `unknown` 资源） | 直接可用的退出码契约设计 |
| **非事务性明说**：「Bootstrap is a sequence, not a transaction: if a later phase fails, earlier successful changes remain」 | 诚实声明胜过假装原子性 |
| **不管 TCC**：明说替换 `/Applications` 下的 app 会导致 macOS 撤销隐私授权，且因此**刻意**不把 app 内容漂移当作需要重装 | 知道哪些事不该「自动修复」 |

来源：https://mise.jdx.dev/bootstrap/packages/ ，https://mise.jdx.dev/bootstrap.html ，https://mise.jdx.dev/bootstrap/packages/brew.html ，https://mise.jdx.dev/bootstrap/packages/winget.html

### 3.3 应当避开：mise 对 Homebrew 的自实现

这是本节最重要的判断。**mise 的 winget 与 brew 走的是两条完全相反的路线**，很容易被误判为同一套东西：

- **winget 侧是真代理**：shell out 到 `winget.exe`，用 `--id` + `--exact`，status 靠 `winget list --id <ID> --exact`，pin 按**不透明字符串**比较，silent + 禁交互 + 接受两类协议，`--update` 刷新 source，**不绕过 UAC**，原样透出 winget 失败。
- **brew 侧是完全自实现**：「mise 把 homebrew/core formula **直接**装进规范的 Homebrew prefix……从 formulae.brew.sh API 取元数据，解析运行时依赖闭包，从 ghcr.io 下载预构建 bottle（校验 sha256），并执行 `brew` 倾倒 bottle 时所做的**同样的重定位、代码签名和链接工作**。……**mise 从不为 homebrew/core formula shell out 到 `brew`。**」

自实现的深度远超「封装」：六步 pour 流程（fetch / extract / relocate / re-sign / receipt / link），其中 relocate 要重写 Mach-O 的 load command（「exactly like brew's ruby-macho does」）、Linux 上按 PatchELF 方式打 ELF interpreter 与 rpath；macOS arm64 上还必须做 ad-hoc 重签名，因为「内核会杀掉签名不匹配的二进制」（这一条有 Apple 官方依据：「the operating system enforces that **any executable must be signed** before it's allowed to run……a simple ad-hoc signature is sufficient」，且「doesn't apply to translated x86 binaries running under Rosetta 2, nor……on Intel-based platforms」）。连无 bottle 时的源码构建也自实现——用 mise 管理的 ruby 加**自有的 Formula-DSL shim** 求值 formula 并运行 `def install`。

> 关于这一步「用什么做签名」，Homebrew 自己换过实现，这个细节对 osdk 的选型很关键——见 §6.4.7。简言之：arm64 上它用 ruby-macho 的纯实现，**刻意避开需要 CLT 的 `codesign`**。
> 来源：https://developer.apple.com/documentation/macos-release-notes/macos-big-sur-11_0_1-universal-apps-release-notes

**osdk 不应走这条路**，理由有三：

1. **维护面与 osdk 的定位不匹配。** 这等于在 osdk 里长出半个 Homebrew。mise 自己列出的 Limitations 清单已经很长：cask artifact 覆盖窄（「其他 artifact 类型、无 `pkgutil` ID 的 pkg 安装器、带自定义 choices 的 pkg 安装器**显式失败**」）、`brew services` 未实现、cask import 未实现、源码构建只覆盖「常见 formula 形状」、非 GitHub tap 不支持。
2. **故障模式很重。** 社区实证：discussion #11058 报告对含 `__MACOSX/` 目录的 zip，「**若该 app 已存在于 `/Applications`，mise 会在失败前把可工作的 app 替换成一个空目录骨架**」；discussion #12367 报告裸 XAR/pkg 下载被误当作 raw executable 暂存导致安装失败（已修）。这类 bug 是自实现产物处理不可避免的代价。
3. **体积代价直接撞上 osdk 的硬约束——这一条已实测，不是估算。** 自实现 bottle pour 需要 OCI registry 客户端、Mach-O/ELF 重写、macOS 代码签名。**按最便宜的可行组合**（`oci-client` 升到与 osdk 对齐的 0.17、签名 shell out、依赖求解复用现有 `resolvo`）实测净增量仍有 **+1.56 MB（+13.1%）**，是 AGENTS.md 10% 红线（1.19 MB）的 **1.31 倍**；若签名改用纯 Rust 的 `apple-codesign`，则是 **+2.90 MB（+24.3%）**。完整测量方法、逐组数据、成因拆解与复现命令见 §6.4。

   而这笔开销换来的**唯一**收益是「Homebrew 未安装时也能用 brew 包」。下一节说明为什么这个收益比它看起来更弱。

**osdk 的选择：两侧都走代理路线（即 mise 的 winget 姿态），不自实现任何一侧的产物处理。** 代价是 Homebrew 必须已安装；收益是 osdk 不承担产物重定位、代码签名、artifact 形状适配的任何维护责任。当 Homebrew 未安装时，osdk 的回答是「引导你安装 Homebrew」而非「我替你实现一个」。

#### 3.3.1 那笔开销换来的唯一收益，成立吗

拿到实测数字后需要正面回答这个问题，而不是停在「太贵了」。自实现相对代理路线，收益只有一条：**Homebrew 未安装时也能安装 brew 包**。这条收益有三处减损：

1. **它只在「首次」这一个时间点有价值。** 装过一次之后，代理路线与自实现路线对用户完全等价。而 osdk 在这个时间点本来就有一个成本低得多的答案——引导用户跑官方安装脚本（并可顺带提供镜像加速的安装方式，§5.3）。用 2.73 MB 换一次性的引导步骤，性价比很差。
2. **它并不能让 osdk 摆脱 Homebrew 的生态约束。** 自实现之后，prefix 仍然必须是 `/opt/homebrew`（否则 bottle 失效，§4.1）——mise 正是因此硬编码 canonical prefix 并直接放弃 Intel Mac。也就是说这笔钱买不到「安装目录控制」，而那恰恰是用户最初的诉求。
3. **它把一整类故障搬进 osdk。** mise 的实测教训是：cask artifact 形状覆盖不全、`__MACOSX` zip 会把已装 app 替换成空目录骨架、裸 XAR/pkg 被误判。代理路线下这些都是 Homebrew 的 bug，自实现之后它们就是 osdk 的 bug。

**反过来，代理路线还有一个自实现拿不到的好处**：brew 装在哪 osdk 就用哪，Intel Mac、自定义 prefix、Linuxbrew 全都自然支持——而 mise 在这三种情形下都直接报「不可用」。

**结论：即使体积数字比预想小，这个收益也不足以支撑。** 实测的最便宜可行组合仍是 +1.56 MB（红线的 1.31 倍），而这三处减损与体积无关、不会随数字变化。§3.3 的代理路线结论在有了数字之后**依然站得住**。

> **一处曾被写进本节、现已被推翻的论据，保留在此以免重蹈。** 本文档上一版在此处写过：「省掉 `oci-client` 之后 `apple-codesign` 仍超红线，除非改用 shell out 到系统 `codesign`（实测 0 KB）」。**后半句的前提不成立**：`codesign` 需要 Xcode Command Line Tools，而**恰恰是 macOS arm64 这个唯一硬性需要重签名的平台**，Homebrew 专门改用了 ruby-macho 的纯实现来避开它（§2.1）。因此「shell out 归零」不是一个无代价的选项，它把 CLT 变成了硬前置。详见 §6.4.7。


来源：https://mise.jdx.dev/bootstrap/packages/brew.html ，https://mise.jdx.dev/bootstrap/packages/winget.html ，https://github.com/jdx/mise/discussions/11058 ，https://github.com/jdx/mise/discussions/12367

### 3.4 应当避开：mise 明确不做或做不到的事，osdk 也不必做

- **Intel Mac**：mise 硬编码 canonical prefix 且「Intel macs are not supported」，维护者在 discussion #10968 直接回复「**I'm not going to add support for intel macs**」。osdk 走代理路线天然没有这个问题——brew 装在哪 osdk 就用哪。**这是代理路线相对自实现的一个实打实的优势。**
- **winget 的声明式移除 / import / prune**：mise 首版明确不支持。osdk 同样不做（§8.2 只支持 `present` 语义）。
- **跨平台包名映射**：mise 不做，osdk 也不做（§2.3）。

来源：https://github.com/jdx/mise/discussions/10968 ，https://mise.jdx.dev/bootstrap/packages/winget.html

---

## 4. 安装目录控制：诚实结论

用户明确关心这一点，并预感「homebrew 可能无法控制」。核查结论：**预感正确，且 winget 侧同样比预期更受限。**

### 4.1 Homebrew：做不到，且不应尝试

**prefix 是固定的**，官方文档化的默认值只有三个：`/opt/homebrew`（macOS Apple Silicon）、`/usr/local`（macOS Intel）、`/home/linuxbrew/.linuxbrew`（Linux）。

**装到别处的代价是 bottle 失效。** 官方 FAQ 列出 bottle 被跳过的四个条件，其中之一就是「Homebrew 安装在非默认 prefix（尽管**某些** bottle 支持非默认 prefix）」。后果是退化为源码构建，而官方对源码构建的定性是「耗时很长、容易失败、**且不受支持**」，并以一句罕见的措辞收尾：「Do yourself a favour and install to the default prefix so that you can use our pre-built binary packages. **Pick another prefix at your peril!**」

**Support Tiers 把这件事量化了**：非默认 prefix 属于 Tier 2「Homebrew installed outside the default prefix, requiring source builds for official packages」，某些情形落入 Tier 3。Tier 3 的后果是「运行时会打印**响亮的配置警告**」「CI 覆盖不可用，bottle 极少构建或发布」「只影响这些配置的 issue 可能不予回复」「功能可能为了受支持配置的利益而**故意退化**」。社区实证：维护者对非默认 prefix 的 Linux 安装直接回复「Your brew doctor output correctly identifies this as Tier 3. …… **Please reinstall Homebrew in the default prefix.**」

**几个常被误用的环境变量，逐一澄清**：

| 变量 | 真实语义 | 能否用于控制安装位置 |
| --- | --- | --- |
| `HOMEBREW_PREFIX` | 是 `brew shellenv` 的**输出**（「变量 `$HOMEBREW_PREFIX`、`$HOMEBREW_CELLAR` 和 `$HOMEBREW_REPOSITORY` 也被导出，以避免多次查询它们」），**不在 `man brew` 的可配置 ENVIRONMENT 列表中** | **否。** 它是查询结果，不是配置输入 |
| `HOMEBREW_CELLAR` | 同上，`brew --cellar` 默认为 `$(brew --prefix)/Cellar` | **否** |
| `HOMEBREW_CASK_OPTS` | 「Append these options to all `cask` commands. All `--*dir` options、`--language`、`--require-sha` 和 `--no-binaries` are supported」 | **仅 cask 产物。** 可以把 app 装到 `~/Applications`，但**对 formula 与 Cellar 完全无效** |
| `HOMEBREW_NO_RELOCATE_BUILD_PREFIX` | 「若设置，安装时不重定位为不同 prefix 构建的 bottle。Homebrew 将改为从源码构建」 | **否**，它只是禁用重定位 |

> **待验证 V-1**：`HOMEBREW_PREFIX` 作为**输入**（在未安装状态下预设以决定安装位置）是否被安装脚本读取。官方 Installation 页只文档化了 `HOMEBREW_BREW_GIT_REMOTE`、`HOMEBREW_CORE_GIT_REMOTE`、`HOMEBREW_NO_INSTALL_FROM_API`、`NONINTERACTIVE`、`HOMEBREW_PKG_USER`。
> 验证方法：阅读 `https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh` 检索 `HOMEBREW_PREFIX` 的赋值逻辑。
> **即使验证为「可以」，本设计也不建议使用**——因为代价是把用户推进 Tier 2/3。

**osdk 的结论：不提供 Homebrew 安装目录控制，并在文档中明确解释为什么。** 唯一提供的相关能力是**读取并报告**当前 prefix（`brew --prefix`），以及在 doctor 中提示用户当前是否处于非默认 prefix（因为那会影响 osdk 对「为什么这个包装得这么慢」的诊断能力）。cask 的 `--appdir` 可以作为**可选的透传配置项**暴露，但必须在文档中写明它只影响 cask。

来源：https://docs.brew.sh/FAQ ，https://docs.brew.sh/Support-Tiers ，https://docs.brew.sh/Manpage ，https://docs.brew.sh/Installation ，https://github.com/orgs/Homebrew/discussions/6976

### 4.2 winget：只有 portable 类型真正可控

`--location` 的官方措辞就是有条件的：「Location to install to (**if supported**)」。按 installer 类型分解：

| Installer 类型 | `--location` 是否生效 | 依据 |
| --- | --- | --- |
| **PORTABLE** | ✅ **真正可控**，且有完整文档化的目录模型 | settings.json 的 `installBehavior.portablePackageUserRoot`（默认 `%LOCALAPPDATA%/Microsoft/WinGet/Packages/`）与 `portablePackageMachineRoot`（默认 `%PROGRAMFILES%/WinGet/Packages/`），**必须是绝对路径** |
| **MSI / WIX / BURN** | ⚠️ **部分生效，不可靠** | winget 把 `--location` 翻译成 `TARGETDIR`，但**很多 MSI 实际需要 `INSTALLDIR`**。issue #1857（**至今 Open**）给出可复现清单：`Oracle.JDK.17`、`StrawberryPerl.StrawberryPerl`、`ChristianHohnstadt.xca` 用 `-l` 均无效，只有 `--override "/passive INSTALLDIR=..."` 生效 |
| **EXE（含 INNO/NULLSOFT）** | ⚠️ **完全取决于安装器自身** | 官方对同类问题的判断：「基于 EXE 的安装器行为不一定是确定性的，有时根本没有指定参数的能力」 |
| **MSIX/APPX** | ❌ **不适用** | 目录由 Windows 应用模型管理（`C:\Program Files\WindowsApps`） |

**两个常见误解需要纠正**：

1. **不存在 `WINGET_*` 安装目录环境变量。** portable 根目录只能通过 settings.json 键或 `--location` 控制。`winget --info` 会**显示**这些目录（`Portable Package Root (User)` 等），但那是展示而非环境变量入口。
2. **`defaultInstallRoot` 不是全局安装前缀。** 官方原文：「该设置**仅在包 manifest 包含 `InstallLocationRequired` 时才被使用**」。discussion #2850「defaultInstallRoot setting does not take effect」正是这个误解的产物。

另有一个实操陷阱：settings.json 的路径类设置**不支持环境变量展开**（issue #6360 报告把 `portablePackageUserRoot` 设为 `%LOCALAPPDATA%/...` 会被判定为无效）。

> **待验证 V-3**：winget 对 MSIX 包传入 `--location` 时是静默忽略还是返回 `UNSUPPORTED_ARGUMENT`（-1978335143）。
> 验证方法：实机 `winget install --id <纯 MSIX 包> -l C:\tmp\x -e`，记录退出码并查 verbose 日志。

**osdk 的结论：`--location` 仅在用户显式提供且包为 portable 类型时透传，其余情形拒绝该参数并给出明确解释**，而不是传下去让它静默失效。理由是「传了但没生效」比「明确告诉你不支持」危害大得多——用户会以为包装在了指定位置。

来源：https://learn.microsoft.com/en-us/windows/package-manager/winget/install ，https://github.com/microsoft/winget-cli/blob/master/doc/Settings.md ，https://github.com/microsoft/winget-cli/issues/1857 ，https://github.com/microsoft/winget-cli/issues/6360 ，https://github.com/microsoft/winget-cli/discussions/2850 ，https://learn.microsoft.com/en-us/windows/package-manager/winget/troubleshooting

### 4.3 那么想要目录控制的用户该怎么办

这是设计必须正面回答的问题，否则等于把用户的原始诉求丢掉了。**答案是：这个需求不该由系统包管理器满足，osdk 本来就有更好的通道。**

| 用户的真实意图 | 正确通道 |
| --- | --- |
| 「我想把工具装到 D 盘，不占系统盘」 | osdk 自有 backend + `OSDK_DATA_DIR`。osdk 的 `installs/` 本来就完全受控 |
| 「我要多版本并存并按项目切换」 | osdk 自有 backend + shim。系统包管理器**结构上做不到**（通常只允许一个版本） |
| 「我要可复现的、锁得住的安装」 | osdk 自有 backend + `osdk.lock`（§10 解释为什么系统包进不了 lock） |
| 「我只是要装个 ffmpeg / libssl 给别的东西用」 | **这才是系统包管理器的场景**，且此时用户通常并不在意装在哪 |

设计上应当把这条分流写进 CLI 的错误提示里：当用户对一个非 portable 的 winget 包请求 `--location` 时，除了说「不支持」，还应提示「如果你需要目录可控的安装，考虑 `osdk install <tool>`」——前提是该工具确有对应 backend。

---

## 5. 镜像加速与探测

### 5.1 两侧镜像的结构性差异（关键）

| | winget | Homebrew |
| --- | --- | --- |
| 镜像的对象 | **只有 manifest / 索引** | **元数据 API + 二进制产物（bottle）** |
| 为什么 | source 类型 `Microsoft.PreIndexed.Package` 的实体是一个内含 SQLite 索引的 MSIX（`source.msix` / `source2.msix`），静态镜像即可。但 manifest 里的 `InstallerUrl` **指向上游原始下载地址**（如 `https://nodejs.org/dist/...`） | bottle 集中托管在 `ghcr.io/v2/homebrew/core`，可被 `HOMEBREW_BOTTLE_DOMAIN` / `HOMEBREW_ARTIFACT_DOMAIN` 整体替换 |
| 实际加速效果 | **只加速「找包」，不加速「下包」** | **两段都能加速** |

**这个差异必须诚实地呈现给用户**，否则换了 winget 源之后下载依然慢，用户会认为 osdk 的镜像功能是坏的。osdk 的 `doctor` / `mirrors` 输出应当显式区分「索引加速」与「产物加速」两类能力。

来源：https://raw.githubusercontent.com/microsoft/winget-cli/master/doc/troubleshooting/README.md ，https://github.com/microsoft/winget-pkgs/blob/master/manifests/o/OpenJS/NodeJS/25.1.0/OpenJS.NodeJS.installer.yaml ，https://docs.brew.sh/Manpage

### 5.2 winget 镜像现状（与流行印象相反：存在）

| 镜像站 | 地址 | 状态 |
| --- | --- | --- |
| **USTC** | `https://mirrors.ustc.edu.cn/winget-source` | ✅ **有官方帮助页**，同步时间戳 2026-09-03。区分版本给出命令：winget **≥ 1.8** 需 `--trust-level trusted`，≤ 1.7 不需要 |
| USTC winget-fonts | `https://mirrors.ustc.edu.cn/winget-fonts` | ⚠️ 镜像站自身标注「截至 2026 年 1 月，WinGet 字体管理功能尚未达到普遍可用状态……本镜像当前仅供测试验证」 |
| **NJU** | `https://mirrors.nju.edu.cn/winget-source/` | ⚠️ 路径实测 200（`source.msix` 可取），但帮助页返回「No document available」 |
| **华为云** | `https://mirrors.huaweicloud.com/winget-source/` | ⚠️ 路径实测 200，**待验证 V-4**：是否有官方帮助页与推荐命令 |
| TUNA | — | ❌ 实测 404，**不提供** |
| 腾讯云 | — | ❌ 广泛流传的 `mirrors.cloud.tencent.com/winget-pkgs/cache` 实测 **404** |

> ⚠️ **一条必须排除的污染信息**：流传较广的腾讯云地址出自一篇 CSDN 文章，同文还给出了 `winget source pin --name tencent --priority 1` 这样的命令——**`winget source pin` 并不存在**于官方 source 子命令列表（`add`/`edit`/`list`/`update`/`remove`/`reset`/`export`）中。实现时不得采信该来源。

**换源的伴生代价**（都必须在 osdk 的提示中讲清楚）：

1. `source add` / `remove` / `reset` **都需要管理员权限**。
2. winget ≥ 1.8 需 `--trust-level trusted`。默认 `winget` source 的 Trust Level 显示为 `Trusted|StoreOrigin`——**镜像源无法获得 `StoreOrigin` 这一部分**。
3. 组策略 `EnableAllowedSources` 可完全禁止添加额外 source；被阻断时退出码为 `BLOCKED_BY_POLICY`（-1978335174）。**企业环境下换源可能根本不可行**，osdk 必须能识别这个状态并如实报告，而不是反复重试。

> **待验证 V-5**：换用镜像 source 后，除 `--trust-level` 外是否还有签名/证书校验差异（`PINNED_CERTIFICATE_MISMATCH`、`SOURCE_DATA_INTEGRITY_FAILURE`、`SOURCE_NOT_SECURE` 的触发条件）。
> 验证方法：实机添加镜像源后 `winget source list --name winget` 对比 Trust Level 字段，并跑 `winget source update --verbose-logs` 检查完整性校验日志。

来源：https://mirrors.ustc.edu.cn/help/winget-source.html ，https://mirrors.ustc.edu.cn/help/winget-fonts.html ，https://learn.microsoft.com/en-us/windows/package-manager/winget/source ，https://github.com/microsoft/winget-cli/blob/master/doc/admx/DesktopAppInstaller.admx

### 5.3 Homebrew 镜像现状（4.x 之后配置方式已变）

**最重要的一条：4.0.0（2023-02-16）起 API 优先成为默认，镜像配置的重心从「换 git remote」转移到「设置 `HOMEBREW_API_DOMAIN`」。** 大量 2020–2022 年的教程只教 `git remote set-url`，在 4.x 默认配置下**对元数据获取路径不再生效**——因为默认根本不读那个本地 clone。

TUNA 帮助页的表述最清楚：「自 brew 4.0.0 起 `HOMEBREW_INSTALL_FROM_API` 会成为默认行为，无需设置。**大部分用户无需再克隆 `homebrew-core` 仓库，故无需设置 `HOMEBREW_CORE_GIT_REMOTE` 环境变量；但若需要运行 `brew` 的开发命令或者 `brew` 安装在非官方支持的默认 prefix 位置，则仍需设置**。」

| 镜像站 | API 域名 | 产物域名 | 实测（2026-09-12） |
| --- | --- | --- | --- |
| **TUNA** | `https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles/api` | `https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles` | `formula.jws.json` / `cask.jws.json` **200**；brew.git 与 homebrew-cask.git **200** |
| **USTC** | `https://mirrors.ustc.edu.cn/homebrew-bottles/api` | legacy flat: `HOMEBREW_BOTTLE_DOMAIN`；OCI: `HOMEBREW_ARTIFACT_DOMAIN`，均为 `https://mirrors.ustc.edu.cn/homebrew-bottles` | `formula.jws.json` **200**；brew.git **200** |
| **阿里云** | `https://mirrors.aliyun.com/homebrew-bottles/api`（另有 `/homebrew/homebrew-bottles/api`） | `https://mirrors.aliyun.com/homebrew/homebrew-bottles` | 两个 API 路径均 **200**；homebrew-core.git **200** |

**几条实现时必须知道的细节**：

- **USTC 提供两套互斥配置**：legacy flat（`HOMEBREW_BOTTLE_DOMAIN`，镜像站注明「未来 homebrew 可能会停止支持此项配置」）与 OCI（`HOMEBREW_ARTIFACT_DOMAIN`，「目前测试中」）。镜像站**明确要求**：「如果之前使用过 `HOMEBREW_BOTTLE_DOMAIN`，请先移除相关的配置」。**osdk 生成配置建议时必须保证二者不同时出现。**
- **`HOMEBREW_API_DOMAIN` 有官方 fallback，`HOMEBREW_ARTIFACT_DOMAIN` 没有**：前者「若该 URL 的元数据文件暂时不可用，将使用默认 API 域名作为后备镜像」；后者「若对 `$HOMEBREW_ARTIFACT_DOMAIN` 的请求失败，Homebrew 会报错而不是尝试其他/默认 URL」。**这个差异直接决定 osdk 的回退策略在哪一层生效**（§5.4）。
- **TUNA 不对欧盟用户提供服务**（其帮助页页脚明示）。
- **阿里云帮助页内容已部分过时**：仍在教 `homebrew/cask-fonts`、`cask-versions`、`command-not-found`、`services` 这些**已弃用/已合并**的 tap。TUNA 记录：「截止到 brew 4.6.12，`homebrew-{services,bundle,homebrew-command-not-found}` 均已被弃用，所有 tap 合并至 `brew` 仓库」。
- **容易漏配的一项**：`HOMEBREW_PIP_INDEX_URL`（formula 内 Python resource 的独立下载通道，默认 `https://pypi.org/simple`）。

> **待验证 V-6 / V-7**：阿里云两个 API 路径的同步新鲜度是否一致、是否其一为遗留别名；以及阿里云镜像是否仍在活跃同步（帮助页含已弃用 tap，暗示维护滞后）。
> 验证方法：分别取回两处 `formula.jws.json` 比对 `Last-Modified`/内容哈希；对比同一 formula 的 stable 版本号与 `https://formulae.brew.sh/api/formula/<name>.json` 的差值。

来源：https://mirrors.tuna.tsinghua.edu.cn/help/homebrew/ ，https://mirrors.tuna.tsinghua.edu.cn/help/homebrew-bottles/ ，https://mirrors.ustc.edu.cn/help/homebrew-bottles.html ，https://mirrors.ustc.edu.cn/help/brew.git.html ，https://developer.aliyun.com/mirror/homebrew ，https://docs.brew.sh/Manpage

### 5.4 探测机制：复用 `source/`，不新建一套

仓库里已经有完整的镜像探测基础设施，**不应重复实现**：

- `source/mod.rs`：`Source`（含 `index_url` / `download_url` 分离，正好对应 Homebrew 的 API 域名与 bottle 域名）、`SourceKind`（Official/Mirror/Custom）、`Selection`（Auto/Pinned/Ordered）、`ProbeResult`（throughput + ttfb_ms + ok + measured_at）、`ProbeCache`、`candidate_fingerprint`（对候选集做指纹，含 header 值哈希以免泄密）。
- `source/select.rs`：`active_source`、`ranked_source_list`、`ranked_source_candidates`。

**关键发现：已经存在为「非 Backend 下载者」准备的入口**，正是为本设计这种情形铺的路：

```rust
/// [`effective_sources`] for a downloader that is not a [`Backend`].
///
/// osdk's own release download is the one such case. …… Making it a backend
/// would add another `dyn Backend` to the registry -- and so to the shim's
/// vtables -- for something no tool request can ever name.
pub fn effective_sources_for(ctx: &Ctx, tool: &str, sources: Vec<Source>) -> Vec<Source>

/// `probe_url` plays the part of [`Backend::probe_url`] …… Everything else --
/// the pin, the probe cache and its fingerprint, offline behaviour -- is shared
/// with the backend path, so a non-backend downloader cannot drift into its own
/// mirror policy.
pub async fn ranked_source_candidates_for(...)
```

**系统包管理器的镜像探测应当直接走这两个函数**，用伪工具 id（如 `pkg:winget-source`、`pkg:brew-api`、`pkg:brew-bottle`）作为 `tool` 参数。这样自动获得：pin 处理、探测缓存与 TTL、`--refresh-sources` 语义、offline 行为、`--source` 一次性覆盖——且注释明确说明了这样做的目的就是「让非 backend 下载者无法漂移出自己的镜像策略」。

**两侧探测对象不同**：

| | 探测什么 | 探测 URL 选取 |
| --- | --- | --- |
| winget | manifest 索引源 | 各镜像的 `source.msix`（或对其发 HEAD/Range 请求测吞吐） |
| Homebrew（API） | 元数据 API | `<api_domain>/formula.jws.json`——实测各镜像均返回 200，是理想的探测目标（体积适中、结构与官方一致） |
| Homebrew（bottle） | 二进制产物域名 | 需单独探测，因为 API 域名与产物域名可以分属不同镜像 |

**回退策略要区分两层**，这是 §5.3 提到的 fallback 差异的直接后果：

- `HOMEBREW_API_DOMAIN` 有 Homebrew 自带的官方 fallback，osdk 的排序只影响「优先试哪个」，失败有兜底。
- `HOMEBREW_ARTIFACT_DOMAIN` **没有** fallback，失败即报错。osdk 在推荐这个变量时应当更保守（例如要求探测成功率达标才推荐），并在 doctor 中明确提示这一点。

### 5.5 镜像配置由谁写入：一个重要的边界

**osdk 不直接改写用户的 shell profile 或 winget settings.json。** 这与 `container/mirror.rs` 已确立的姿态一致——该模块的文档注释写着：「The planners consume already validated osdk policy plus typed discovery. They return versioned fingerprints and in-memory candidate bytes, but **never write, restart, recreate, or elevate a native runtime**.」写入走独立的 `container/apply.rs`，且需要用户确认一个精确的、带指纹的预览（`--execute --accept-preview <SHA256_ID>`）。

**但这条边界只适用于「写入宿主全局配置」，不适用于「osdk 自己这一次调用用哪个源」。** 两者必须分开，否则会得出「镜像加速必须每次手动操作」这一错误结论。实际上有三层，提权与副作用逐层升级：

| 层 | 作用范围 | 是否改宿主状态 | 是否需要提权 | 姿态 |
| --- | --- | --- | --- | --- |
| **L1 osdk 自有下载源** | 只影响 osdk 的 store | 否 | 否 | **已是现状**，`source::select` 的 `Selection::Auto` 自动探测并按实测速度排序 |
| **L2 osdk 调用包管理器时的一次性源选择** | 只影响 osdk 发起的那一次调用 | 否 | 否 | **应当自动**，见下 |
| **L3 包管理器的全局源配置** | 影响这台机器上所有 winget/brew 使用者 | 是 | **是** | 首次需用户确认，之后可自动维护 |

**L2 是用户预期中「安装依赖时自动检测并设置镜像」的正确落点。** winget 的 `install` / `search` / `show` 均支持 `--source <name>`，指定某个已注册的源用于本次调用。osdk 因此可以在自己调用 winget 时，自动选用实测最快的已注册源，而**完全不触碰全局配置**：用户手敲 `winget install` 时行为不变，不需要管理员权限，也不会影响机器上其他 winget 使用者。探测结果本就有缓存与 TTL（§5.4 复用的 `source::select` 提供），因此这不会给每次安装都加上一轮测速。

**L3 才是需要确认的那一层，原因具体而非教条**：

- `winget source add` 写的是**机器级配置**，需要管理员权限。
- 它影响的不只是 osdk：此后所有人、所有工具调用 winget 都会看到这个源。
- **换源会削弱信任链**：镜像源无法获得默认源的 `StoreOrigin` 信任标记，只能是 `Trusted`（§9）。让这件事静默发生是不可接受的。

因此 L3 采用**「首次确认一次，之后自动维护」**：第一次需要注册镜像源时，osdk 打印将要执行的确切命令与上述信任链影响，由用户确认一次；此后 osdk 可自动在已确认的源之间按实测速度选择，不再打扰。这既给到了自动化，又保证那一次真正有副作用的操作是用户知情的。

**这同时满足了用户的既有偏好**——「源与配置一律通过 osdk 自身命令管理，不手写配置或代理源」。用户不需要自己去 `~/.zshrc` 里加 `export HOMEBREW_API_DOMAIN=...`，也不需要自己记 `winget source add` 的参数，但 osdk 也不会背着用户改机器级配置。

**关于 `plan` 是否单独成命令**：`container/` 把 planner 与 apply 分成两个模块是实现层的分层，不必然要求命令面也分成两条命令。预览的价值是「让用户在确认前看到要改什么」，而这可以是 `apply --dry-run` 的职责。命令面因此收敛为 `test` 与 `apply`（后者带 `--dry-run`），少一个用户需要记住的命令，且预览与执行共用同一套计划生成代码，不会出现「预览过的计划和实际执行的不一致」这类分叉。

对 Homebrew 有一个额外的落点选择：除了 shell profile，Homebrew 支持 `brew.env` 配置文件（`${HOMEBREW_PREFIX}/etc/homebrew/brew.env` 或 `$XDG_CONFIG_HOME/homebrew/brew.env`），**这比改 shell profile 干净得多**（不污染交互式 shell、对所有 brew 调用一致生效）。注意约束：这些文件**不支持 shell 变量展开或命令执行**，且「环境变量必须有值才会被检测到」。**建议优先写 `brew.env`。**

来源：https://docs.brew.sh/Manpage ，`crates/osdk-core/src/container/mirror.rs`、`container/apply.rs`

---

## 6. 架构方案：独立子系统，不扩展 `Backend`

### 6.1 为什么不能扩展 `Backend` trait

这是本设计中约束最硬的一条，且项目 AGENTS.md 已经把教训写在前面了：

> `Registry::new()` 会把全部 13 个 backend 实例化成 `Arc<dyn Backend>`，于是每个方法都进入 vtable，链接器无法证明它不可达，也就无法裁掉。这条曾让 shim 白背 5.15 MB（实测：同样 13 个 backend，`dyn` 分发 7.02 MB，静态分发 1.83 MB）。

代码层面确认（`backend/registry.rs`）：

```rust
pub struct Registry {
    backends: Vec<Arc<dyn Backend>>,
    ...
}
```

而 shim 确实构造 Registry（`osdk-shim/src/main.rs` 导入 `osdk_core::backend::registry::Registry`）。因此：

- 若在 `Backend` 上新增 `fn system_package_status(...)` 之类的方法，**即使 shim 永不调用，它也会进 vtable**。
- 若把这些方法门控在 `#[cfg(feature = "install")]` 之后，shim 侧确实不付代价——但这样做在语义上是错的：系统包管理能力与「install 某个 SDK 版本」不是同一件事，硬塞进同一个 feature 会让这个 feature 的含义进一步模糊。
- 更根本的是：**系统包管理器根本不满足 `Backend` 的契约**。`Backend` 要求 `bin_paths`、`bin_names`、`list_installed`（按 `installs/<tool>/<version>/` 布局扫描 `.osdk-complete` 标记）、`default_sources`、`probe_url`。系统包装在宿主 prefix 下，没有 osdk 的安装目录、没有 `.osdk-complete`、不参与 shim。硬实现这些方法只能全部返回空值或错误——这是典型的「接口不匹配」信号。

### 6.2 已有的正确先例：`container/`

仓库里已经有一个管理宿主设施的子系统，它的处理方式就是答案。`container/mod.rs` 的文档注释：

> Native container-runtime contracts. This native-first layer deliberately contains no OCI store. It provides stable diagnostic, redaction, and injectable process boundaries plus one stale-checked atomic path for explicitly confirmed mirror configuration.

`container/` 管理 Docker Engine / containerd / BuildKit——这些是宿主上已存在、osdk 不拥有的设施。它**完全不实现 `Backend` trait**，而是自成一套：`docker.rs` / `containerd.rs` / `buildkit.rs`（各运行时的发现与适配）、`runtime.rs`（`RuntimeAdapter` / `ProbeCommand` / `ForegroundCommand` 抽象）、`mirror.rs`（只读镜像计划）、`apply.rs`（确认后的原子写入）、`redact.rs`（命令与 URL 脱敏）、`report.rs`（schema 化的诊断报告）、`plan.rs`（指纹与计划类型）。

**系统包管理器与容器运行时是同一类东西。** 应当采用同构的模块布局：

```
crates/osdk-core/src/syspkg/
    mod.rs          // 子系统契约与 re-export（与 container/mod.rs 同构）
    manager.rs      // PackageManagerAdapter trait（注意：不是 Backend）
    winget.rs       // winget 发现、调用、输出解析、退出码映射
    brew.rs         // brew 发现、调用、--json=v2 解析
    discovery.rs    // 可用性探测（含未安装/版本过旧/未注册的区分）
    mirror.rs       // 镜像候选、探测（复用 source::select 的 *_for 入口）、只读计划
    apply.rs        // 确认后的镜像配置写入 / 包安装执行
    plan.rs         // 计划类型与指纹
    report.rs       // schema 化诊断与 status 报告
    redact.rs       // 复用或参照 container::redact
```

`PackageManagerAdapter` 是一个**小得多**的 trait，且只在 CLI 侧被实例化——它的 vtable 完全不进 shim，因为 shim 根本不引用 `syspkg` 模块。

### 6.3 体积影响评估

**先更正一处基线。** AGENTS.md 记录的基线是 `osdk` ≈ 8.9 MB、`osdk-shim` ≈ 3.5 MB。2026-09-12 在本仓库实测（`cargo 1.98.0` / `rustc 1.98.0`，`x86_64-pc-windows-msvc`，两次独立调用）：

| 二进制 | AGENTS.md 记录 | 本次实测 | 差异 |
| --- | --- | --- | --- |
| `osdk.exe` | ≈ 8.9 MB | **11.92 MB**（12,503,552 B） | +3.0 MB（代码库自该文撰写以来已增长） |
| `osdk-shim.exe` | ≈ 3.5 MB | **3.50 MB**（3,671,552 B） | 一致 |

下文所有百分比以**实测的 11.92 MB** 为分母；AGENTS.md 的 10% 红线对应 **1.19 MB**。（顺带建议：AGENTS.md 中的 8.9 MB 已过时，宜在某次提交中一并更正。）

| 方案 | osdk 影响 | osdk-shim 影响 | 评价 |
| --- | --- | --- | --- |
| **扩展 `Backend` trait**（不门控） | 小幅增长 | **每个新方法进 13+ 个 vtable**，按历史数据外推是数 MB 级 | ❌ 直接违反 AGENTS.md 的教训 |
| 扩展 `Backend` trait + `install` feature 门控 | 小幅增长 | 0 | ⚠️ 体积上可行，但语义错误（§6.1） |
| **独立 `syspkg` 子系统**（本方案） | 主要是解析与计划逻辑，**无新增重依赖** | **0**（shim 不引用该模块） | ✅ |
| mise 式自实现 bottle pour | **实测最便宜可行组合 +1.56 MB（+13.1%），为红线的 1.31 倍**；纯 Rust 签名则 +2.90 MB（+24.3%） | 0（可门控） | ❌ 见 §3.3 与 §6.4 |

**依赖评估：本方案预期不引入任何新的重依赖。** 需要的能力仓库里都有：

- 子进程调用与超时 → `process.rs`（已有 `crates/osdk-core/src/process.rs`，25.6 KB）
- JSON 解析 → `serde_json`（已有）
- HTTP 探测 → `http/mod.rs` + `reqwest`（已有）
- 可执行文件发现 → `which`（已有）
- 配置读写 → `toml_edit`（已有）

因此**不需要在 `Cargo.toml` 的 `install = [...]` 里新增任何 `dep:`**。如果实现过程中发现需要新依赖，必须回头重新评估——那是设计出了偏差的信号。

**验证方法**（遵循 AGENTS.md 的要求，两次独立调用）：

```powershell
$env:CARGO_TARGET_DIR="target\size-check"
cargo build --release -p osdk-cli
cargo build --release -p osdk-shim
```

预期：`osdk-shim` 相对 3.50 MB 基线**零增长**（若有增长，说明 `syspkg` 意外进入了 shim 的依赖图，必须排查）；`osdk` 相对 11.92 MB 基线增长应远低于 1.19 MB 的红线。

**shim 零增量不是假设，已实测验证。** 当前 `osdk-shim` 的依赖图里，`install` feature 门控的重依赖**一个都没有**：

| crate | 在 `osdk-cli` 图中 | 在 `osdk-shim` 图中 |
| --- | --- | --- |
| `sigstore` / `rattler` / `resolvo` / `sevenz-rust2` / `x509-cert` | 有 | **0 处** |

依赖图规模：`cargo tree -e normal -p osdk-cli` = **1550 行**，`-p osdk-shim` = **427 行**。这证明 `optional = true` + `install = ["dep:..."]` 的门控机制确实有效，`syspkg` 只要不被 shim 引用，就同样是 0 增量。

### 6.4 自实现 Homebrew 的体积实测（方法与数据）

本节回答「如果 Homebrew 侧也做成 mise 那样的自解析，要新增多少依赖、多少空间」。**所有数字为实测，不是估算。**

#### 6.4.1 测量方法

在仓库之外的临时目录建独立 probe crate（不污染 workspace，不改 `crates/` 下任何代码），复制 osdk `[profile.release]` 的**全部**设置（`opt-level = "z"`、`lto = "fat"`、`codegen-units = 1`、`strip = true`、`panic = "abort"`，以及 sha2/sha1/blake3/digest/block-buffer/cpufeatures 六条 per-package `opt-level = 3` override）。

关键设计——**probe 的基线里预先放入 osdk 今天已经有的 crate**（`tokio`、`reqwest 0.13`、`serde`、`serde_json`、`sha2`、`flate2`、`tar`、`zstd`、`hex`、`semver`）并在代码中逐一调用。这样测出的是**真增量**，而不是候选 crate 的绝对体积：凡是与 osdk 已有依赖共享的部分都不会被重复计入。

复现命令（PowerShell 7；`<probe>` 为临时目录）：

```powershell
# 与用户真实 SDK 状态隔离（AGENTS.md 要求）
$env:CARGO_HOME = "<probe>\cargo-home"
Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
# 本仓库 .cargo/config.toml 的 rsproxy 镜像需一并复制到 <probe>\.cargo\
cargo generate-lockfile        # 必须先解析；否则 build 会长时间阻塞在网络解析上
cargo build --release
cargo tree -e normal           # 依赖图行数
cargo tree --duplicates        # 重复 crate 数
(Get-Item "<probe>\target\release\depprobe.exe").Length
```

#### 6.4.2 一个必须讲清的方法论陷阱

第一轮测量得到的数字**全部偏小一到两个数量级**，原因是 `lto = "fat"` 会把没有被真正调用到的代码整体裁掉。例如 `apple-codesign` 编译产物有 69 MB（rlib 口径），但只要 probe 里仅仅 `Default::default()` 一下，链接进最终二进制的只有 **15 KB**；`resolvo` 更极端——探针写成 `size_of::<T>()` 时常量折叠，最终二进制与基线**逐字节相同**。

因此第二轮改为**真正驱动各 crate 的主路径**（OCI：解析 reference → 拉 manifest → 逐 layer 拉 blob → list tags；Mach-O/ELF：遍历 section 与 symbol、构造并 `write()` 出新对象；签名：`MachOSigner::new` → `write_signed_binary`），且输入取自运行时（命令行参数与磁盘文件），使 LTO 无法证明其不可达。**两轮的差距本身就是结论的一部分**：

| 分组 | 浅探针（会被 LTO 裁掉） | 深探针（真实调用路径） |
| --- | --- | --- |
| `apple-codesign` | +15 KB | **+1,680 KB** |
| `oci-client` | +1,497 KB | **+2,036 KB** |
| `resolvo` | 0 KB（逐字节相同） | — |

还设了一个**控制组**：一个不新增任何依赖、只用 `std::process::Command` 的配置，实测 **+74 KB**。这是「探针代码自身」的体积，下表的「净增量」列已扣除它。基线配置连测两次，**结果逐字节一致**（2,217,984 B），说明构建确定、不存在随机噪声。

#### 6.4.3 逐组实测结果

probe 基线 2,217,984 B；控制组 2,294,272 B。

| # | 能力 | 候选 crate | 二进制 | 相对基线 | **净增量** | 依赖图 | 重复 crate |
| --- | --- | --- | --- | --- | --- | --- | --- |
| — | 基线（osdk 已有依赖） | — | 2,217,984 | 0 | — | 272 | 2 |
| — | **控制组**（0 新依赖） | — | 2,294,272 | +74 KB | **0 KB** | 272 | 2 |
| 1 | **OCI registry 客户端**（旧测，版本失配） | `oci-client 0.15` | 4,302,848 | +2,036 KB | +1.92 MB | 409 | 4 |
| 1' | **OCI registry 客户端**（版本对齐后） | `oci-client 0.17` | 3,790,336 | +1,535 KB | **+1.43 MB** | 387 | 8 |
| 2 | **Mach-O load command 重写** | `object 0.36`（macho, read+write） | 2,284,544 | +65 KB | **≈ 0**（低于控制组） | 278 | 3 |
| 3a | **代码签名（纯 Rust）** | `apple-codesign 0.29` | 3,938,304 | +1,680 KB | **+1.57 MB** | 836 | 13 |
| 3b | **代码签名（shell out）** | 无（`std::process`） | 2,294,272 | +74 KB | **0 KB**（但前提不成立，见 §6.4.7） | 272 | 2 |
| 4 | **ELF interpreter/rpath 改写** | `object`（+elf feature） | 2,289,664 | +70 KB | **≈ 0**（低于控制组） | 278 | 3 |
| 4b | 仅 ELF（不含 Mach-O） | `object`（elf only） | 2,283,008 | +64 KB | ≈ 0 | 278 | 3 |
| 5 | **依赖闭包求解** | `resolvo 0.12` | — | — | **0 MB（CLI 侧）** | 310 | 4 |
| 6 | 完整栈（旧测，`oci-client 0.15`） | 全部 | 5,159,424 | +2,872 KB | +2.73 MB | 933 | 14 |
| **F1** | **最便宜的可行组合**：OCI 0.17 + object + **签名 shell out** + resolvo | — | 3,935,232 | +1,633 KB | **+1.56 MB** | 422 | 10 |
| **F2** | 同上但签名用纯 Rust | 含 `apple-codesign` | 5,336,064 | +3,024 KB | **+2.90 MB** | 943 | 21 |

几条需要单独说明的：

- **第 2、4 组几乎不要钱。** `object` 是纯解析/构造库，无重传递依赖（依赖图只从 272 涨到 278，+6 行）。Mach-O 与 ELF 两种格式加在一起也只有 +70 KB，仍低于控制组的 +74 KB——即**淹没在探针代码自身的体积里**。「Mach-O 重写很贵」是直觉误判；真正贵的是签名与 OCI。
- **第 3a vs 3b 的对比在体积上成立，但在可用性上有前提。** 纯 Rust 签名 +1.57 MB、依赖图从 272 → **836 行**（+564）、重复 crate 从 2 → **13 个**；shell out 到系统 `/usr/bin/codesign` 是 **0 KB、0 新依赖**。但 §6.4.7 说明了为什么「0 KB」不等于「免费」。
- **第 5 组对 CLI 是 0。** `resolvo`（SAT 求解器）**已经在 `osdk-cli` 的依赖图里**——经由 `rattler` 的 conda 支持（`resolvo v0.12.1`）。formula 依赖闭包求解可直接复用，无新增。这是 osdk 相对从零开始的一个既有优势。
- **组合小于各组之和。** F1（1.56 MB）小于「OCI 0.17 + object」之和，F2（2.90 MB）小于各组相加，原因是共享传递依赖。**报总量时必须用合并测量值，不能把分组数字相加。**

#### 6.4.4 OCI 增量的成因拆解：版本对齐能省多少

原测的 +1.92 MB 需要拆开看成因。做法是设三组对照，把「第二份 HTTP+TLS 栈」与「OCI 协议本身」分离：

| 配置 | 二进制 | 净增量 | 依赖图 | 重复 crate |
| --- | --- | --- | --- | --- |
| `oci-client 0.15`（钉 reqwest 0.12，与 osdk 的 0.13 冲突） | 4,302,848 | **+1.92 MB** | 409 | 4（含 `reqwest`） |
| `oci-client 0.17`（用 reqwest 0.13，与 osdk 对齐） | 3,790,336 | **+1.43 MB** | 387 | 8（**不含** `reqwest`） |
| 单独引入第二份 `reqwest 0.12`（不带 OCI） | 3,185,152 | +0.85 MB | 313 | 4（含 `reqwest`） |

**拆解结论**：

- **版本对齐省下 0.49 MB**，即原 1.92 MB 中**约 26% 来自重复的 reqwest+TLS 栈**，**约 74%（1.43 MB）是 OCI 客户端自身**（协议实现、manifest/media-type 处理、`olpc-cjson` 规范化 JSON、正则等）。
- 因此「大头是第二份 reqwest 而非 OCI 协议」这个直觉**方向对但份额高估了**：重复栈确实是一笔可消除的浪费，但它只占四分之一，OCI 客户端本体才是主要成本。
- 一个看似矛盾需要解释的现象：对齐后重复 crate 数从 4 **上升**到 8。这不是退步——`reqwest` 从重复列表中**消失了**，新增的 8 项是 `block-buffer`/`cpufeatures`/`crypto-common`/`digest`/`sha2`/`untrusted` 这类**小型哈希与加密原语**的次要版本共存，体积影响远小于两份 HTTP+TLS 栈。**重复 crate 的个数不是体积的良好代理，必须看字节数。**

#### 6.4.5 版本冲突：reqwest 双版本坑——`oci-client` 侧已可消除，`apple-codesign` 侧不能

AGENTS.md 记过 reqwest 长期双版本编译的教训，并给出指导原则：「遇到重复依赖先用 `cargo tree -i` 看是谁引入，若是上游已整体前进，正确做法是跟随而不是钉住。」本次实测的初版复现了同一个模式，但**进一步核查后发现，这条指导原则恰好适用，且上游确实已经前进了**：

```
reqwest v0.12.28
├── apple-codesign v0.29.0
├── apple-xar v0.20.0            (← apple-codesign 的传递依赖)
├── cryptographic-message-syntax v0.27.0
└── oci-client v0.15.0           (← 这一条已可消除)

reqwest v0.13.5
└── depprobe (即 osdk 自己所用的版本)
```

**`oci-client` 已经跟上了。** 查 crates.io 的版本依赖元数据：

| 版本 | 发布日期 | reqwest 要求 |
| --- | --- | --- |
| 0.15.0 | 2025-05-15 | `^0.12.4` |
| **0.16.0** | **2026-01-27** | **`^0.13`** |
| 0.16.1 | 2026-03-05 | `^0.13` |
| 0.17.0 | 2026-05-19 | `^0.13` |

即 **`oci-client` 自 0.16.0 起已转向 reqwest 0.13**，与 osdk 当前版本对齐。上一版文档里「无上游可跟随」的判断**对 `oci-client` 是错的**，已据实修正——0.16 与 0.17 实测重复 crate 列表中都不再出现 `reqwest`（0.16 的 `cargo tree --duplicates` 甚至回落到与基线相同的 2 项）。

**但 `apple-codesign` 侧确实无上游可跟随**：其最新版本仍是 **0.29.0（2024-11-29 发布）**，依赖要求为 reqwest `^0.12`，至今未升。若采用纯 Rust 签名方案，双版本问题依然存在（F2 配置实测重复 crate **21 项**，`reqwest` 仍在其中）。

#### 6.4.6 折算到真实二进制与红线

| 场景 | `osdk` 体积 | 相对 11.92 MB | 红线（1.19 MB / 10%） |
| --- | --- | --- | --- |
| 当前 | 11.92 MB | — | — |
| 自实现，签名用纯 Rust（F2） | **14.83 MB** | **+2.90 MB / +24.3%** | ❌ **超出 2.4 倍** |
| **自实现，最便宜可行组合（F1）** | **13.49 MB** | **+1.56 MB / +13.1%** | ❌ **仍超出 1.31 倍** |
| 再省掉 `oci-client`（手写 OCI 协议） | ≈ 12.0 MB | +0.07 MB | ✅ 但工作量与故障面见 §3.3.1 |
| **本设计（代理路线）** | ≈ 11.92 MB | **≈ 0** | ✅ |

**这是本轮最重要的一个数字变化**：上一版认为「签名 shell out 后约 +1.16 MB，贴着红线」，本轮把各组合完整测出来后是 **+1.56 MB，明确越线 1.31 倍**。上一版的 1.16 MB 是由分组数字推算的，而分组数字不可相加（见上文）；F1 是直接测量的合并配置，应以它为准。

对 `osdk-shim`：以上全部为 **0**，只要新代码门控在 `install` feature 之后且 shim 不引用（机制已于 §6.3 实测验证）。

#### 6.4.7 「签名 shell out 归零」的前提不成立

这是本轮推翻的一条结论，需要明确写出。

上一版据「shell out 实测 0 KB」推荐：若真要自实现，签名这一项必须 shell out。**体积数字没错，但可用性前提错了。**

- **`codesign` 需要 Xcode Command Line Tools。** 依据是 Homebrew 在 2026-08 的一次架构取舍中写下的判断：「Restore the fatal preinstall developer tools check, now skipped on Apple Silicon rather than gated to it, **as `codesign` requires the Command Line Tools while ruby-macho does not**」（https://github.com/Homebrew/brew/pull/23429）。
- **需要签名的平台恰好是最不该依赖它的平台。** macOS arm64 是唯一硬性要求重签名的平台，而 Homebrew 正是为了消除 CLT 依赖，在 arm64 上改用 ruby-macho 的纯 Ruby 实现 `MachO.codesign!`（同一 PR：「Keep `MachO.codesign!` on Apple Silicon, where it is required, proven and **avoids a `codesign` subprocess per relocated file**」）。Intel 侧因 ruby-macho 6.0 生成的签名被 Intel macOS 拒绝（`CODESIGNING, Invalid Page`，https://github.com/Homebrew/brew/issues/23418）才回退用 `codesign`，并保留 fatal 检查。
- **与 §2 的 Homebrew 官方口径不矛盾，但需收窄理解。** 官方说「cask 和 bottle 可无开发者工具安装」——这句话成立的前提**正是 Homebrew 不调用 `codesign`**。osdk 若自实现并 shell out，就把这个前提破坏了：安装 bottle 会重新变成需要 CLT。

因此三条路各有代价，没有免费选项：

| 方案 | 体积 | 前置条件 |
| --- | --- | --- |
| shell out 到 `codesign` | 0 KB | **需要 CLT**；破坏「bottle 免开发者工具」这一性质 |
| 纯 Rust `apple-codesign` | +1.57 MB | 无外部前置，但超红线且带 reqwest 0.12 |
| 自行实现 ad-hoc 签名 | 未测 | 需复刻 code directory 页哈希与签名块构造（ruby-macho 走的路） |

- **待验证 V-11**：Rust 生态中是否有等价于 ruby-macho `MachO.codesign!` 的 ad-hoc 签名实现（能生成 code directory 页哈希、不依赖 CLT、且不牵入 `apple-codesign` 的 XAR/CMS/X.509 全链）。**验证方法**：检索 crates.io 中具备 Mach-O 写入与 code signature 构造能力的 crate，核对是否支持 ad-hoc 签名；若有，按 §6.4.1 方法测其净增量。此项仅在将来重新考虑自实现时才需要，当前设计不依赖它。

**Linux 侧不存在「缺少 codesign」的跨平台缺口——这一点需要明确写出，以免读者误以为有遗留风险。** `/usr/bin/codesign` 确实是 macOS 专有、Linux 无等价物，但 **Linux 根本不需要这个动作**：

- ad-hoc 重签名是 **macOS arm64 独有的硬要求**。Apple 官方表述：「the operating system enforces that **any executable must be signed** before it's allowed to run……a simple ad-hoc signature is sufficient」，并明确「**doesn't apply to translated x86 binaries running under Rosetta 2, nor does it apply to macOS 11 running on Intel-based platforms**」。
- Linux 侧的 relocation 只有 ELF interpreter 与 rpath 改写，**不含任何签名步骤**——Homebrew 与 mise 对 Linux 路径的描述里都没有重签名这一步，与 macOS 侧明确存在的签名步骤形成直接对照。
- Linux 上**没有默认生效的用户态二进制强制签名校验**。内核 `CONFIG_IMA_APPRAISE` 上游 **`default n`**；即使编译进内核，未加载 appraise 策略时「the default policy is to **not appraise anything**」；而 `CONFIG_IMA_ARCH_POLICY` 自动添加的 appraise 规则只覆盖 `KEXEC_KERNEL_CHECK`、`MODULE_CHECK`、`FIRMWARE_CHECK`、`POLICY_CHECK`，**不含针对一般可执行文件的强制规则**。Secure Boot 与模块签名针对引导链与内核模块，不是普通用户态程序。（IMA appraisal 一旦由管理员显式启用确实是强制的，但那是非默认的加固配置。）

**结论：需要重签名的平台（macOS arm64）恰好就是提供了签名工具的平台。跨平台缺口在平台维度上不存在**；真正的缺口在**同一平台内 CLT 是否安装**这个维度上（即上表第一行）。这两件事容易混为一谈，故分开写明。

顺带澄清一个容易被误引入的依赖：**`rcodesign`（`apple-codesign` 的 CLI 形态）与本场景无关。** 其官方定位是「从非 Apple 操作系统（Linux、Windows、BSD）签名、公证并发布 Apple 软件」，配套设施是 YubiKey / PKCS#11 HSM / remote signing / GitHub Actions——整条能力线指向**发布链路**（在 Linux CI 上产出可分发的 macOS 产物）。而 osdk 的场景是「用户在自己的 mac 本机安装 brew 包」，机器本身就是 macOS，不存在跨平台签名需求。**它不是本设计必须引入的依赖。**

- **待验证 V-12**：`codesign` 在无 CLT 环境下的确切失败形态，以及 `codesign_allocate`（属 cctools，随 CLT 提供）是否为真正缺失的组件。**验证方法**：在从未装过 Xcode/CLT 的 macOS（`xcode-select -p` 报错）上执行 `/usr/bin/codesign --sign - <binary>; echo $?`，记录 stderr 全文与退出码；`file /usr/bin/codesign` 判断其是 shim 还是完整二进制。
- **待验证 V-13**：主流发行版**发行内核**是否启用 `CONFIG_IMA_APPRAISE=y`（已确证上游默认 `n`；「IMA is compiled in by most distros」指 measurement 而非 appraisal）。**验证方法**：`zgrep -E 'CONFIG_IMA_APPRAISE|CONFIG_EVM' /proc/config.gz` 或 `/boot/config-$(uname -r)`；运行时查 `/sys/kernel/security/ima/policy`。此项不影响当前设计（代理路线不改写 ELF），仅在将来考虑自实现时才需要。

来源：https://github.com/Homebrew/brew/pull/23429 ，https://github.com/Homebrew/brew/issues/23418 ，https://developer.apple.com/documentation/macos-release-notes/macos-big-sur-11_0_1-universal-apps-release-notes ，https://ima-doc.readthedocs.io/en/latest/ima-configuration.html ，https://gregoryszorc.com/docs/apple-codesign/main/index.html

#### 6.4.8 维护面

| crate | 版本 | 依赖图增量 | 平台覆盖 | 维护面风险 |
| --- | --- | --- | --- | --- |
| `oci-client` | 0.15 | +137 行 | 全平台 | 中。`oci-distribution` 的社区续作，钉 reqwest 0.12 |
| `object` | 0.36 | +6 行 | 全平台 | **低**。gimli 项目维护，Rust 生态基础设施，无重依赖 |
| `apple-codesign` | 0.29 | **+564 行** | 全平台可编译，语义仅 macOS | **高**。单人主导；牵入 XAR、CMS、X.509、ASN.1 一整条链；13 个重复 crate 的主要来源 |
| `resolvo` | 0.12 | 0（已在图中） | 全平台 | 无 |

#### 6.4.9 口径与局限（诚实声明）

- **本次测量在 Windows（`x86_64-pc-windows-msvc`）上完成，全部为最终二进制口径**（链接 + `strip` 后的 `.exe` 字节数），非 rlib 口径、非估算。
- **平台偏差方向已知：Windows 上的数字是偏小的下界。** `apple-codesign` 与 `object` 的 Mach-O 路径在 macOS 目标上会有更多平台相关代码进入链接。因此 F1 的 +1.56 MB 与 F2 的 +2.90 MB 都应理解为**下界**，真实 macOS 产物只会更大——这不改变「超红线」的结论，只会让它更明确。
- 交叉编译到 `aarch64-apple-darwin` 只能量 rlib（Windows 上无 Apple 链接器），与本表的最终二进制口径不可直接比较，故未混入上表。
- `resolvo` 的「0 MB」仅对 **`osdk-cli`** 成立（它已因 rattler 在图中）。若某个不含 rattler 的配置要用它，需另行测量。
- **`codesign` shell out 的 0 KB 是纯体积口径**，不含可用性代价（§6.4.7）。测量时探针调用的是 `/usr/bin/codesign` 路径，在 Windows 上必然失败并走 `Err` 分支——这不影响体积测量（链接进去的是 `std::process` 调用代码，与调用是否成功无关），但意味着**本测量未验证该命令的实际行为**。
- 未测第 6 类「无 bottle 时的源码构建路径」：mise 为此内嵌了一个 mise 管理的 ruby 加自有 Formula-DSL shim。**osdk 不可能做这块**——它要求分发或管理一个 Ruby 运行时，并复刻 Homebrew 的 Formula DSL 语义，与 osdk 的定位和体积预算都不相容。该项直接从预算中剔除，不参与上述任何合计。


---

## 7. Linux 发行版包管理器：apt / apk / pacman（及 dnf、zypper 评估）

本节把调研范围扩到 Linux 发行版包管理器。**结论先行：它们不应与 winget/Homebrew 同级进入 osdk，而应降级为「只读检测 + 给出建议命令」。** 理由不是能力不足，而是它们与前两者存在**类别差异**——见 §7.3。

本节的 `apk` 一律指 **Alpine Linux 的 apk-tools**，与 Android APK 无关。Android APK 不在本设计范围内（它是应用分发格式而非宿主包管理器，与 osdk 的 SDK/工具链管理场景无交集），不再单列。

### 7.1 与 winget/Homebrew 同一套维度的横向对比

| 维度 | winget | Homebrew | apt/dpkg | apk (Alpine) | pacman |
| --- | --- | --- | --- | --- | --- |
| **非交互调用** | `--disable-interactivity --silent` + 双协议接受 | 基本非交互 | `-y` + `DEBIAN_FRONTEND=noninteractive`（**两套机制各管一段**） | **默认即非交互**（唯一一家） | `--noconfirm` |
| **退出码** | 有文档化码表 | 一般 | **仅 0 / 100**（apt）；dpkg 有 0/1/2 含断言语义 | **无成文 EXIT STATUS**（V-12） | **无成文 EXIT STATUS**（V-13） |
| **机器可读输出** | JSON 有限 | `brew info --json` 良好 | **apt 官方警告 CLI 无稳定接口**；稳定面是 `dpkg-query -W -f=` 与 `dpkg --robot` | **最强**：`apk query --format json\|yaml` + `--fields` | 无 JSON；`--print-format`、`-F --machinereadable`（格式精确定义） |
| **版本查询** | `winget list --id --exact` | `brew list --versions` | `dpkg-query -W -f=` | `apk info -e -v` / `apk query` | `pacman -Q` / `-T` |
| **锁定不升级** | pin（不透明字符串） | `brew pin` | `apt-mark hold`（可被 `--ignore-hold` 越过）；`Pin-Priority: -1` | world 约束 `name=version`；文件身份哈希约束 | `--ignore` / `IgnorePkg`（Arch 官方警告风险） |
| **安装指定版本** | 部分支持 | 基本不支持（仅 `@` 版本化 formula） | `pkg=version`；APT pinning（`Pin-Priority ≥ 1000` 才允许降级） | 原生 `name=version` | **实际不可行**——Arch 仓库只存最新版 |
| **安装目录控制** | 仅 portable 可控 | prefix 事实固定 | `--instdir`/`--root`：**chroot 语义，非前缀** | `--root`：rootfs 语义 | `--root`：**官方明文禁止用作 `/usr/local` 前缀** |
| **权限** | 部分需管理员；portable 用户态可装 | **不需要 sudo**（仅首次建 prefix） | **变更一律需 root**（查询免 root） | **变更需 root**；`--usermode` 有限非 root | **变更一律需 root** |
| **幂等性** | 需先查后装 | 较好 | `--no-upgrade` / `--only-upgrade` | world 约束模型天然接近幂等 | **`--needed`**（官方幂等开关） |
| **镜像换源** | 仅镜像 manifest，**不镜像安装包** | `HOMEBREW_API_DOMAIN` 等 | **镜像包本体**，TUNA/USTC/阿里云均有官方帮助页 | 同左，一条 sed 改 `/etc/apk/repositories` | 同左，改 `/etc/pacman.d/mirrorlist` |
| **事务性/回滚** | 无 | 无 | **无 undo**，仅日志；有 `half-installed` 等非原子状态 | 无 undo | **无 undo**，仅 cache 手工降级（官方称 last resort） |

**镜像一条的结构性差异值得单独点出**：Linux 发行版镜像的是**包本体 + 索引**（`.deb`/`.apk`/`.pkg.tar.zst` 都托管在仓库内），换源真正加速下载；而 winget 镜像只镜 manifest，`InstallerUrl` 仍指向上游（§5.2）。这是技术根因，不只是运营投入差异。

**机器可读输出这一维度上，Linux 三家反而强于 winget**：`apk query --format json` 是本次调研见到的最完整的结构化接口（`--fields`/`--match`/`--from`/`--summarize`，官方明确区分「machine parseable output」与人类可读格式）。但 apt 侧需要特别注意——官方在 apt(8) 里成文警告「the apt(8) commandline is designed as an end-user tool and **it may change behavior between versions**」，并指示脚本改用 apt-get/apt-cache；真正稳定的接口是 `dpkg-query -W -f=` 与 `dpkg --robot`。mise 的实现印证了这一点：它用 `dpkg-query` 而非解析 `apt list --installed`。

### 7.2 安装目录控制：三家都比 Homebrew「看起来更可控」，实际不然

这一条需要修正一个容易产生的印象。apt 的 `--instdir`/`--root`、pacman 的 `--root`/`--dbpath`、apk 的 `--root` 看起来提供了 Homebrew 所缺的前缀控制能力，**但官方文档明确否定了这一用法**：

- **pacman 直接写了禁止**：`-r/--root` — 「**This should not be used as a way to install software into /usr/local instead of /usr.**」`-b/--dbpath` — 「should not be used unless you know what you are doing」。操作挂载的 guest system 应改用 `--sysroot`。
- **dpkg 的 `--instdir` 本质是 chroot**：「`instdir` is also the directory passed to **`chroot(2)`** before running package's installation scripts, which means that the scripts see `instdir` as a root directory.」跳过 chroot 需 `--force-script-chrootless`，此时正确性责任转移给每个 maintainer script 是否老实使用 `$DPKG_ROOT` 前缀——这不是包管理器能保证的。
- **`--root` 会连带迁移整套状态**：admindir（`/var/lib/dpkg`）、dbpath（`/var/lib/pacman`）、cache、keys、log 一并挪到新根下。结果是新根拥有**独立的已装包视图**，与宿主互不可见。

**这些参数的设计本意是 bootstrap 新系统 / chroot / 容器构建 / 交叉架构镜像提取**，证据是它们的配套选项无一例外属于 rootfs 构建工具链：`--initdb`、`--arch` 随 root 保存、`--force-no-chroot`、`--scripts=no`、`--sysroot`。

**结论：它们提供的是「换一个完整世界」（rootfs bootstrap）的能力，不是「换一个安装前缀」的能力。** 在非标准前缀下，动态链接器路径、maintainer script 绝对路径、依赖数据库归属这三件事都不成立。这与 §4 对 Homebrew 的结论方向一致：**真正需要目录控制与可复现性的场景，应走 osdk 自有的下载安装通道（现有 backend 体系），而不是系统包管理器。**

- **待验证 V-14**：`--root` 安装下动态链接器路径的具体表现（Debian 包内 ELF 的 interpreter 固定为 `/lib64/ld-linux-x86-64.so.2`，在非 chroot 的替代根下是否直接不可执行）。**验证方法**：`dpkg --instdir=/tmp/root --admindir=/tmp/root/var/lib/dpkg --force-script-chrootless -i coreutils_*.deb`，随后 `readelf -l /tmp/root/bin/ls | grep interpreter` 并直接执行观察是否报 `No such file or directory`。

### 7.3 结构性问题：它们应以何种姿态进入 osdk

**这是本节的核心，答案是「只读检测 + 建议命令」，不是同级 provider。** 与 §3.2/§4 对 winget 非 portable 包定下的「明确拒绝而非静默失效」原则一致，这里也需要同样诚实的边界。

三条结构性差异决定了这个判断：

**一、它们管理全局系统状态，影响范围超出 osdk 管控目录。** winget 的 portable 包装进 `%LOCALAPPDATA%`、Homebrew 的 prefix 归安装用户所有——这两者的变更都局限在用户态。而 apt/pacman/apk 的变更一律需要 root 且落在 `/usr`、`/etc`、`/var`。apt 的 `upgrade` 与 `full-upgrade` 的差别本身就说明问题：后者「**will remove currently installed packages if this is needed** to upgrade the system as a whole」。一条 `apt-get install` 可能连带升级共享库、删除其他包、触发 `autoremove` 的候选变化。

**二、它们与发行版自身的升级生命周期耦合，且 Arch 官方明确声明部分升级不受支持。** 这是最强的一条依据，Arch Wiki 设有独立小节「Partial upgrades are unsupported」：

> 「if two packages depend on the same library, upgrading only one package might also upgrade the library (as a dependency), which might then break the other package which depends on an older version of the library. **That is why partial upgrades are not supported.** Do **not** use: `pacman -Sy package`……」
> 「Avoid doing partial upgrades. In other words, **never** run `pacman -Sy`; instead, **always** use `pacman -Syu`.」

即：**任何以「只装/只升我需要的那个包」为目标的自动化，在 Arch 上都落在官方声明不支持的区域内**——而这恰恰是 osdk 这类工具最自然的行为模式。同一页还指出 `IgnorePkg` 属同类风险、中断的 `-Syu` 会留下危险状态、升级不作用于已运行进程、**升级前应先读发行版新闻公告**（这一步无法自动化）。pacman 自身的免责声明也很直白：「it does not attempt to handle all corner cases. **Users must be vigilant and take responsibility for maintaining their own system.**」

**三、失败后无法回滚，且各家能力差异极大。** dnf 有真正的事务撤销（`dnf history undo`/`rollback`，且 rollback 本身原子：无法全撤则不撤任何）；pacman 只能从 cache 手工降级，官方定位为「last resort」，而清理 cache 又是官方推荐的常规维护；apt/dpkg 只有日志没有 undo，其 `half-installed`/`half-configured`/`reinstreq` 状态族与默认 `--abort-after=50` 直接证明了非原子性。**跨发行版的统一抽象无法承诺一致的失败恢复语义**——这是比「难实现」更根本的问题。

**因此 osdk 的姿态应当是：**

| 动作 | 是否提供 | 说明 |
| --- | --- | --- |
| **检测是否安装、版本查询** | ✅ 提供 | 只读，绝不提权。用各家稳定接口：`dpkg-query -W -f=`、`apk info -e -v` 或 `apk query --format json`、`pacman -Q`/`-T` |
| **诊断与 doctor 集成** | ✅ 提供 | 报告缺失包、镜像可达性 |
| **镜像探测与配置建议** | ✅ 提供计划 | 与 §5 同构，但**只生成建议不写入**——这几家的源配置文件属系统文件 |
| **生成可复制的建议命令** | ✅ 提供 | 打印完整命令让用户自己执行，含 Arch 上应改用 `-Syu` 的提示 |
| **代为执行安装/升级/删除** | ❌ **不提供** | 需 root、影响全局、不可回滚、在 Arch 上属官方不支持的 partial upgrade |
| **纳入 lockfile** | ❌ 不提供 | 与 §10 对 winget/brew 的结论一致，理由更强 |

这个姿态与 `container/` 先例一致：`container/mirror.rs` 的 planners「never write, restart, recreate, or elevate a native runtime」。**系统包管理器比容器运行时更需要这条约束。**

### 7.4 mise 的做法：一个需要修正的前提，与一组更有价值的论据

调研前的假设是「mise 未覆盖 apt/pacman，它为什么不做本身就是论据」。**这个前提不成立**——官方文档的 manager 表格明确包含 `apk`、`apt`、`aur`、`dnf`、`pacman`、`brew`、`brew-cask`、`flatpak`、`flatpak-user`、`mas`，另有 plugin 机制；winget 与 nix 已有完整独立文档页（尚未进入汇总表格，属文档滞后）。

**但它「用什么姿态做」比「做不做」信息量更大，且高度支持 §7.3 的判断**：

- **状态检查一律只读、绝不提权**：apt 用 `dpkg-query`、apk 用 `apk info -e -v`、pacman 用 `pacman -Q` 与 `-T`，官方文档对三者都标注「read-only, never elevates」。
- **永不隐式安装**：「mise never installs system packages implicitly……**only `mise bootstrap packages apply` ever installs anything**」。
- **sudo 边界四档且可完全禁止**：已 root 则直接执行；交互终端走正常 sudo 提示；**非交互且无免密 sudo 时报错并打印待执行命令，「it never hangs waiting for a password」**；每次执行前**记录完整命令行**。`system_packages.sudo = false` 可彻底禁止提权，改为打印命令。
- **把发行版官方立场原样转达而非藏起来**：pacman 页直接写「**Arch officially supports only full-system upgrades (`pacman -Syu`)** — upgrading individual packages is a partial upgrade, so prefer running `pacman -Syu` yourself on a rolling-release system」。
- **不承诺事务性**：「Bootstrap is a sequence, not a transaction: if a later phase fails, earlier successful changes remain.」
- **pin 能力诚实分档**：apk/apt/dnf 支持 pin；AUR/pacman/brew/brew-cask/flatpak/flatpak-user/mas **不能装 pin，跳过并警告**（pacman 的理由是 Arch 仓库只存最新版）；nix 亦不支持版本 pin，改用 source revision。
- **升级只用 `--only-upgrade`**（apt）以确保「nothing not already installed gets pulled in」；pacman 用 `--needed` 保证幂等，并**跳过由 `Provides` 满足的请求**以免替换已装的 provider。

**osdk 与 mise 的定位差异决定了可以更保守**：mise 的 `[bootstrap.packages]` 是一个显式的、用户主动调用的引导流程；osdk 的核心职责是 SDK/工具链管理，系统包管理器只是周边设施。因此 osdk 可以只取「只读检测 + 建议命令」这一子集，把执行权完整留给用户——这既不损失诊断价值，又完全规避了 §7.3 的三类风险。

### 7.5 是否纳入 dnf 与 zypper

- **dnf：建议纳入，但仍限于只读检测。** 理由是它在一个维度上带来 apt 所没有的信息量——**真正的事务性与回滚**（`dnf history undo` / `rollback`）。这不是「与 apt 同构」，而是结构性差异：它使「工具驱动的系统变更是否可逆」在 Fedora/RHEL 系上有截然不同的答案。若将来重新评估「是否代为执行」，dnf 会是唯一一个技术前提更宽松的候选。
- **zypper：本次未取证，不做结论。** 现有线索指向它可能在两个维度上**不**与 apt 同构（openSUSE 的 btrfs snapshot + snapper 集成可提供文件系统级回滚；zypper 有 `--non-interactive` 全局选项与成文退出码表），因此**不应断言它与 apt 同构而略过**。
  - **待验证 V-15**：zypper 在本节九个维度上是否带来 apt/dnf 之外的新增信息量。**验证方法**：读 `man zypper`（https://en.opensuse.org/SDB:Zypper_manual ）确认 EXIT CODES 节是否存在及其码表；读 openSUSE 官方 snapper 文档确认 zypper 与 snapshot 的集成方式与默认开启状态。

来源：https://manpages.debian.org/unstable/apt/apt.8.en.html ，https://manpages.debian.org/unstable/apt/apt-get.8.en.html ，https://manpages.debian.org/unstable/dpkg/dpkg.1.en.html ，https://manpages.debian.org/unstable/apt/apt_preferences.5.en.html ，https://man.archlinux.org/man/apk.8.en ，https://man.archlinux.org/man/apk-add.8.en ，https://man.archlinux.org/man/apk-query.8.en ，https://man.archlinux.org/man/pacman.8.en ，https://wiki.archlinux.org/title/System_maintenance ，https://dnf.readthedocs.io/en/latest/command_ref.html ，https://mise.jdx.dev/bootstrap/packages/ ，https://mise.jdx.dev/bootstrap/packages/apt.html ，https://mise.jdx.dev/bootstrap/packages/apk.html ，https://mise.jdx.dev/bootstrap/packages/pacman.html ，https://mirrors.tuna.tsinghua.edu.cn/help/debian/ ，https://mirrors.tuna.tsinghua.edu.cn/help/alpine/

---

## 8. osdk 侧的具体设计

### 8.1 定位声明

> **系统包管理器是宿主设施，osdk 是它的协调者，不是它的替代品。**
>
> osdk 提供：发现与诊断、状态查询、镜像探测与配置计划、显式的声明式 apply。
> osdk 不提供：安装目录接管、shim 集成、版本锁定承诺、隐式安装、包管理器自身的重新实现。

### 8.2 配置 schema

置于顶层独立段，**不混入 `[tools]`**（理由见 §3.1）：

```toml
[syspkg]
# 允许参与的管理器；未列出的即使宿主上存在也不使用
managers = ["winget", "brew"]
# 禁止 osdk 代为提权。为 true 时遇到需要提权的操作，
# 打印用户需要手工执行的确切命令而非尝试提升
no_elevate = false

[syspkg.packages]
# 键为 "manager:package-id"，管理器前缀强制（借鉴 mise）
"winget:BurntSushi.ripgrep.MSVC" = "latest"
"winget:Microsoft.PowerToys" = "0.101.0"
"brew:ffmpeg" = "latest"
"brew-cask:visual-studio-code" = { os = "macos" }

[syspkg.mirrors]
# 镜像选择策略，复用 source::Selection 的语义
selection = "auto"           # auto | pinned | ordered
probe_timeout_ms = 1500
cache_ttl = "24h"
# 可选：显式 pin 到某个镜像 id
# winget_source = "ustc"
# brew_api = "tuna"
# brew_bottle = "tuna"
```

**几处刻意的设计选择**：

- **键上强制管理器前缀**，且**不做跨平台包名映射**（§2.3）。同一软件在两个平台上要分别写两行，这比一个会出错的映射表诚实。
- **`os` 过滤**借鉴 mise，名称与现有 `[tools]` 的平台表述保持一致。
- **只支持 `present` 语义，不支持 `state = "absent"`**。声明式移除在首版不做（mise 的 winget 侧同样不做）——移除的副作用面比安装大得多，且 winget 的卸载完全委托给安装器自己的 uninstall 命令（找不到就返回 `NO_UNINSTALL_INFO_FOUND`）。
- **版本值按不透明字符串处理**（借鉴 mise 的 winget 侧做法）。不尝试对系统包做 semver 语义比较——两侧的版本字符串格式都不受 osdk 控制。
- **`[syspkg.packages]` 属于 execution-affecting 配置**，因为它能导致在用户机器上安装软件。**必须纳入现有的 `trust.rs` 流程**，与 `[registries]`、`[containers]` 同级对待（见 §9）。

### 8.3 CLI 命令面

与 `osdk container` 的形状保持一致，便于用户迁移既有心智：

```
osdk pkg doctor [--manager winget|brew] [--json]
    发现宿主上的包管理器：是否安装、版本、是否可用、当前 source/prefix、
    权限状态、已知的不可用原因（未注册 / 版本过旧 / 被组策略封锁 /
    Homebrew 处于非默认 prefix 即 Tier 2/3）

osdk pkg status [--json] [--missing]
    对照 [syspkg.packages] 报告每个包的状态。
    --missing：有缺失则 exit 1（CI 检查用，借鉴 mise）
    --json：deterministic、schema 化（与 container 的 --json 同构）

osdk pkg plan [--json] [--detailed-exitcode]
    打印将要执行的操作（含完整命令行）而不执行。
    --detailed-exitcode：0 = 无变更，2 = 有变更，1 = 规划失败或存在 unknown

osdk pkg apply [PKG...] [--dry-run] [--yes] [--manager X]
    显式安装缺失的包。这是唯一会改变系统状态的包操作命令。

osdk pkg mirrors test [--manager X] [--json]
    探测镜像候选并打印排序（吞吐 / TTFB / 可达性），不做任何修改

osdk pkg mirrors apply [--manager X] [--dry-run] [--accept-plan <SHA256_ID>]
    把实测最快的镜像注册进包管理器的全局配置（L3，需管理员）。
    --dry-run：只打印带指纹的变更计划与信任链影响，不执行
    首次执行需要 --accept-plan 确认指纹；此后同一目标可自动维护
```

**几处关键约定**：

- **`apply` 是唯一会安装东西的命令。** `osdk install` 在检测到 `[syspkg.packages]` 有缺失时，只打印一次性提示，**绝不顺手安装**（§1.2 第 4 条）。
- **每次调用外部命令前，完整命令行都要记录并可见**（借鉴 mise，与 `container/redact.rs` 的姿态一致）。
- **`--json` 输出必须是确定性的、schema 版本化的**，字段名与枚举值不随 `--lang` 翻译——这是 `container doctor --json` 已确立的约定。
- `osdk pkg mirrors` 与现有 `osdk source`（管 SDK 下载源）、`osdk registry`（管项目依赖 registry）是**三条独立链路**，文档中必须像 `docs/package-registry-design.md` 那样画清边界，避免用户混淆。

### 8.4 与外部命令交互的可靠性设计

这是实现中最脆弱的部分，必须从设计上隔离。

**winget 调用约定**：

- 一律附加 `--disable-interactivity`、`--nowarn`。**`--no-progress` 不作为必需项**（V-8 实测：重定向后无 ANSI 污染，且该标志已从帮助中移除，详见 §8.4.1）。
- 安装时附加 `--silent`、`--accept-package-agreements`、`--accept-source-agreements`；查询时只需 `--accept-source-agreements`。
- 一律使用 `--id <ID> --exact` 精确匹配，**绝不接受模糊匹配**（借鉴 mise：「bootstrap never accepts an ambiguous fuzzy match」）。
- **osdk 自己发起的调用自动附加 `--source <name>`，选用实测最快的已注册源**（§5.5 的 L2）。这一步不改全局配置、不需提权、不影响用户手敲 winget 的行为。若无可用的镜像源，或探测数据过期且当前离线，则省略该参数、退回 winget 自身的默认源选择——**降级必须是省略参数，而不是猜一个源名**：传入未注册的源名会让 winget 直接报错（`0x8A150012`，V-8 实测），把一次本可成功的安装变成失败。
- **winget 镜像无法与官方源并存，只能顶替它。这一条推翻了 L2 的适用前提**（实机验证，管理员会话）：
  - `winget source add --name osdk-probe-ustc --arg <USTC> --type Microsoft.PreIndexed.Package --trust-level trusted` 失败，退出码 `0x80073D06`（`-2147009274`），报「已安装此程序包的更高版本」。
  - 根因是包身份固定：官方源的 `Data`/`Identifier` 均为 `Microsoft.Winget.Source_8wekyb3d8bbwe`，而失败信息里的包正是 `Microsoft.Winget.Source_..._8wekyb3d8bbwe`——**同一个 MSIX 身份**。`Microsoft.PreIndexed.Package` 类型的源以固定身份安装，镜像分发的是同一个包的副本，因此第二个源装不进去；又因镜像同步滞后（镜像 `2026.915.1105.46` < 本机 `2026.915.1714.48`），Windows 直接以「已装更高版本」拒绝。
  - 这解释了为什么 USTC 官方帮助页教的是 `source remove winget` + `source add winget <镜像>`：**并存在机制上不可能，顶替是唯一形状**，回滚靠 `source reset winget`。
  - `Microsoft.Rest`（msstore 用的类型）不是替代出路：它指向 REST API，而国内镜像站提供的是 `source.msix` 静态文件镜像，不是 REST 服务。
  - **对 L2 的直接后果**：顶替之后，镜像**就是**名为 `winget` 的那个源，对所有 winget 调用（含 osdk 自己的）默认生效，**根本不需要 `--source`**；而顶替之前，镜像不以任何独立名字存在，`--source` 也无从选择。因此 **`--source` 在 winget 上不是镜像加速的手段**——加速完全由 L3 的顶替动作达成，L2 对 winget 退化为「保持沉默」。这不影响 Homebrew：那侧靠环境变量，与此机制无关。

- **顶替前必须做「镜像是否比本机更新」的可行性检查，而判据只能是 `Last-Modified`，不能是版本号。** 这一条直接来自上面 `0x80073D06` 的失败：镜像比本机旧时，顶替同样会被拒，用户会撞上一次已知可预测的失败。实测数据：

  | 来源 | `Last-Modified` (GMT) | 对应 MSIX 版本 |
  | --- | --- | --- |
  | 官方 CDN | 2026-09-15 **17:45** | 本机已装 `2026.915.`**`1714`**`.48` |
  | USTC | 2026-09-15 **10:21** | 失败信息中的 `2026.915.`**`1105`**`.46` |
  | NJU | 2026-09-14 17:09 | 未取（更旧） |

  两点结论：
  1. **不能从 `Last-Modified` 推算版本号**——`1714` ≠ `17:45`，`1105` ≠ `10:21`，版本里的时刻是构建时刻而非发布时刻，强行换算就是编造。
  2. **但两者单调同序**，可用于比较新旧：USTC 的 10:21 早于官方 17:45，其版本 1105 也确实低于 1714。因此检查用 `HEAD` 取 `Last-Modified` 比较即可，代价是一个请求，不必下载 20 MB 的包。

  **镜像不提供版本元数据文件**，这一点做过对照验证：华为云对**任意**路径都返回 `200` 并吐同一个 11963 字节的 HTML 页（用一个随意编造的路径名验证过），所以「`/version` 返回 200」不能当作该文件存在——按状态码判断会把 HTML 当版本号写进决策。USTC 对 `/version` 如实返回 404。

- **`--source` 是排他的：指定一个源就屏蔽其余全部源。** 实机验证（winget 1.29.290）：`winget search --query WhatsApp` 能返回 msstore 的 WhatsApp（ID `9NKSQGP7F2NH`），而 `--source winget` 后该结果消失，只剩 winget 源的条目，退出码均为 0——**没有任何报错，只是结果变少**。两条推论：
  1. **实测最快者为官方源时必须省略 `--source`，而不是显式指定它。** 显式指定不带来任何加速（它本就是默认），却会让 msstore 独有的包变成「找不到」。这是一个静默的功能损坏，正是「验证失效模式」里「看起来正常的降级」那一类。
  2. 因此 L2 的返回值只在**选中镜像**时才是源名；选中官方源属于「无需指定」而非「选择失败」，两者在类型上必须可区分（实现为 `NoPreferredSource::OfficialIsFastest`）。
- **退出码按 HRESULT 白名单分类**，而非「非零即失败」：

| 分类 | 退出码 | osdk 的处理 |
| --- | --- | --- |
| 成功 | `0`，以及 manifest 声明的 `InstallerSuccessCodes` | 成功 |
| **视为已满足（幂等）** | `PACKAGE_ALREADY_INSTALLED` (-1978335135)、`INSTALL_ALREADY_INSTALLED` (-1978334963)、`UPGRADE_VERSION_NOT_NEWER` (-1978335153)、`UPDATE_NOT_APPLICABLE` (-1978335189) | 报告为「已满足」，不视为失败 |
| 需用户介入 | `COMMAND_REQUIRES_ADMIN` (-1978335207)、`INSTALLER_PROHIBITS_ELEVATION` (-1978335146)、`ADMIN_CONTEXT_ACTION_PROHIBITED` (-1978335107) | 打印用户需手工执行的确切命令 |
| 环境/策略不可用 | `BLOCKED_BY_POLICY` (-1978335174)、`INSTALL_BLOCKED_BY_POLICY` (-1978334961) | 报告为「被组策略封锁」，**不重试** |
| 歧义 | `MULTIPLE_APPLICATIONS_FOUND` (-1978335210) | 报错并要求用户给出精确 ID |
| 其他 | — | 原样透出 winget 的错误文本与退出码 |

- **绝不解析 `winget list` / `show` / `source list` 的表格输出**。V-8 实测确认这些输出是**界面语言本地化**的（中文系统上列头为「名称/ID/版本/可用/源」），按英文列头解析会在非英文宿主上直接失效。优先级顺序：退出码 → `winget source export` 的 JSON Lines → `winget export -o` 的 schema 2.0 JSON → （最后手段）表格解析，且必须单独成模块并配足回归测试。

#### 8.4.1 V-8 实测结论（已验证，阻塞解除）

实测环境：Windows 11 Build 26200.9445 / X64，winget **v1.29.290**（`Microsoft.DesktopAppInstaller v1.29.290.0`），界面语言为简体中文。

| 观察项 | 实测结果 |
| --- | --- |
| 重定向到文件后的 ANSI 污染 | **ESC = 0**。`list`（无标志）、`list --no-progress`、`list --disable-interactivity`、`search --no-progress` 四种情形下，输出字节完全一致（927 / 927 / 927 bytes），均无 `0x1B`、无退格符 |
| `--no-progress` 是否仍在帮助中 | **否**。`list`/`search`/`show`/`install`/`upgrade`/`uninstall`/`source` 七个子命令的 `-?` 输出均**不再列出** `--no-progress` |
| `--no-progress` 是否仍被接受 | **是**，退出码 `0`。对照组：未知参数 `--definitely-not-a-flag` 报 `0x8A150002` 并打印帮助 |
| `--disable-interactivity` | 上述七个子命令**全部**列出该标志 |

**结论一：`--no-progress` 不必依赖。** 进度渲染在 stdout 非 TTY 时本就不发生，重定向场景下加不加该标志输出完全相同。它已从文档化选项中移除但仍被静默接受——这正是不应依赖的形态：既无文档保证，又无实际收益。osdk 采用 `--disable-interactivity` + `--nowarn`，两者都是当前文档化的稳定选项。

**结论二（文档此前未预见的风险）：winget 的人类可读输出是界面语言本地化的。** 中文宿主上 `winget list` 的列头是「名称 / ID / 版本 / 可用 / 源」，`winget source list` 是「名称 / 参数 / 显式」。**任何按英文列头编写的表格解析器都会在非英文 Windows 上失效**，而这类失效是静默的——解析不到列就当作"包未安装"，比报错更危险。

**结论三：存在与语言无关的结构化通路，应优先使用。**

- `winget source export` 直接输出 **JSON Lines**，键名为英文且不随界面语言变化，实测含 `Name` / `Identifier` / `Arg` / `Type` / `TrustLevel` / `Explicit` / `Data`：

  ```json
  {"Arg":"https://cdn.winget.microsoft.com/cache","Data":"Microsoft.Winget.Source_8wekyb3d8bbwe","Explicit":false,"Identifier":"Microsoft.Winget.Source_8wekyb3d8bbwe","Name":"winget","TrustLevel":["Trusted","StoreOrigin"],"Type":"Microsoft.PreIndexed.Package"}
  ```

  这**同时解决了 V-5**：`TrustLevel` 是结构化字段而非需要解析的文本，镜像源接入后的信任级别可直接读取比对。

- `winget export -o <file> [--include-versions]` 输出 `https://aka.ms/winget-packages.schema.2.0.json` 的带版本 schema，实测退出码 `0`（对于无法溯源到任何 source 的已装程序，仅在 stderr 打印「无法从任何源获得已安装的程序包: X」告警，**不影响退出码**）。

**结论四：退出码目录（本机实测补充）。**

| 场景 | 退出码 | HRESULT |
| --- | --- | --- |
| `list` / `show` 查询无匹配 | -1978335212 | `0x8A150014` |
| `source list --name <不存在的源>` | -1978335214 | `0x8A150012` |
| 未知命令行参数 | -1978335230 | `0x8A150002` |
| 查询有匹配 / `source export` / `export -o` 成功 | 0 | — |

**对实现的影响**：Phase 1 的输出层按「退出码优先 → JSON 通路 → 绝不依赖本地化表格」三级设计。解析器不得包含任何中英文列头字面量。

**brew 调用约定**：

- **绝不用 sudo 调用 brew**（Homebrew 明确拒绝）。
- 查询优先用 `brew --prefix --installed <formula>`（官方文档化的布尔探测：未装则返回失败状态码），而非解析 `brew list` 输出。
- 需要结构化信息时用 `brew info --json=v2`（**必须显式指定 v2**，默认 v1 不含 cask）。反序列化必须宽松（容忍新增字段），因为官方明确「字段可能在不递增 schema 的情况下按需添加」。
- 非交互场景设置 `HOMEBREW_NO_AUTO_UPDATE=1` 以避免隐式的漫长 update；但要注意官方更推荐调大 `HOMEBREW_AUTO_UPDATE_SECS`，且警告「设置本项且新 tap 可能导致配置损坏」。
- cask 的 `pkg` / `installer script` 产物**会要求 sudo 密码**。osdk 必须把提示**透传到终端**而不是吞掉——否则表现为神秘挂起。非交互场景应当预先检测并拒绝，打印用户需手工执行的命令（借鉴 mise 的「never hangs waiting for a password」）。

> **待验证 V-2**：`brew install <已装且最新>` 的退出码是 0 还是非 0（已知走 `opoo` 警告路径，`brew bundle install` 明确是 no-op，但退出码无文档）。
> 验证方法：实机在某包已装且最新时 `brew install hello; echo $?`。**这一条直接决定幂等判定逻辑，应在实现 Phase 4 前完成验证。**

### 8.5 与 `store/`、`shim/`、`inventory` 的关系

**完全不集成，这是刻意的：**

- **不进 `store/`（CAS）**：系统包的产物由包管理器自己管理，osdk 不持有其字节。
- **不进 `installs/`**：不存在 `installs/winget/<pkg>/<version>/` 这样的目录，也不写 `.osdk-complete`。
- **不生成 shim**：`osdk reshim` 不扫描系统包。
- **不进 `osdk list`**：`osdk list` 的语义是「osdk 安装的工具版本」，系统包出现在那里会误导用户以为可以 `osdk use`。系统包的状态只在 `osdk pkg status` 中呈现。
- **不参与 `activate` / `hook-env`**：系统包的 PATH 由宿主机制负责。

`osdk pkg status` 的状态判定**直接查询包管理器**（winget 用退出码、brew 用 `--prefix --installed`），不维护 osdk 侧的影子清单。理由是影子清单必然与真实状态漂移——用户完全可能绕过 osdk 直接 `brew install`，而那应当被正确识别为「已满足」（mise 在这一点上的做法值得参照：「formulae installed by brew count as installed」）。

### 8.6 信任与校验：`trust.rs` / `verification/` 如何适用

**`trust.rs` 适用，且是必须的。** `[syspkg.packages]` 能导致在用户机器上安装软件，属于典型的 execution-affecting project configuration。现有 `docs/package-registry-design.md` 对 `[registries]` 的判断是「因为 `[registries]` 能改变子进程的网络目的地，它属于 execution-affecting project configuration，应沿用 one-sdk 的项目配置 trust 流程」——`[syspkg]` 的影响面**更大**（不是改变网络目的地，而是直接安装软件），理应适用同一流程，且未受信任时应当**完全拒绝执行 apply**，而不只是降级。

**`verification/` 与 `pipeline/verify.rs` 不适用，这是一个必须讲清楚的能力缺口。** osdk 对自有 backend 的产物做 BLAKE3/SHA-256 校验、minisign 验签、GitHub attestation 验证。对系统包：

- osdk **不接触字节**——下载与校验由 winget / brew 完成。
- **winget** 会校验 manifest 中声明的安装器哈希（不符则 `INSTALLER_HASH_MISMATCH`）。
- **Homebrew** 对 bottle 有 sha256 校验，且有 attestation 机制（`HOMEBREW_NO_VERIFY_ATTESTATIONS` 可关闭）；cask 侧依赖 Developer ID 签名 + Apple 公证 + Gatekeeper。
- **osdk 能做也应该做的是**：在 doctor 中**报告**这些机制的状态（例如检测到用户设了 `HOMEBREW_NO_VERIFY_ATTESTATIONS` 时给出警告），而不是自己再做一遍校验。
- **换镜像会削弱信任链**，必须如实告知：winget 镜像源无法获得默认 source 的 `StoreOrigin` 信任标记（只能是 `Trusted`）。这一点应当在 `osdk pkg mirrors apply --dry-run` 的输出中显式列出，让用户在确认前就看到。

文档中必须明确写出这条界线：**「osdk 对系统包提供的是发现与协调，不提供 osdk 级别的产物验证保证。」** 否则用户会合理地假设 `osdk pkg apply` 装的东西享有与 `osdk install` 同等的校验强度。

---

## 9. 权限与降级路径

| 场景 | osdk 的行为 |
| --- | --- |
| winget 需要管理员（含 `source add/remove/reset`） | **不尝试自行提升。** 打印用户需手工执行的确切命令 + 说明为何需要管理员 |
| winget 被组策略 `EnableAllowedSources` 封锁 | 识别 `BLOCKED_BY_POLICY`，报告为环境约束，**不重试**，并说明这是企业策略而非 osdk 故障 |
| winget 在 SYSTEM 上下文 | 官方明确不支持 CLI。doctor 直接报告不支持，并指向 `Microsoft.WinGet.Client` PowerShell 模块 |
| winget 未安装 / 未注册 / 版本过旧 | 三种状态分别报告（注意版本 < 1.6.3482 可能**无任何输出**，不能把空输出当作「无包」） |
| brew 需要 sudo（cask 的 pkg 产物） | 交互式：透传密码提示到终端。非交互：**报错并打印手工命令，绝不挂起等密码** |
| brew 未安装 | 引导用户安装（给出官方脚本与镜像加速的安装方式），**不自实现**（§3.3） |
| brew 处于非默认 prefix | doctor 报告 Tier 2/3 状态并解释影响（bottle 可能失效、issue 可能不予受理） |
| `no_elevate = true` | 任何需要提权的操作一律改为打印命令 |
| 离线 / `--offline` | 复用现有 offline 语义：不探测、不刷新 source、只做本地状态查询 |

**共同原则（借鉴 mise，与 osdk 现有姿态一致）**：宁可打印一条用户可以自己复制执行的命令，也不要自行提权或静默挂起。

---

## 10. 对 lockfile 的影响

**结论：系统包不进 `osdk.lock`。**

这不是偷懒，而是因为**基底不支持可复现性**，写进去会是虚假承诺：

- **Homebrew 是 rolling release**，官方明确「不支持安装任意旧版本」，且「`brew bundle` 不会、也永远不会有 `Brewfile` lock file 的概念来 pin 版本（不像 `package-lock.json` 或 `Gemfile.lock`）」。
- **`brew pin` 不是锁定原语**：pin 期间收不到安全更新；**被 pin 的 formula 会阻塞其他 formula 的安装或升级**（因为 Homebrew「不支持任意混搭 formula 版本」）；pin 的 cask 仍可能自行更新。
- **winget 的历史版本依赖上游 URL 存活**：manifest 保留在仓库里，但 `InstallerUrl` 指向上游原始地址，上游删版本后安装即失败（已有 Package Issue 报告此类失效）。
- **winget 的 pin 也不是锁定**：官方明确「被 pin 的包仍可能自行升级，也可能通过 Windows Package Manager 之外的途径被升级」。

**osdk 的做法**：

- `[syspkg.packages]` 中的版本值是**期望值**，语义是「装的时候按这个要求」，不是「锁定在这个版本」。
- `osdk pkg status` 报告**实际观测到的版本**，与期望值不符时如实标注，但**不试图强制收敛**。
- 对无法安装指定 pin 的情形，**跳过并警告**（借鉴 mise 的诚实分档做法），而不是失败或假装成功。
- 在 `osdk.lock` 的文档中**明确写出「系统包不在锁定范围内」**，避免用户误以为 lockfile 覆盖了全部依赖。

来源：https://docs.brew.sh/Brew-Bundle-and-Brewfile ，https://docs.brew.sh/Versions ，https://docs.brew.sh/FAQ ，https://learn.microsoft.com/en-us/windows/package-manager/winget/pinning ，https://github.com/microsoft/winget-pkgs/issues/128758

---

## 11. 分阶段落地

每个阶段是**一次可独立提交的行为变更**，与项目规范「每个提交只聚焦一处行为变更，并包含它的测试和直接相关的文档」对齐。每阶段都需同步更新 `README.md`、`README.zh-CN.md`、`site/guide/`、`site/en/guide/`、`site/.vitepress/config.mts`（中英文成对），并跑 VitePress 生产构建暴露失效链接。

**Phase 0：验证待确认项（不产生提交）**
先完成 §12 中标记为「阻塞实现」的验证项（V-2、V-8 优先）。这些结论直接决定 Phase 1 与 Phase 4 的解析层与幂等逻辑形状，先写代码会返工。

**进度**：V-8 已于 2026-09-12 在 Windows 11 / winget 1.29.290 上实测完成（§8.4.1），Phase 1 的阻塞已解除，并附带发现「输出本地化」这一文档此前未预见的风险；V-5 部分解决。V-2 属 Homebrew 侧，需 macOS 实机，阻塞的是 Phase 6 而非 Phase 1。

**Phase 1：只读发现与诊断**
`syspkg/{mod,manager,discovery,winget,brew,report}.rs` 骨架 + `osdk pkg doctor [--json]`。
只做发现：管理器是否存在、版本、可用性、当前 source/prefix、权限与策略状态。不读配置、不装任何东西。
验收：在装有/未装 winget 的 Windows 与装有/未装 brew 的 macOS 上，doctor 都给出可操作的结论；`--json` 输出确定性且 schema 版本化。

**Phase 2：镜像探测与只读计划**
`syspkg/mirror.rs` + `osdk pkg mirrors test`。
复用 `source::select::{effective_sources_for, ranked_source_candidates_for}`，不新建探测机制。内置镜像候选集（USTC/NJU/华为云 for winget；TUNA/USTC/阿里云 for brew）。**明确区分索引加速与产物加速**（§5.1）。官方端点一并参与测速，使「没有镜像值得切换」成为一个可以得出的结论。
**已完成**（提交 `dbdc74b`）：`osdk pkg mirrors test` 含 `--json`；四个候选实机全部可达，华为云 11.5 MiB/s 对官方源 1.1 MiB/s；`--offline` 拒绝而非返回未测排名；osdk +0.12%，osdk-shim 字节不变。

**Phase 3：osdk 自身调用时自动选用最快源（L2，不提权）** — 已实现，但实测后确认对 winget **不产生加速作用**（见 §8.4 的并存不可能一条）。保留该层的价值在于：它正确处理了用户 pin、拒绝把内置 id 当源名传出去、并在选中官方源时省略参数；对 Homebrew 仍然适用。winget 的加速改由 Phase 3.5 的顶替动作达成。
这是「安装依赖时自动检测并设置镜像」的落点，且不改宿主任何状态，因此排在需要提权的 L3 之前。
osdk 发起的 winget 调用自动附加 `--source <name>`，选实测最快的**已注册**源；探测走 Phase 2 的缓存与 TTL，不给每次安装都加一轮测速。
关键约束：无可用镜像源、或探测数据过期且当前离线时，**省略该参数**退回 winget 默认行为，绝不猜一个源名——传未注册的源名会让 winget 直接报 `0x8A150012`，把本可成功的安装变成失败。
验收：用户手敲 winget 的行为完全不变；无管理员权限也能工作；离线时不报错、只是不加速。

**Phase 3.5：镜像配置的确认式写入（L3，需管理员）**
`syspkg/apply.rs` + `osdk pkg mirrors apply [--dry-run] [--accept-plan <ID>]`。**不设独立的 `plan` 子命令**：预览是 `--dry-run` 的职责，与执行共用同一套计划生成代码，避免「预览过的计划与实际执行不一致」这类分叉。
首次注册需用户确认指纹，并在确认前显式列出**镜像源拿不到 `StoreOrigin` 信任标记**这一影响（§9）；此后同一目标可自动维护，不再打扰。
Homebrew 侧优先写 `brew.env` 而非 shell profile；winget 侧需管理员，无权限时打印命令而不尝试提权。保证 `HOMEBREW_BOTTLE_DOMAIN` 与 `HOMEBREW_ARTIFACT_DOMAIN` 互斥。

**Phase 4：声明式配置与状态查询**
`[syspkg]` / `[syspkg.packages]` schema + `trust.rs` 集成 + `osdk pkg status [--json] [--missing]` + `osdk pkg plan [--detailed-exitcode]`（这条 `plan` 管的是**包安装**计划，与镜像配置无关——镜像侧的预览已合并进 `mirrors apply --dry-run`）。
仍然**不安装任何东西**，只对照配置报告状态。

**Phase 5：winget 侧 apply**
`osdk pkg apply`，先只支持 winget（退出码语义最明确、无 sudo 交互复杂度）。
含完整的 HRESULT 白名单分类（§8.4）、命令行记录、`--dry-run`、不提权原则。

**Phase 6：Homebrew 侧 apply**
brew formula 与 cask 支持。cask 的 sudo 透传与非交互拒绝是本阶段的主要复杂度。

**Phase 6.5（可选）：Linux 发行版包管理器的只读检测**
apt / apk / pacman（可含 dnf）的**只读**检测与建议命令，**不实现 apply**（§7.3）。
排在 winget/brew 之后而非并列，理由是：它是一个纯增量的只读能力，不依赖前序阶段的 apply 机制，但也不应抢在两个主目标之前——用户诉求的起点是 winget 与 Homebrew。
内容限于：用各家稳定接口做状态查询（`dpkg-query -W -f=`、`apk info -e -v` 或 `apk query --format json`、`pacman -Q`/`-T`），doctor 报告，镜像可达性探测与**建议**（不写入系统源文件），以及打印可复制的安装命令——在 Arch 上必须提示改用 `pacman -Syu` 而非单包安装（§7.3）。
验收：在 Debian/Alpine/Arch 容器内，status 与 doctor 均不触发任何提权、不修改任何系统文件；建议命令可被用户直接复制执行。

**Phase 7：文档与 CI 收口**
VitePress 用户指南（`guide/system-packages.md` 中英成对）与实现说明（`guide/implementation/system-packages.md` 中英成对）；侧边栏与导航更新；README 增加用法段落（**只写用法，架构与取舍放实现说明章节**）。
体积回归验证（两次独立 cargo 调用，§6.3）；`scripts/windows-wine-tests.sh` 全量 Windows GNU 测试套件。

---

## 12. 待验证清单

实现前必须逐项确认。标注「**阻塞**」的会改变代码形状，应在对应阶段动工前完成。

| # | 待验证 | 验证方法 | 影响 |
| --- | --- | --- | --- |
| V-1 | `HOMEBREW_PREFIX` 作为**输入**能否决定安装位置 | 读 `https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh` 检索赋值逻辑 | 低（即使可以本设计也不采用，§4.1） |
| V-2 | `brew install <已装且最新>` 的退出码 | 实机 `brew install hello; echo $?` | **阻塞 Phase 6**（决定幂等判定） |
| V-3 | winget 对 MSIX 传 `--location` 是静默忽略还是报 `UNSUPPORTED_ARGUMENT` | 实机 `winget install --id <MSIX 包> -l C:\tmp\x -e`，记录退出码与 verbose 日志 | 中（决定 §4.2 的拒绝逻辑精度） |
| V-4 | 华为云是否提供官方 winget 帮助页与推荐命令 | 访问 `https://mirrors.huaweicloud.com/home` 搜索 winget | 低（决定是否纳入内置候选集） |
| ~~V-5~~ | ~~镜像 source 除 `--trust-level` 外的签名/证书校验差异~~ | **部分已验证（§8.4.1）**：`winget source export` 以结构化字段直出 `TrustLevel`（内置 winget 源为 `["Trusted","StoreOrigin"]`），无需解析文本。**仍待验证**：接入第三方镜像源后该字段的实际取值与证书校验差异 | 中（影响 §8.6 的信任告知内容） |
| V-6 | 阿里云两个 API 路径的新鲜度是否一致、是否其一为遗留别名 | 分别取 `formula.jws.json` 比对 `Last-Modified`/内容哈希 | 低 |
| V-7 | 阿里云 homebrew 镜像是否仍活跃同步 | 对比其 `formula.jws.json` 与 `formulae.brew.sh` 同一 formula 的 stable 版本号 | 低 |
| ~~V-8~~ | ~~`--no-progress` 支持哪些子命令、自哪个版本引入、能否完全消除 stdout 污染~~ | **已验证，阻塞解除（§8.4.1）**：winget 1.29.290 实测重定向后 ESC=0，该标志已从七个子命令的帮助中移除但仍被接受；改用 `--disable-interactivity` + `--nowarn`。**附带发现**：人类可读输出为界面语言本地化，须改走 JSON 通路 | ~~阻塞 Phase 1~~ → 已解除 |
| V-9 | mise winget manager 传递的确切 flag 组合 | Windows 实机 `mise bootstrap packages apply --dry-run --manager winget` 读取记录的完整命令行；或读 mise 源码 `system/` 下实现 | 低（仅作参照，不构成 osdk 的依据） |
| V-10 | Windows Server 2019/2022 与 LTSC/无 Store 镜像上 App Installer 的安装步骤与受支持程度 | 干净环境执行 `Add-AppxProvisionedPackage -online -PackagePath <msixbundle> -LicensePath <license> -DependencyPackagePath <VCLibs>`，登录后 `winget --info` | 中（影响 doctor 的引导文案） |
| V-11 | Rust 侧是否存在等价 ruby-macho `MachO.codesign!` 的 ad-hoc 签名实现（不依赖 CLT、不牵入 XAR/CMS/X.509 全链） | 检索 crates.io 具备 Mach-O 写入与 code signature 构造能力的 crate；若有则按 §6.4.1 测净增量 | 低（当前设计不依赖；仅在重新考虑自实现时需要） |
| V-12 | `codesign` 在无 CLT 环境下的确切失败形态；`codesign_allocate` 是否为真正缺失的组件 | 干净 macOS（`xcode-select -p` 报错）上 `/usr/bin/codesign --sign - <binary>; echo $?` 记录 stderr 与退出码；`file /usr/bin/codesign` | 低（同上；结论已足以否决 shell out 方案） |
| V-13 | 主流发行版**发行内核**是否启用 `CONFIG_IMA_APPRAISE=y`（上游默认 `n` 已确证） | `zgrep -E 'CONFIG_IMA_APPRAISE\|CONFIG_EVM' /proc/config.gz` 或 `/boot/config-$(uname -r)`；查 `/sys/kernel/security/ima/policy` | 低（代理路线不改写 ELF，不受影响） |
| V-14 | `--root`/`--instdir` 安装下动态链接器路径的实际表现（「是否真能得到可用软件」的最硬证据） | `dpkg --instdir=/tmp/root --admindir=/tmp/root/var/lib/dpkg --force-script-chrootless -i coreutils_*.deb`；`readelf -l /tmp/root/bin/ls \| grep interpreter`；直接执行观察是否报 `No such file or directory` | 中（强化 §7.2 结论，但不改变方向） |
| V-15 | zypper 在 §7.1 九个维度上是否带来 apt/dnf 之外的新增信息量（线索：btrfs snapshot + snapper 集成、`--non-interactive`、成文退出码表） | 读 `man zypper`（https://en.opensuse.org/SDB:Zypper_manual ）确认 EXIT CODES 节；读 openSUSE snapper 文档确认与 zypper 的集成方式与默认状态 | 低（决定是否把 zypper 纳入只读检测清单） |

---

## 13. 与现有文档的关系

- **`docs/package-registry-design.md`**：确立了「工具自身的 download source」与「项目依赖 registry」的边界。本文引入**第三条链路**——「宿主系统包管理器的 source/mirror」。三者必须在用户文档中画清：`osdk source`（osdk 下载 SDK 用什么源）、`osdk registry`（项目依赖从哪个 registry 拉）、`osdk pkg mirrors`（宿主包管理器用哪个镜像）。
- **`docs/research/mise-dev-tools-backends-2026-08-28.md`**：覆盖 mise 的 **dev-tools backend 子系统**。本文覆盖的是 mise 刻意放在该子系统**之外**的 bootstrap packages，两者互补，不重叠。
- **`docs/research/docker-image-acceleration-cache-2026-08-28.md`**：容器镜像加速研究，其探测与计划/确认写入模式是本设计在 §5.4 / §5.5 直接参照的内部先例。
- **`site/guide/implementation/sources-registries.md`**：实现落地后，镜像探测复用的说明应在此处补充交叉引用。
