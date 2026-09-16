# 系统包管理器的检查机制

`osdk pkg` 面对的是宿主自己的包管理器——winget、Homebrew。这类东西和 SDK 不同：
osdk 不拥有它们，只能询问。本页说明这个子系统为什么长成现在的样子。

## 为什么不是一个 backend

最直接的想法是给 winget 写一个 `Backend` 实现，但这条路会付出与收益不相称的代价。

`Registry::new()` 会把全部 backend 实例化为 `Arc<dyn Backend>`，于是 trait 上的
每个方法都进入 vtable，链接器无法证明它不可达、也就无法裁剪。历史上这件事让
`osdk-shim` 白背过 5.15 MB——同样 13 个 backend，`dyn` 分发 7.02 MB，静态分发 1.83 MB。

更根本的是概念不匹配：backend 描述的是"osdk 能安装和切换版本的 SDK"，而系统包管理器
是"osdk 只能询问的宿主设施"。`container/` 子系统面对 Docker、containerd、BuildKit 时
做过同样的判断，刻意不实现 `Backend`。`syspkg/` 与它同构。

实测结果印证了这个选择：新增该子系统后 `osdk` 从 11.92 MB 增至 11.95 MB（+0.25%），
`osdk-shim` **保持 3.5 MB 不变**。

## 为什么门控在 `install` feature 之后

shim 的职责是启动工具，永远不需要知道 winget 在做什么。因此 `syspkg` 模块整体挂在
`#[cfg(feature = "install")]` 之后，与 `self_update`、`verification` 同列。这保证
子系统连同它的进程探测代码完全不进入 shim 的构建。

验证时必须**分两次独立调用** cargo：

```powershell
$env:CARGO_TARGET_DIR="target\size-check"
cargo build --release -p osdk-cli
cargo build --release -p osdk-shim
```

单次 `--workspace` 构建会统一 feature，把 `install` 重新打开，测出的 shim 体积是错的。

## 为什么不解析包管理器的表格输出

这是本子系统最重要的一条设计约束，也最容易被低估。

在简体中文的 Windows 上，`winget list` 的列头是「名称 / ID / 版本 / 可用 / 源」，
`winget source list` 是「名称 / 参数 / 显式」。**按英文列头编写的解析器在那里不会报错，
而是静默失效**——找不到列就当作"包未安装"。这比崩溃危险得多。

因此输出层按三级优先设计，且**不含任何列头字面量**：

1. **退出码**。这是唯一与语言无关的通道：查询无匹配在任何界面语言下都是
   `0x8A150014`。因此分类只看 `exit_code`，不看消息文本。
2. **JSON 通路**。`winget source export` 直出 JSON Lines，键名为英文且不随界面语言
   变化，还带 `TrustLevel` 结构化字段；`winget export -o` 输出 schema 2.0 的包清单。
3. **不再有第三选择**。表格解析被排除在外。

`report.rs` 的测试用中文字面量做反向断言，确保这些文本永远不会泄漏进机器可读输出。

## 为什么不传 `--no-progress`

设计初稿假定需要 `--no-progress` 抑制 stdout 的进度污染。实机验证（Windows 11,
winget v1.29.290）推翻了这个假设：

- 重定向到文件后，加与不加该标志的输出**逐字节相同**，均无 ESC 序列。
- 该标志已从 osdk 调用的七个子命令的帮助文本中**移除**，但仍被静默接受。

依赖一个既无文档保证、又无实际收益的标志是纯粹的负债。调用约定改用
`--disable-interactivity` 与 `--nowarn`，两者都在当前文档化的选项列表中。
`winget.rs` 有一条测试专门断言 osdk 不会传 `--no-progress`。

## 探测的边界

- **只读**。发现阶段运行两条查询命令，不安装、不改配置、不提权。
- **有超时上限**。用户在等，因此无响应的管理器必须超时失败而不是挂起。
- **可注入**。探测通过 `CommandRunner` 执行，测试用脚本化的 runner 覆盖各种宿主状态，
  无需真的装上 winget。
- **记录目的而非命令行**。`ProbeRecord` 存的是探测目的与退出码，不存参数，
  这样既不会随 flag 变化而失效，也不会泄漏参数内容。

## 镜像探测为什么复用 `source::select`

`osdk pkg mirrors test` 没有自建测速逻辑，而是走 `source::select` 的
`effective_sources_for` 与 `refresh_with_timeout`，用伪工具 id `pkg:winget-source`
占位。

这两个入口本就是为"不是 backend 的下载者"准备的——osdk 自更新是第一个使用者。它们
已经处理好了 pin、探测缓存与指纹、`--refresh-sources`、离线模式、一次性 `--source`
覆盖。自建一套测速会立刻在这些语义上产生分叉，而 `select.rs` 的文档里写得很直白：
这条路存在的目的就是防止"非 backend 的下载者漂移出自己的一套镜像策略"。

`pkg:` 前缀把这些伪 id 与真实工具隔开，因此 `osdk source` 对某个工具的配置永远不会
与包管理器的镜像配置撞名。

### 为什么探测超时是 12 秒而不是默认的 1.5 秒

默认 `probe_timeout_ms` 是为版本索引那种小文件调的。winget 的源探测拉的是
`source.msix`——一个真实的索引包。在 1.5 秒窗口下所有候选都会超时、全部被判为
不可达，排名于是静默退化成固定优先级顺序，**与"测出哪条线路更快"正好相反**。

`self_update` 早先遇到过同一个问题（实测某 CN 代理仅首字节就要 6.1 秒），这里沿用
它的结论。探测是并发的，所以 12 秒是整条命令的上界，不是每个源各等 12 秒。配置若把
`probe_timeout_ms` 调得更高会被尊重，但不会低于这个下限。

## 为什么要区分"加速索引"和"加速产物"

`Acceleration` 这个枚举存在的唯一理由，是 winget 与 Homebrew 在结构上根本不同：

- winget 的源是一份 manifest 索引，manifest 里的 `InstallerUrl` 指向各厂商自己的
  服务器。镜像**只能**加速找包，对下载安装包毫无作用。
- Homebrew 的 bottle 集中托管，镜像可以整体改写，两段都能加速。

把这件事藏起来的代价很具体：用户为了解决"下载慢"而换源，换完发现毫无变化，会认定
这个功能是坏的。所以 `MirrorCandidate` 如实标注每个候选加速的是哪一半，人类可读输出
在全部候选都是 `IndexOnly` 时追加一段说明。`pkg.rs` 里有一条测试断言这段文字必须出现。

同理，`acceleration_of` 对 Homebrew 返回 `None` 而不是猜一个值——Homebrew 尚未接入，
编造一个取值就是撒谎。

## 候选集里为什么包含官方源

官方端点与镜像一同参与测速。这不是冗余：**"没有镜像比官方源更快"是一个真实且有用的
结论**，尤其对中国大陆以外的用户。只测镜像的排名会把用户往换源的方向推，哪怕换了更慢。

候选集本身也经过取证。广为流传的 TUNA 与腾讯云 winget 源实测均为 404，两者都不在
列表里，`mirror.rs` 有一条测试锁定这个事实。那篇流传的腾讯云教程还给出了并不存在的
`winget source pin` 命令，整份资料不可采信。

未被运营方文档记录的端点（nju、huaweicloud）在无实测数据时排序靠后：它们今天可用，
但没有承诺，如实说明比默默排前更诚实。


## 选源：两个命名空间与一个排他性

osdk 调用 winget 时自动选用最快的源（`preferred_winget_source`），这里有两个坑，
都会以「看起来正常」的方式出错。

### 内置镜像 id 与宿主注册名是两套命名空间

osdk 内部的镜像 id 是 `ustc` / `nju` / `huaweicloud`，而 `winget --source` 只接受
宿主**已注册**的 `Name`。两者可能长得像，但绝不能假设相同——实测传一个未注册的源名，
winget 以 `0x8A150012` 直接失败，一次本可成功的安装就变成了错误。

因此匹配走 **endpoint 比对**而不是 id 比对：拿实测结果的 URL 去 `winget source export`
的注册列表里找同一个 endpoint，命中了才用它的 `Name`。比对时两侧都去掉尾部斜杠，
因为 `winget source add` 会原样保留用户输入的形式。

反方向同样有测试覆盖：路径相同但主机不同（`evil.invalid/winget-source`）**不算命中**，
否则会把 osdk 指向一个无关主机。

### 镜像无法并存，所以判据是「源名」而不是「endpoint」

这一条推翻了本模块的初版实现，值得记下推翻的过程。

初版按 `SourceKind::Official` 判断「要不要省略 `--source`」，隐含假设是：镜像会以
自己的名字注册，与官方源并存，因此「官方」和「镜像」是两个可区分的源。

以管理员身份实测否证了这个假设。新增一个指向 USTC 的
`Microsoft.PreIndexed.Package` 源，失败于 `0x80073D06`：

```
Operation failed: Windows 无法安装程序包
Microsoft.Winget.Source_2026.915.1105.46_neutral__8wekyb3d8bbwe，
因为它的版本为 2026.915.1105.46。已安装此程序包的更高版本 2026.915.1714.48。
```

对照 `winget source export` 可见官方源的 `Data` / `Identifier` 都是
`Microsoft.Winget.Source_8wekyb3d8bbwe`——**与失败信息里的包是同一个 MSIX 身份**。
这类源以固定身份安装，镜像分发的是同一个包的副本，所以第二个装不进去；又因镜像
同步滞后（`2026.915.1105.46` < 本机 `2026.915.1714.48`），Windows 以「已装更高版本」
直接拒绝。

这解释了镜像站官方教程为何是 `source remove winget` + `source add winget <镜像>`：
**顶替是唯一可行的形状**。

于是判据必须换掉。真正要问的不是「这个 endpoint 是不是官方的」，而是
**「这个源是不是已经是 winget 的默认源」**——而这由**名字**决定：名为 `winget` 的源
永远默认参与调用，无论它指向 Microsoft 的 CDN 还是某个镜像。按 endpoint 判断会在
顶替后失效：那时最快的源是镜像，代码会显式指定 `--source winget`，从而屏蔽 msstore。

`NoPreferredSource::AlreadyTheDefaultSource` 因此同时覆盖两种宿主：未顶替的（默认源
是官方 CDN）和已顶替的（默认源是镜像）。变异验证确认了这个判据被测住：把它换回按
endpoint 判断，测试立即变红。

**对 L2 层的结论**：`--source` 在 winget 上根本不是镜像加速的手段。顶替之前镜像不以
任何独立名字存在，无从选择；顶替之后镜像就是默认源，不需要指定。加速完全由顶替动作
达成。该层仍然保留，因为它正确处理用户 pin、拒绝把内置 id 当源名传出，且这正是
Homebrew 需要的形状——那侧通过环境变量按次选择镜像，不依赖共享的注册表。

### `--source` 是排他的，所以默认源必须省略而不是指定

指定一个源就屏蔽其余全部源。实测（winget 1.29.290）：`winget search --query WhatsApp`
能返回 msstore 的 WhatsApp，加上 `--source winget` 后该结果消失，**退出码仍是 0**。

于是「实测最快的是官方源」不能返回 `Ok("winget")`：那样会传一个毫无收益的参数
（它本就是默认），却让 msstore 独有的包变成「找不到」——一个不报错的功能损坏。
这种情况建模为 `NoPreferredSource::AlreadyTheDefaultSource`，语义是「无需指定」，
与「选择失败」在类型上区分开。

`NoPreferredSource` 的每个变体都对应一句不同的解释，因为补救方式不同：镜像未注册要去
注册，而用户 pin 生效根本不需要补救。笼统说一句「不可用」会让用户无法分辨。

### 变异验证

这套判定的测试做过变异验证，四个注入缺陷全部被捕获，其中包括最危险的那个——
把返回值从注册名换成内置 id（即上面 `0x8A150012` 那个 bug），被三条测试同时抓住。
按仓库既有的判据：没见过红色的测试，其绿色不构成证据。

## 应用镜像：为什么是三段而不是一步

`mirrors apply` 是这个子系统里唯一会改机器状态的入口，因此它的结构完全由风险决定。

### 顶替必然经过一个「没有源」的窗口

winget 不支持把一个源原子地重新指向别处，只能 `remove` 再 `add`。两条命令之间宿主
**没有任何软件源**，而最可能的失败恰恰发生在第二条：镜像落后于上游时，Windows 以
`0x80073D06` 拒绝安装更旧的包。

这决定了两件事必须同时做，缺一不可：

1. **能预测的就提前拒绝**——`assess_feasibility` 在动手之前比对发布时间。
2. **不能预测的就自动回滚**——任一命令失败即执行 `winget source reset`。

回滚用 `reset` 而不是「重新 add 官方地址」，因为 winget 自己知道内置源该是什么定义，
而 osdk 硬编码的 endpoint 会随上游变化而过期。回滚的成败**如实上报**：静默失败的回滚
是唯一一种绝不能告诉用户「没事」的结果。

### 可行性判据是发布时间，不是版本号

镜像不提供版本元数据。USTC 对 `/version` 如实返回 404；华为云对**任意**路径都返回
`200` 并吐同一个 11963 字节的 HTML 页——所以「请求成功」不等于「文件存在」，按状态码
判断会把 HTML 当版本号送进一个破坏性决策。

改用 `source.msix` 的 `Last-Modified`，一次 `HEAD` 即可，不必下载 20 MB。它不能换算成
版本号（`2026.915.1714.48` 的发布时间是 17:45 GMT，`1714` 是构建时刻），但与版本单调
同序，足够判新旧。另外响应的 `Content-Type` 为 `text/html` 时直接判为未知，这正是防
上面那种「200 + HTML」。

日期解析成有序字段而非按字符串比较：`Dec` 的字典序早于 `Sep`，但十二月更晚，字符串
比较会把更新的镜像判成过期、拒绝一次本该成功的应用。变异测试最初正是在这里发现缺口——
第一版用的日期恰好字符串序也正确，把比较换成字符串后测试仍全绿。

### 指纹确认的是「某个宿主状态」，不只是「某个计划」

执行前重新读取已注册的源并与计划的指纹比对，不一致就拒绝——与 `container/apply.rs`
对配置文件做的 `StaleInput` 检查同构。「指纹不符」与「宿主已变」是两种不同的拒绝：
前者是复制粘贴过期，后者是机器在你确认之后被改动，补救方式不同，所以措辞也不同。
两者都不会执行任何一条命令。

### 退出码：没做成就不能报成功

`--dry-run` 打印完计划即为 0，因为报告本身就是被请求的产物。但**请求了 apply 而最终
没有应用**——无可用镜像、未确认、指纹不符、宿主已变、执行失败——一律非 0。否则脚本会
把「什么都没做」读成成功，而这正是最难发现的一类缺陷。

## Linux 检测为什么是只读的，以及怎么验证它

### 姿态由三个事实决定，不是保守

apt/apk/pacman/dnf 与 winget/brew 结构不同，因此在类型上就分开：它们放在报告的
独立字段 `distro_managers` 而非并入 `managers`。如果合在一起，消费者就可能把一条 apt
记录当成 osdk 能安装的目标，而它刻意不是。

三个事实各自独立地否决了「代为执行」：

1. **变更全局且需 root。** apt 手册写明 `full-upgrade`「会删除已安装的包，如果这是
   整体升级系统所必需的」。
2. **Arch 声明部分升级不受支持。** 而「只装项目需要的那个包」就是部分升级。因此
   `install_command` 对 pacman 打印的是 `-Syu --needed` 而非 `-S`——给用户一条其发行版
   官方不支持的命令，比不给更糟。有测试专门锁定这一点。
3. **回滚能力三档分裂。** `RollbackAbility` 因此是 per-manager 的枚举而不是一个布尔：
   dnf 事务性、pacman 只能从 cache 手工降级、apt 只有日志。一个统一的答案会是假话。

### 查询接口选择的是「格式由 osdk 指定」的那些

`dpkg-query -W -f=${Version}` 与 `rpm -q --qf` 都显式给出格式串，因此解析目标是 osdk
自己定的，不随发行版默认格式变化；`pacman -Q` 与 `apk info -e -v` 的输出形状是文档化
且不本地化的。有一条测试断言**任何查询命令都不可能安装东西**——即不含 `install`/`add`/`-S`，
也不以 `sudo` 开头。

apk 的输出 `name-version` 解析有个坑：包名自身可能含连字符（`py3-foo-1.2-r0`）。
按第一个连字符切会把 `foo-1.2-r0` 当成版本号，所以切点是**第一个后面紧跟数字的连字符**，
并有专门用例覆盖。

### 平台判断作为参数，否则关键分支在开发机上永不执行

`detect` 里若直接写 `cfg!(target_os = "linux")`，那么在 Windows 上开发时**唯一有意义的
分支永远不会被测到**。因此实际逻辑在 `detect_for(runner, limits, is_linux)`，`cfg!` 只留
在 `detect` 这一层。Debian、Arch、Fedora 三种宿主形态因此都能在 Windows 上验证。

### 单元测试证明不了的那一件事，用真容器补

单元测试用脚本化 runner 覆盖分支，但它证明不了**这些查询接口真的存在且输出符合预期**。
`dpkg-query` 某个 flag 变了，或 `apk info` 输出形状与文档不同，会让所有测试全绿而所有
报告出错。

所以 `scripts/linux-distro-detection.sh` 在真实的 Debian / Alpine / Arch / Fedora 容器里
跑 `osdk pkg doctor --json`，逐个断言：该发行版自带的管理器被报为存在、不自带的不被误报、
输出中不出现 `sudo`。它 grep 的是 JSON 而非人类可读输出（后者会本地化），且**有一条单元
测试锁定那个 JSON 字面形状**——否则字段改名会让这个脚本静默地什么都匹配不到、却依然通过。

没有 docker 或 podman 时脚本输出跳过原因并以 0 退出，不会让本机没有容器运行时的开发者
构建失败。

## 平台缺失与安装缺失是两回事

`ManagerStatus` 把 `NotApplicable` 和 `NotInstalled` 分开，因为两者的处置完全不同：
macOS 上没有 winget 不是需要修的问题，doctor 不应建议用户去安装。报告结构里所有
已知管理器恒定出现，不随宿主变化——消费者因此可以依赖固定的输出形状。
