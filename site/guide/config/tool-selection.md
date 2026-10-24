# 工具选择

先了解统一排序，再查[各工具适用来源](#tool-order-reference)、[项目来源诊断](#inspect-sources)和[各工具环境变量](./tool-environment)。SDK 下载镜像与项目包 Registry 属于独立配置。

## 可配置的声明优先级

```toml
[settings.precedence]
tools = "osdk"
packages = "native"
tool_order = [".node-version", ".nvmrc", "osdk"]
tool_conflicts = "error" # 默认 prefer，按顺序选取
npm = "osdk"           # 可选：覆盖 packages；inherit 清除该生态覆盖
python = "native"
```

`tool_order` 是所有工具共用的一份来源类型顺序，从高到低排列，不是按工具名分别配置的表。仅适用于当前工具的来源参与选择；每个文件名取最近声明，`idiomatic` 还保留下述 backend 固定文件顺序。支持 `system`（需主动启用）、`osdk`、`global`、
`.tool-versions`、`.nvmrc`、`.node-version`、`idiomatic`、
`package.json#packageManager`、`package.json#devEngines.packageManager`、
`package.json#engines.node`、`package.json#devEngines.runtime`。遗漏的声明来源按默认顺序补齐；`system` 是例外，仅在数组中明确列出时启用。
未知值和重复值报错；`[]` 清除继承的自定义顺序并禁用继承的 `system`。`idiomatic` 将其他原生文件归为一类：Python 使用 `.python-version`；Go 先 `go.mod` 后 `.go-version`，Rust 先 `rust-toolchain.toml` 后 `rust-toolchain`，Java 先 `.java-version` 后 `.sdkmanrc`。这些文件名不能独立写入 `tool_order`；不同文件名之间先按 backend 顺序，再考虑目录距离。默认顺序是项目 osdk、
.tool-versions、.nvmrc、.node-version、其他 idiomatic、packageManager、
devEngines.packageManager、engines.node、devEngines.runtime、global；`tools="native"`
把项目 osdk 移到原生项之后。自定义顺序可以明确提升 global。

项目 osdk 只使用最近配置及显式 `extends`，不隐式合并父配置。precedence 的用户、
extends、项目层按字段覆盖，数组替换；未写的策略字段继承。工具声明同类中被覆盖的
父层仅作为 shadowed 证据，不参与冲突检查，也不校验已失效的版本值。其他工具的声明以 ignored 记录，不解释其版本或完整性值。原生管理器字段跨管理器名称选择最近身份；自动选择遇到不支持的身份会报错。`osdk current` 输出同一选择结果中的
tool、path、field、category、winner/shadowed/ignored 和 reason；结构化配置字段、JSON 字段和简单
文件行号会在已知时报告，不能确定的路径保留为空，不猜测。

`tool_conflicts="error"` 检查不同来源的有效声明：精确版本满足范围不算冲突，数字前缀、
支持的 semver 比较符及 `||` 按范围检查两两交集；配置 alias 先展开。它不把多个范围
合并成新的版本请求，最终仍选择最高优先级声明。不同的 latest/LTS/channel 或不透明选择器
无法离线证明兼容时单独报“无法验证”，不伪称已确认冲突，也不联网安装来解释。
`prefer` 选择最高项，但仍校验有效候选声明的结构。packageManager 要求精确 semver，不支持 URL、hash 或 build suffix；
devEngines.packageManager 的多选数组明确拒绝，单对象/单元素数组支持范围。
显式 `tool@version`、指定的 deps provider/installer 及锁重放保持各自权威。

### tool_order 合法值与工具适用范围 {#tool-order-reference}

`tool_order` 是由以下 **11 个精确字符串**组成的数组，排列的是声明来源及需主动启用的外部执行来源，不是工具名、命令名、下载镜像或 registry。同一数组适用于所有工具；合法类型若没有当前工具的候选声明，就不会影响该工具。没有 `[settings.precedence.node]` 或按工具独立设置的 `tool_order`。

| 合法值 | 候选来源 | 适用的内置工具 |
| --- | --- | --- |
| `system` | 适配器 home 或 PATH 中已有的可执行文件；仅明确列出时启用 | Node、Python、Go、Java、npm、pnpm、Yarn、Bun、Deno、Zig；见[系统工具](#system-tools) |
| `osdk` | 最近项目 `osdk.toml` / `.osdk.toml` 中的 `[tools]`，包括显式 `extends` | 所有具有匹配 backend 键的工具 |
| `global` | osdk 用户配置 `$OSDK_CONFIG_DIR/config.toml` 中的 `[tools]`；用 `osdk config path` 查看实际位置 | 所有具有匹配 backend 键的工具 |
| `.tool-versions` | 最近适用 `.tool-versions` 中匹配该工具的行 | 所有具有匹配 backend 键的工具 |
| `.nvmrc` | `.nvmrc` 版本声明 | Node |
| `.node-version` | `.node-version` 版本声明 | Node |
| `idiomatic` | backend 声明的文件，使用下表固定顺序 | 下表对应的 backend；不包含 Node 单独分类的两个文件 |
| `package.json#packageManager` | `packageManager` 中的管理器精确版本 | npm、pnpm、Yarn |
| `package.json#devEngines.packageManager` | `devEngines.packageManager` 中的版本/范围 | npm、pnpm、Yarn |
| `package.json#engines.node` | `engines.node` 中的 Node 版本/范围 | Node |
| `package.json#devEngines.runtime` | `name="node"` 条目中的运行时版本/范围 | Node |

`global` 指 **osdk 用户级的工具默认配置**，不是 `PATH` 中已安装的版本，也不是 nvm/npm 的原生全局配置。例如，用户 `config.toml` 的 `[tools]` 声明 `node="20"`，项目 `osdk.toml` 的 `[tools]` 声明 `node="22"`，项目使用以下策略时选中请求 `20`：

```toml
[settings.precedence]
tool_order = ["global", "osdk"]
tool_conflicts = "prefer"
```

把 `tool_conflicts` 改为 `"error"` 会拒绝不兼容的 `20` 与 `22` 声明，不再选取其中一个。不配置自定义顺序时，默认项目优先，选中 `22`。显式 CLI 版本与锁重放仍保持权威。

没有 `env` 来源类型，也没有通用 `OSDK_<TOOL>_VERSION` 覆盖：`OSDK_NODE_VERSION`、`OSDK_PYTHON_VERSION` 均未实现。`CLI > OSDK_* > 项目` 只适用于已实现的设置。机器上已有的 PATH/home 工具通过单独、需主动启用的 [`system` 执行来源](#system-tools) 选择；未启用时，shim 未选中版本仍会报错。

下表每一行还支持 `osdk`、`global`、`.tool-versions`，按该工具的 backend 键匹配。只有在“额外适用”列明确标注 `system` 的工具支持该来源，且必须主动启用。存在版本文件不等于自动请求安装该工具；本表说明请求或激活该工具时如何选择版本。

| 工具 / backend 键 | 额外适用的 `tool_order` 值 | `idiomatic` 内的文件，按优先级排列 |
| --- | --- | --- |
| <span id="node"></span>`node` | `system`, `.nvmrc`、`.node-version`、`package.json#engines.node`、`package.json#devEngines.runtime` | 无：`.nvmrc` 和 `.node-version` 各有独立类型；移动 `idiomatic` 不会移动它们 |
| <span id="python"></span>`python` | `system`, `idiomatic` | `.python-version` |
| <span id="go"></span>`go` | `system`, `idiomatic` | `go.mod`（`go` 指令），然后 `.go-version` |
| <span id="rust"></span>`rust` | `idiomatic` | `rust-toolchain.toml`，然后 `rust-toolchain` |
| <span id="java"></span>`java` | `system`, `idiomatic` | `.java-version`，然后 `.sdkmanrc` |
| <span id="bun"></span>`bun` | `system`, `idiomatic` | `.bun-version` |
| <span id="deno"></span>`deno` | `system`, `idiomatic` | `.dvmrc` |
| <span id="zig"></span>`zig` | `system`, `idiomatic` | `.zig-version` |
| <span id="maven"></span>`maven` | `idiomatic` | `.mvn-version` |
| <span id="gradle"></span>`gradle` | `idiomatic` | `.gradle-version` |
| <span id="kotlin"></span>`kotlin` | `idiomatic` | `.kotlin-version` |
| <span id="js-managers"></span>`npm`、`pnpm`、`yarn` | `system`（需主动启用）、`package.json#packageManager`、`package.json#devEngines.packageManager` | 无 |
| 其他内置及动态 backend | 无 | 无 |
| 声明式插件 backend | 定义声明了文件时支持 `idiomatic` | 其他文件按定义的 `idiomatic_files` 顺序；保留名 `.nvmrc` / `.node-version` 仍使用独立类型；不增加枚举值 |

普通版本文件接受一行版本值（可含注释），不表示支持任意原生管理器配置语法。特别是已登记的 `.sdkmanrc` 路径目前使用普通版本读取器，不解释 SDKMAN 的多键 `java=...` 格式。Rust toolchain 文件和 Go 的 `go` 指令使用专门解析。

未知字符串和重复项会报错：`"node"`、`".python-version"`、`"go.mod"` 都不是合法的 `tool_order` 值。遗漏的声明类型会按 `tools` 决定的默认相对顺序追加，不会被禁用。`system` 不会自动追加，遗漏即禁用。`[]` 清除继承的自定义顺序并禁用继承的 `system`。`idiomatic` 内先按固定文件名顺序，再看目录距离，因此祖先目录的 `go.mod` 声明可以优先于更近的 `.go-version`；`tool_order` 不能反转这两个文件。下方提供[三个可运行示例](#示例-node、python-与-javascript-管理器)。

### 示例：Node、Python 与 JavaScript 管理器

以下是分别使用的项目 `osdk.toml` 示例。这些策略字段直接编辑 TOML，再用 `osdk current node`、`osdk current python` 或 `osdk current pnpm` 查看 winner 与 shadowed/ignored 证据。已有锁重放或显式版本请求仍保持权威。

**Node：让 `.node-version` 优先于 `.nvmrc` 和 `[tools]`：**

```toml
[tools]
node = "20"

[settings.precedence]
tool_order = [".node-version", ".nvmrc", "osdk"]
tool_conflicts = "prefer"
```

若 `.node-version` 内容为 `22`、`.nvmrc` 为 `20`，选中的请求是 `22`。把 `tool_conflicts` 改为 `"error"` 会拒绝这些不兼容声明。该排序也适用于其他工具，但两个 Node 专用文件类型不会影响 Python 或 pnpm。

**Python：优先使用 `.python-version`，同时保留原生包索引：**

```toml
[tools]
python = "3.12"

[settings.precedence]
tool_order = ["idiomatic", "osdk"]
packages = "osdk"
python = "native"
```

若 `.python-version` 内容为 `3.13`，Python 选中的请求为 `3.13`。`idiomatic` 也会提升 Go/Rust/Java 适用的原生文件；没有只针对 Python 的 `tool_order` 配置。单独的 `python="native"` 让 Python 包索引保留原生策略，而其他生态使用 `packages="osdk"`。子层写 `python="inherit"` 可恢复共用包源策略；写 `tool_order=[]` 可清除继承的自定义版本顺序。

**JavaScript 管理器：让 devEngines 范围优先于 packageManager 精确版本：**

```toml
[settings.precedence]
tool_order = ["package.json#devEngines.packageManager", "package.json#packageManager", "osdk"]
tool_conflicts = "error"
npm = "native"
```

配合[包管理器页的 package.json 示例](../tools/package-managers#包管理器版本发现)，pnpm 选择 `>=10 <11`；精确声明 `10.15.0` 与其兼容，但不会将胜出范围固定到该版本。`npm="native"` 控制 npm 生态（含 pnpm/Yarn）的包 registry，不控制管理器可执行文件版本。需要精确选择时，让 `packageManager` 排在前面，或显式指定版本，例如 `osdk exec -t pnpm@10.15.0 -- pnpm --version`。

### 机器上已有的系统工具 {#system-tools}

要使用机器上已有的可执行文件，而不将它安装进 osdk：

```toml
[settings.precedence]
tool_order = ["system", "osdk"]
tool_conflicts = "prefer"
```

这是一份对所有已支持工具生效的策略。`osdk current node` 显示绝对执行路径及来源；`osdk exec -t node -- node --version` 验证并执行它。`osdk reshim` 为可用的伴随命令生成 shim，activation 使用同一选择。`prefer` 下发现/current/activation 不运行版本探针，执行前版本诚实标记为未知。`error` 下选择时探测外部版本，与有效声明（含范围）检查兼容性；`lts` 等不透明选择器无法证明兼容，会报错。JS 管理器探针使用选中的 Node；严格检查若选中托管 Node，该 Node 必须已经安装。

`system` **默认关闭**，遗漏时不会自动追加；用 `[]` 替换继承顺序会禁用它。其他遗漏的声明类型仍按默认顺序追加。例如 `["osdk", "system"]` 优先项目声明，其次系统，再处理遗漏的原生/global 来源。要让系统成为最后回退项，先列出全部声明来源：

```toml
[settings.precedence]
tool_order = [
  "osdk", ".tool-versions", ".nvmrc", ".node-version", "idiomatic",
  "package.json#packageManager", "package.json#devEngines.packageManager",
  "package.json#engines.node", "package.json#devEngines.runtime", "global", "system",
]
```

[查看各工具选择器、子进程变量、缓存与版本探测格式](./tool-environment#per-tool-environment)。


只有这些适配器参与。Rust/rustup、Maven、Gradle、Kotlin、uv、动态包工具和插件 backend 没有 system 适配器。`PNPM_HOME`、`NVM_DIR`、缓存变量及其他 home 不会成为选择器。现有的无关环境/缓存变量策略不变；`override_env` 不是选择开关。 外部执行前，osdk 也将 activation 注入的强制位置变量（如 `PNPM_HOME`、`npm_config_prefix`、`BUN_INSTALL`）恢复为保存的用户原值或原本未设置状态；这不会让它们变成选择来源。Python venv 或 JDK 优先时，设置普通 `VIRTUAL_ENV` 或 `JAVA_HOME`，再用 `tool_order=["system"]`，无需新增 osdk 版本变量。要混用托管 pnpm 与外部 Node，可用 `osdk exec -t pnpm@10.15.0 -- pnpm --version`：显式管理器保持托管，隐式 Node 按策略选择。

PATH 搜索接受绝对目录（允许空格），忽略空项/相对目录；排除 osdk data/install/store、canonical 别名、已知 shim 文件身份/副本及生成的包装脚本。home 必须是绝对路径；显式设置的无效 home 会报错，不悄悄改选 PATH。osdk activation 注入的 locator/PATH 在发现前恢复成保存的原始环境值。Windows 仅支持 `.exe`，名称不区分大小写；不选择 `.cmd`、`.bat`、PowerShell 或任意 PATHEXT 包装器，因此常见 `pnpm.cmd` / `npm.cmd` 安装应改用托管管理器或真实外部可执行文件。用于构造该执行环境的路径/home 必须是 Unicode。

没有合格候选时，较低优先级的声明可胜出。一旦选中外部候选，版本探针失败、输出无法识别或超长都会停止；探针限三秒、每个输出流 8 KiB。探针失败不会自动安装替代品，执行开始后不会重试安装。不承诺支持未识别的预发布 banner、Java 8 下划线版本或自定义启动器。

显式 CLI 版本及托管锁重放保持权威。`install`/`lock` 对选中 system 的无版本请求报错；要创建可复现安装，请给出显式托管版本。外部路径不写成托管 receipt 或可移植锁条目，uninstall/cache 清理不拥有这些文件。非 frozen deps 可使用未固定版本的外部安装器/运行时；已固定安装器和 frozen 计划仍走托管。使用外部工具的 deps 计划不写可移植工具/新鲜度记录；启用 system 策略时 deps 每次重新验证。`deps --dry-run --explain` 显示不执行探针的发现证据，并注明延后验证。system 执行不与动态包工具 receipt 混用，后者仍保留记录的托管运行时契约。

## 检查项目中的来源 {#inspect-sources}

```sh
osdk current node --sources
osdk current python --sources --project ./apps/api
osdk current pnpm --sources --project ./apps/web --json
# 显式允许原生版本探测（JS 管理器也可能探测其选定的 Node）：
osdk current node --sources --probe
```

`current TOOL --sources` 扩展现有查询命令，要求指定一个工具。`--json`、`--project DIR`、`--probe` 均要求 `--sources`。不带这些参数时，原有 `current [TOOL]` 输出保持不变：先输出声明证据 JSON，再输出选中的请求/路径，并非完整来源清单。`--project` 在加载配置**之前**选择目录，不把配置文件名当作项目目录解释。

报告分为：

- **配置顺序与有效顺序**：继承后的自定义数组，以及补齐默认来源后的完整顺序。`[]` 清除继承的 system 选择。有效顺序仍是全工具共享数组，不适用于该工具的类型仍可能在数组中，但不能产生候选。
- **来源清单**：全部 11 个合法类型，含支持/启用标记和从 1 开始的排名。`present` 表示记录到声明（可能属于其他管理器而被 ignored），或找到合格外部路径；`missing` 表示没有声明/候选，即使文件存在；`disabled` 表示支持 system 但未启用；`inapplicable` 表示该 backend 不支持此来源。位置列表单独记录文件是否存在。缺失声明的位置列出搜索的祖先目录；项目 osdk 只跟随选中的文件和显式 `extends`，不转储每个原生文件的全部内容。
- **选择证据**：真实 winner/shadowed/ignored 条目及其来源路径、字段、原因。严格冲突时，把排序第一项标记为 `candidate` 并清空选中结果。无效声明会将清单标为不完整，未确定的类型标为 `unknown`，不能当作缺失。

默认诊断不执行工具、不安装、不获取远程目录元数据、不改项目文件。发现 system 路径时版本仍未知。严格策略需要进一步检查 system 兼容性时，`resolution_status="verification-required"` 与 `candidate` 表示临时排序结果，不能视为严格验证成功。`--probe` 才运行有时限的原生探测和真实冲突检查；它可能执行 JS 管理器选定的 Node，所需托管 Node 必须已安装。探测模式经过配置 trust 检查，不安装缺失运行时，也不换安装器重试。原生可执行文件本身仍保留其通常的进程行为。

`--json` 输出一个对象，包含 `schema_version=1`、`tool`、`project`、`configured_order`、`effective_order`、`tool_conflicts`、`inventory_complete`、`sources`、`evidence`、`selected`、`resolution_status`、`error`、`notes`。`selected` 标明 managed/system、安全的数字选择器或已探测版本，以及适用的外部执行路径。不透明选择器会脱敏；不转储其他工具的原始值或环境变量值。已知解析/探测错误输出报告并非零退出；无效 TOML/策略或未知 backend 可能在报告构造之前失败，仅向 stderr 输出错误，不输出 JSON 对象。只读临时报告退出码为零；自动化必须检查 `resolution_status`，不能仅凭退出码判断已验证。

策略仍直接编辑 TOML：尚未实现 `osdk config get precedence.tool_order`。`sources` 查询 SDK 下载镜像，`registry` 查询项目包来源，`cache env` 描述缓存映射，都不能替代 `current TOOL --sources`。显式执行请求、已安装动态包 receipt 和锁回放具有单独权威，此项目声明诊断不模拟它们。
