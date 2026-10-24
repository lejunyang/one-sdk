# 工具环境变量

[工具选择](./tool-selection#tool-order-reference)回答**工具从哪里来**。本页区分**已有工具的环境选择器、托管执行时覆盖的变量，以及可调整的缓存变量**。osdk 自身实现的 `OSDK_*` 设置单独列在[osdk 环境变量](./environment)。

## 三种不同机制

1. **system 选择器**：只有显式 `tool_order=["system", ...]` 才通过受支持工具的原始 `PATH` 和下列 home 变量发现工具。它选择现有可执行文件/工具链，不请求安装。
2. **托管执行变量**：选择 osdk 托管安装后，将相应 bin 目录置于 PATH 前面，并为子进程/激活 shell 设置必要的目录变量。这些变量不受 `override_env` 控制。
3. **缓存变量**：默认保留用户已设置的值。`settings.override_tools_env` 或 `settings.<tool>.override_env` 允许 osdk 覆盖对应缓存变量；它们不会改变 `tool_order`，也不会把缓存/home 变量变成工具选择器。

## 各工具参考 {#per-tool-environment}

各工具支持的来源及 idiomatic 文件固定顺序统一列在[来源矩阵](./tool-selection#tool-order-reference)；来源顺序是所有工具共享的，不是按工具配置。下表 PATH 指**原始 PATH 中的绝对目录**：排除 osdk 托管目录与 shim 身份，忽略空项及相对路径。Windows 仅支持 `.exe`（扩展名不区分大小写），不选择 `.cmd`、`.bat` 或 PowerShell 包装脚本。

| 工具 | 启用 system 后的选择方式 | 托管执行时设置的变量 | 缓存变量 / 覆盖设置所属工具 |
| --- | --- | --- | --- |
| <span id="node"></span>Node (`node`) | PATH → `node` | `npm_config_prefix` | `npm_config_cache`；共享 npm 缓存使用 `settings.npm`，Node 自身 backend 也读取 `settings.node` |
| <span id="python"></span>Python (`python`) | 先 `VIRTUAL_ENV/bin`（Windows 为 `Scripts`），否则 PATH → `python3`、`python` | 无强制 Python 位置变量 | `PIP_CACHE_DIR`、`UV_CACHE_DIR` / `python` |
| <span id="go"></span>Go (`go`) | 先 `GOROOT/bin`，否则 PATH → `go` | `GOROOT`、`GOBIN`；选定代理时设置 `GOPROXY` | `GOMODCACHE`、`GOCACHE` / `go` |
| <span id="java"></span>Java (`java`) | 先 `JAVA_HOME/bin`，否则 PATH → `java`；要求含 `javac` 的 JDK | `JAVA_HOME` | `GRADLE_USER_HOME` / `java` |
| <span id="npm"></span>npm (`npm`) | PATH → `npm`；使用同一策略选定的 Node | `npm_config_prefix` | `npm_config_cache` / `npm` |
| <span id="pnpm"></span>pnpm (`pnpm`) | PATH → `pnpm`；使用同一策略选定的 Node | `PNPM_HOME` | pnpm 11+：`pnpm_config_store_dir`；更早版本：`npm_config_store_dir` / `pnpm` |
| <span id="yarn"></span>Yarn (`yarn`) | PATH → `yarn`；使用同一策略选定的 Node | `YARN_GLOBAL_FOLDER` | 仅 Classic：`YARN_CACHE_FOLDER` / `yarn`；Berry 使用原生项目缓存配置 |
| <span id="bun"></span>Bun (`bun`) | PATH → `bun` | `BUN_INSTALL` | `BUN_INSTALL_CACHE_DIR` / `bun` |
| <span id="deno"></span>Deno (`deno`) | PATH → `deno` | `DENO_INSTALL_ROOT` | `DENO_DIR` / `deno` |
| <span id="zig"></span>Zig (`zig`) | PATH → `zig` | 无 | `ZIG_GLOBAL_CACHE_DIR` / `zig` |
| <span id="rust"></span>Rust (`rust`) | 不支持；`RUSTUP_HOME`/`CARGO_HOME` 不是 system 选择器 | `RUSTUP_HOME`、`CARGO_HOME` | 共享缓存层可设置 `CARGO_HOME` / `rust`，但选中的托管 Rust backend 随后设置自己的 home |
| Maven、Gradle、Kotlin | 这些工具没有 system 适配器；可由受支持的外部 Java 选择提供 JDK | JDK 环境跟随选定的 Java 运行时 | 共享 `GRADLE_USER_HOME` 使用 `java`；无通用 `MAVEN_HOME`/`GRADLE_HOME` 选择器 |
| Android 各包族 | 无 system 适配器 | SDK 包族：`ANDROID_HOME`、`ANDROID_SDK_ROOT`；NDK：`ANDROID_NDK_HOME`、`ANDROID_NDK_ROOT` | `ANDROID_AVD_HOME` 单独遵循显式用户值优先，不是缓存覆盖选择器；见 [Android](../runtimes/android) |
| 动态/包工具及声明式插件 | 无 system 适配器 | 各 backend/包的执行环境；插件可声明自己的变量 | 没有任意变量的统一覆盖 API；查对应 backend 指南 |

共享缓存层在 activation、exec 和 shim 中参与环境构造，即使未选择某个对应工具。工具专属的缓存值由相应**托管** backend 设置；外部 system 目标不套用该 backend 的托管缓存/目录配置，原有缓存环境与共享缓存策略仍然有效。例如外部 pnpm 不会被强制切换到 osdk 的 pnpm store，但共享 pip/Go 缓存策略仍可能生效。

`PNPM_HOME`、`NVM_DIR`、`BUN_INSTALL`、`DENO_DIR`、`PIP_CACHE_DIR`、`UV_CACHE_DIR`、`PYTHONHOME`、`MAVEN_HOME`、`GRADLE_HOME` **不是可执行文件选择器**。原生工具可能读取它们，但 osdk 不从这些变量寻找可执行文件。未实现 `OSDK_NODE_VERSION`、`OSDK_PYTHON_VERSION`、通用 `OSDK_<TOOL>_VERSION` 或 `env` 来源类型。`"system"` 应写入 `tool_order`，不能作为 `[tools]` 中的版本字符串来启用此功能。

## home 选择与验证

显式设置的受支持 home 先于 PATH 搜索，必须为包含合格可执行文件的绝对目录；无效 home 报错，不静默回退到 PATH。osdk 会先还原自身激活时注入的值为原始环境，避免把托管安装误当系统工具。Java 配套命令来自同一规范化 JDK；显式 Python venv 在探测时必须与解释器的 `sys.prefix` 一致。

版本验证执行 `java -version`、`go version`、`zig version`，其他适配器使用 `--version`。Node 要求 `vMAJOR.MINOR.PATCH`，Python 要求稳定版 `Python MAJOR.MINOR.PATCH`，Go 使用稳定版原生 banner，Java 使用受支持的数字版本。JS 管理器/Bun/Zig 要求稳定 semver；Deno 原生 banner 可识别。探测限制为三秒、每个输出流 8 KiB。不支持的预发布/自定义 banner、Java 8 下划线版本会验证失败；失败后不安装替代品。

[来源诊断](./tool-selection#inspect-sources)默认**不执行工具**，显式 `--probe` 才允许探测。普通 `current` 跟随执行策略：`prefer` 不探测，`error` 可能为了检查兼容性而探测；来源诊断则明确延迟这一步，直到用户请求。

## 使用 system 工具时的还原

执行 system 目标前，osdk 将该工具由激活注入的强制变量（上表托管列）还原为保存的用户值**或未设置状态**，再按需要设置选定的 system home。例如外部 pnpm 得到原来的 `PNPM_HOME` 而非 osdk 托管目录；外部 Node/npm 还原 `npm_config_prefix`；外部 Go 还原 `GOBIN`/`GOPROXY`，在使用 GOROOT home 选择器时设置该 home。被还原的变量不会因此变成选择来源。

无关环境变量与既有缓存策略保持原有行为。`deactivate` 还原 shell 激活接管的变量。此次选择不会重写 shell 启动文件或原生管理器配置。早先“辅助 JVM 工具找不到托管 JDK 时保留用户 `JAVA_HOME`”是更窄的兼容路径，不代表默认系统优先。

## 配置缓存覆盖 {#cache-overrides}

```toml
[settings]
override_tools_env = false  # 默认保留显式用户缓存选择

[settings.python]
override_env = true         # 覆盖 PIP_CACHE_DIR 和 UV_CACHE_DIR

[settings.pnpm]
override_env = true         # 托管 pnpm 对应版本的 store 变量
```

全局和分工具开关取 OR：`override_env=false` 不能抵消全局 `true`。`OSDK_OVERRIDE_TOOLS_ENV=true` 可启用全局开关；false-like 值不能关闭配置中已有的 `true`。这些开关不阻止托管 `JAVA_HOME`/`PNPM_HOME` 的设置，也不启用 system。`osdk cache env` 描述托管缓存映射，不是来源清单，也不保证每个映射都会覆盖用户值。

需要优先使用已有 Python venv 或 JDK 时，在 shell 中设置普通 `VIRTUAL_ENV` 或 `JAVA_HOME`，并使用：

```toml
[settings.precedence]
tool_order = ["system", "osdk"]
```

此设置对所有受支持工具启用 system；若 system 应排在所有声明之后，使用[完整回退示例](./tool-selection#system-tools)。显式版本和托管 lock/frozen 回放仍保持权威；外部路径永远不成为可移植锁条目。
