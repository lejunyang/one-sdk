# Android SDK 工具

本页覆盖通过 osdk 管理 Android SDK 包：NDK、platform-tools（`adb`、`fastboot`）、
build-tools、cmdline-tools、CMake、platforms 等。

osdk 直接解析 Google 官方仓库清单并从 Google 服务器下载，不代理、不转存归档文件，
定位与 Android Studio 内置的 SDK Manager 相同。

## 工具名

每个包族对应一个 backend，形如 `android-<族名>`：

| 工具名 | 提供的可执行文件 | 说明 |
| --- | --- | --- |
| `android-platform-tools` | `adb`、`fastboot` | 单包族，版本即修订号 |
| `android-ndk` | clang/lld 工具链 | side-by-side 布局 |
| `android-build-tools` | `aapt2`、`d8`、`apksigner`、`zipalign` | — |
| `android-cmdline-tools` | `avdmanager`、`lint`、`retrace` | — |
| `android-cmake` | `cmake` | NDK 构建用 |
| `android-platforms` | `android.jar` | 无可执行文件 |
| `android-emulator` | `emulator` | — |
| `android-sources` | — | 源码，无可执行文件 |
| `android-system-images` | — | 模拟器磁盘镜像，无可执行文件 |

```bash
osdk list-remote android-platform-tools
osdk install android-platform-tools@37.0.1 -o accept-licenses=true
osdk exec -t android-platform-tools@37.0.1 -- adb version
```

已被 side-by-side 布局取代的 `ndk-bundle` 未纳入，以免与 `android-ndk` 混淆。

复数形式的 `emulators` 也未纳入，且它并不是 `android-emulator` 的另一种写法。
按线上清单实测，两者有五处不同：`emulators` 以 `emulators;<build-id>` 形式
side-by-side 版本化、把 `emulator` 本身声明为依赖、Windows 包体积为 273 MB 而非
421 MB、文件名是 `emulator_windows_x64-*` 而非 `emulator-windows_x64-*`、且只存在
于预览渠道。它是叠加在单数包之上的增量组件，因此版本号之间甚至无法直接比较：
`emulators;latest` 为 37.1.2，而 stable 的 `emulator` 已是 37.1.11。


## 许可协议

绝大多数 Android 包必须先接受许可协议。osdk 默认**不会**替你接受，需显式传参：

| 选项 | 作用 |
| --- | --- |
| `accept-licenses=true` | 接受本次请求涉及的全部协议 |
| `accept-license=<id>` | 只接受指定协议，可用逗号分隔多个 |

```bash
# 接受全部
osdk install android-ndk@29.0.14206865 -o accept-licenses=true

# 只接受指定协议
osdk install android-ndk@29.0.14206865 -o accept-license=android-sdk-license
```

按 id 接受**不会**扩散到其他协议：接受了 `android-sdk-license` 不等于接受
`android-sdk-preview-license`，因此预览版包仍会被拦下。

未接受时安装会失败，并直接给出可用命令，不会下载任何字节。

### 查看与导出

```text
osdk android licenses show TOOL[@VERSION] [--digest-only]
osdk android licenses status
osdk android licenses export --sdk-root DIR
```

```bash
# 查看协议全文与当前是否已接受
osdk android licenses show android-ndk@29.0.14206865

# 只看 id 与摘要
osdk android licenses show android-ndk@29.0.14206865 --digest-only

# 已接受哪些协议
osdk android licenses status

# 导出给 Gradle 复用
osdk android licenses export --sdk-root /path/to/sdk
```

接受记录写在 `<installs>/android-sdk/licenses/<协议 id>`，内容是协议全文的 SHA-1，
与官方工具及 Gradle / AGP 的格式一致。导出后把该目录交给 Gradle 即可避免重复提示。

摘要始终从**当前**清单实时计算。社区与 CI 配方中流传的固定哈希是旧版协议文本的
快照，Google 修订措辞后即失效；osdk 不硬编码这些值。因此协议文本更新后，旧的接受
记录会被视为未接受并重新要求确认——这与官方工具行为一致。

### 锁文件与团队协作

许可接受**不会**写入 `osdk.lock`。锁文件会提交并在其他机器上回放，把接受记录进去等于替
队友同意 Google 的协议，因此它只记录制品与校验和，接受由每台机器各自给出：

```bash
# 已提交 osdk.lock 的仓库，队友首次安装
osdk install                            # 被拦下，并提示需要接受
osdk install -o accept-licenses=true    # 显式同意后，安装锁定的制品
osdk install                            # 之后照常，接受已记录在本机
```

仅传接受选项时锁文件照常生效（接受不是制品选择项）；一旦混入 `channel` 等真正影响
选型的选项，就回到"绕过锁文件"的既有行为。

## 渠道

清单把包分在 stable / beta / dev / canary 四个渠道。osdk 默认只安装 stable，
非 stable 需显式选择渠道：

```bash
osdk install android-ndk@30.0.16138531 -o channel=beta
```

注意渠道与协议是两件独立的事：某些包位于 stable 渠道，但仍使用预览版协议，
这类包依然需要接受 `android-sdk-preview-license`。

## 校验强度

Android 清单只为每个归档提供 **SHA-1**，不提供更强摘要。这弱于 osdk 其他下载源，
是该源的显式例外：

- osdk 始终按清单声明的 SHA-1 校验，不会跳过校验安装；
- 其他 backend 的校验强度不受影响，仍要求 SHA-256 或更强；
- 该弱点在代码中显式标注，不会被误当作强摘要使用。

## 下载源

默认从 Google 官方仓库下载，并附带一个可用的公共镜像：

| 源 | 地址 |
| --- | --- |
| `google`（官方） | `https://dl.google.com/android/repository/` |
| `tencent`（镜像） | `https://mirrors.cloud.tencent.com/AndroidSDK/` |

镜像清单与官方逐字节一致（SHA-256 相同），因此清单内嵌的 SHA-1 在镜像场景同样有效。
实测官方直连吞吐反而更高，故镜像排在其后；实际顺序仍由
[下载源选优](./sources-security)动态决定。

## 共享命令名

有两个包族会提供同名的 R8 启动器：`build-tools` 与 `cmdline-tools` 都带
`d8`、`r8`、`retrace`、`resourceshrinker`。两个族都装时，这些名字由
`build-tools` 提供的那份接管——它才是构建实际调用的副本；`cmdline-tools`
独有的 `sdkmanager`、`avdmanager`、`lint` 等则照常生成。

其余同名情况仍按冲突处理并报错，需要你自行取舍。

## 挑选要生成的 shim

默认给工具暴露的每个可执行文件都生成 shim。个别 SDK 确实很大——一个 NDK 就有
172 个可执行文件（每个 API level 一个 clang 包装器）——但那是它真实的形状，
默认隐藏会破坏"按 API level 选编译器"的正常用法，所以收窄是可选项。

在配置里按需排除或限定：

```toml
[settings.shims]
exclude = ["apkanalyzer", "*-clang"]
```

`include` 非空时只生成匹配的名字，`exclude` 最后生效，因此可以先放宽再修剪：

```toml
[settings.shims]
include = ["*"]
exclude = ["d8"]
```

模式支持 `*` 与 `?`，忽略大小写。加上 backend 前缀可以只收窄某一个工具，
而不必逐个列出它的可执行文件：

```toml
[settings.shims]
exclude = ["android-ndk:*"]
```

排除只是不生成 shim，工具本身仍然装着，激活 shell 后依旧在 PATH 上，
`osdk exec` 也照常可用。改完执行 `osdk reshim` 生效，用
`osdk config list` 可以查看当前取值。

两个族共享的命令名（见上）由排除后仍在场的那一方接管：若排除了
`android-build-tools`，`d8` 就转由 cmdline-tools 提供。

## Java 运行时

Google 的 Android 包**不含 JDK**：`sdkmanager`、`avdmanager`、`d8`、`lint`
等是包在 jar 外面的启动脚本（仅 `cmdline-tools` 就有 125 个 jar），包里没有
`java`，缺少 JDK 时会直接退出。

osdk 会自动补上：运行这类工具时，若环境里还没有 `JAVA_HOME`，就使用 osdk
管理的 JDK——优先当前目录选定的版本，否则取最新的一个已安装版本。

```powershell
osdk install java
osdk install android-cmdline-tools -o accept-licenses=true
sdkmanager --version   # 无需先激活，也无需手动设 JAVA_HOME
```

已有的 `JAVA_HOME` 一律不动，无论它来自 shell 激活还是你自己设置，
因此可以用系统 JDK 覆盖。未安装任何托管 JDK 时，工具仍会报它自己的缺少
JDK 错误，此时装一个 `java` 即可。

`maven`、`gradle`、`kotlin` 同理。

## 环境变量

| 工具 | 导出变量 |
| --- | --- |
| `android-ndk` | `ANDROID_NDK_ROOT`、`ANDROID_NDK_HOME` |
| 其他 Android 包 | `ANDROID_SDK_ROOT` |

`ANDROID_SDK_ROOT` 指向存放许可记录的共享 SDK 根目录，而非单个包目录，
以便 Gradle 等工具复用接受记录。

## 系统镜像与依赖解析

模拟器系统镜像发布在主清单之外，按厂商分布在各个子站点中。osdk 会把它们合并为
一个包族，因此一条命令即可列出全部：

```bash
# 五个子站点共 263 个版本：default、google_apis、
# google_apis_playstore、android-tv、android-wear
osdk list-remote android-system-images

osdk install "android-system-images@android-35;google_apis;x86_64" \
  -o accept-licenses=true
```

子站点清单只在该包族需要时才拉取。为每次 `osdk install adb` 都额外发起六个请求，
去换一份别处都不会读的列表，并不值得。

子站点清单中的 archive URL 是相对该清单自身目录的，而两个子站点可能发布同名文件
——`x86_64-35_r09.zip` 就在多个厂商目录下都存在。osdk 在解析时会把每个 URL 改写为
相对仓库根，因此不会从错误厂商的目录下载。

### 依赖

主清单中只有三个预览包声明依赖，而这里恰恰相反：`google_apis` 清单中的 56 条依赖
声明全部指向 `emulator`，多数还带最低修订号。osdk 会解析这个闭包并补装缺失的部分，
因此安装镜像即可得到可用的模拟器。

以下两条规则源于真实数据的形态：

- **许可门覆盖依赖。** 子站点引用了主清单从未提及的八份协议，包括
  `intel-android-sysimage-license` 和 `android-googletv-license`。同意镜像自身的
  协议，不能等同于同意一份从未向你展示过的厂商协议。
- **依赖按 stable 渠道解析。** `emulator` 发布了两条记录，而预览构建的修订号更高。
  若按「最新」选取，就会为用户根本没点名的包装上预览版，随后安装又会被自身的渠道
  校验拒绝。

若某条依赖指向 osdk 未策展的包族，则跳过并告警，而不是让安装失败：清单里的一条边
不足以成为拒绝一个本可正常安装的包的理由。

### 目录布局

Google 的工具要求共用一个 SDK 目录，而 osdk 把每个包装进各自的版本化目录。两者
可以同时成立：数据仍留在 osdk 放置的位置，再用目录链接把它发布到 Android 工具期望
的路径上——`system-images/android-35/google_apis/x86_64`、`platform-tools`、
`emulator` 等。全程不复制，因此 3.5 GB 的镜像只存一份。

Windows 上这个链接是 NTFS junction 而不是符号链接，因为 symlink 需要开发者模式或
提权，junction 两者都不需要，而 Android 工具只会穿越它。Linux 和 macOS 上就是普通
符号链接。

差异只集中在两处，且都已在真实 Linux 上验证：

- **判定"这是不是链接"**：junction 不会被 `is_symlink()` 报出来，所以 Windows 还要
  额外查 reparse-point 属性；其他平台 `is_symlink()` 就够。
- **删除链接**：`remove_dir` 能解除 junction，但 unix 上符号链接必须用
  `remove_file`——那里 `rmdir` 会以 `ENOTDIR` 失败。osdk 先试前者、失败再退到后者，
  因此一条代码路径覆盖两种平台。

卸载与清理逻辑依赖的其余行为在两边完全一致：指向目录的链接可与真实目录区分；删掉
目标后链接仍在但无法解析（这正是发现悬空条目的依据）；往已被占用的路径建链接会失败
而不是覆盖（unix 上是 `EEXIST`）；向上剪空目录会停在第一个非空目录。

如果目标路径上已存在一个真实目录——通常是 Google 自带 `sdkmanager` 装出来的包
——osdk 会原样保留并告警。接管该路径就意味着删除 osdk 从未拥有的数据。

### 模拟器的 SDK 根校验

模拟器判定一个目录是否为可用 SDK 根，只看它有没有 `platform-tools` 子目录，别无
其他。它先查 `ANDROID_HOME`，再查 `ANDROID_SDK_ROOT`，然后从自身位置逐级上推，
凡缺少该子目录的候选一律否决，最终报 `FATAL | Broken AVD system path`。

因此只含 `emulator` 与 `system-images` 的根目录仍然无效。创建 AVD 前请先安装
platform-tools：

```bash
osdk install android-platform-tools -o accept-licenses=true
```

当共用根目录尚不合法时，osdk 会在安装阶段告警——因为模拟器自身的报错只会指出根
目录，而不会说明缺了什么。

### 镜像源

镜像体积很大，用镜像站看起来很有吸引力，但实测吞吐并非如此：腾讯源提供的包与
Google 逐字节一致，速度为 4.38 MB/s，而直连为 5.54 MB/s，加速比 0.79 倍。既有的
源优先级本就会选择更快的一侧，因此没有为镜像单独添加处理。

有一点值得记住：镜像站不同步 beta 镜像。找不到 `x86_64-ps16k-37.2_r04.zip` 反映的
是这一缺口，而不是镜像站坏了——它的主清单与子站点清单哈希均与 Google 完全一致。

## 虚拟设备

`osdk android avd` 负责创建、列出和删除 AVD：

```bash
osdk android avd create pixel-35 --image "android-35;google_apis;x86_64"
osdk android avd create small --image "android-35;google_apis;x86_64" \
  --data-size 4G --sdcard-size 256M
osdk android avd list
osdk android avd delete pixel-35
```

`list` 会报告每个设备的系统镜像是否仍然存在——镜像被卸载后，AVD 看上去依然完好，
直到模拟器在它上面挂掉。

### 为什么不用 avdmanager

`avdmanager` 无法驱动 osdk 管理的 SDK 根目录。基于 cmdline-tools 23.0.0 实测：

- 它通过规范化自身 jar 路径并向上三级来定位 SDK。osdk 把 `cmdline-tools/latest`
  暴露为指向版本化安装目录的目录链接，于是这次上溯**穿过**链接，落在真实根目录
  之上一层。随后每个包都被报成处于 "inconsistent location"。
- `ANDROID_SDK_ROOT` 与 `ANDROID_HOME` 都无法覆盖这一行为，而 `create avd` 直接
  拒绝 `--sdk_root`——它的全局参数只有 `-s` 和 `-v`。
- 即便它成功，写出的 `image.sysdir.1` 也是**相对路径**
  （`system-images\android-35\google_apis\x86_64\`），只会相对它自己推导出的根
  解析，而那在这里是错误的目录。

也就是说，没有任何参数或环境变量能让它认同这套布局。osdk 直接写出它本该写的两个
文件——`config.ini` 和 `<name>.ini` 指针文件——硬件默认值取自 avdmanager 自己
生成的一份 `config.ini`，只是把镜像路径换成绝对路径。

### 路径必须是绝对的，且不含 `%`

模拟器会对 `image.sysdir.1` 做 `%VAR%` 环境变量展开。osdk 的版本化安装目录名是
百分号编码的，直接传入会得到：

```text
WARNING | Environment variable 61 is not set
WARNING | ...~v1~6E72692D35676F6C5F7073783636%34\ is not a valid directory.
FATAL   | Broken AVD system path.
```

——每个十六进制对都被展开成空。因此该值写的是 SDK 根下的桥接路径，它既是绝对路径
又不含 `%`。`create` 会拒绝含 `%` 的路径，而不是写出一份稍后才在模拟器内部失败的
配置。

### 这些链接是什么，以及包被卸载后会怎样

osdk 把每个族装进各自的版本化目录（`installs/android-ndk/27.3.13750724/`），但
Google 的工具不询问任何管理器装了什么——它们遍历一套固定布局，要求
`platform-tools/`、`emulator/`、`ndk/<版本>/` 和
`system-images/<api>/<tag>/<abi>/` 是**同一个根目录下的兄弟目录**，否则模拟器会报
`Broken AVD system path`。

为了既不放弃按族版本化、也不重复存储数 GB，osdk 把真实目录链接进这些工具期望的
布局：Windows 上用 **junction**（symlink 需要开发者模式或提权，junction 两者都不
需要，而这些工具只会穿越它），其他平台用符号链接。载荷只存一份，SDK 根目录是它的
一个视图。

`osdk uninstall` 会**先删链接、再删载荷**，并顺带清掉因此变空的骨架目录。顺序很
关键：先删载荷会让链接变成悬空，而悬空的 junction 对模拟器、`avdmanager`、Gradle
的 `sdk.dir` 所做的存在性检查<b>依然回答"在"</b>——于是包看起来装着，却在更深处失败，
报出的错还指向 SDK 而不是那次卸载。

不是 osdk 创建的真实目录，两条路径都不会删，只会报告出来——它要么是 `sdkmanager`
自己的副本，要么是你的数据。

对于旧版 osdk 留下的、或包被 osdk 之外的手段删掉而留下的链接：

```bash
osdk android sdk-root show     # 报告悬空链接，不做任何改动
osdk android sdk-root repair   # 删除它们，并补齐缺失的部分
```

## Google 工具读取的包索引

Google 的工具并不询问某个管理器装了什么：它们遍历 SDK 根目录，解析每个包目录里的
`package.xml`。否则即使目录布局正确，osdk 装的包对它们也是不可见的——`avdmanager`
会答 `Package path is not valid` 且列不出任何东西，而 `sdkmanager` 却能正常显示
同一个包，因为 sdkmanager 认 `source.properties`，avdmanager 不认。

osdk 在安装时写出该文件。其中所有内容都来自归档内自带的 `source.properties`，
缺失的字段一律省略而非填默认值：错误的 api level 或 abi 会让 avdmanager 提供一个
根本启动不了的 AVD。只发出两种 schema 形态——工具用 `genericDetailsType`，系统
镜像用 `sysImgDetailsType`，后者携带 AVD 匹配所依据的 api level、tag、vendor
和 abi。

对于早先版本 osdk 装下的包：

```bash
osdk android sdk-root show     # 哪些已桥接、哪些已建索引
osdk android sdk-root repair   # 重写索引，并重新检查链接
```

`repair` 刻意做成离线可用：索引所需的一切都已在磁盘上，为了拿回一个小 XML 文件而
重新下载数 GB 是荒谬的做法。
