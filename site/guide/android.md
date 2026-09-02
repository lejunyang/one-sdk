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

```bash
osdk list-remote android-platform-tools
osdk install android-platform-tools@37.0.1 -o accept-licenses=true
osdk exec -t android-platform-tools@37.0.1 -- adb version
```

已被 side-by-side 布局取代的 `ndk-bundle` 未纳入，以免与 `android-ndk` 混淆；
系统镜像位于独立的子站点清单，当前也未纳入。

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
