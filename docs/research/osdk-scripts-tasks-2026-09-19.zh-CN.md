# osdk 配置文件 scripts / tasks 能力调研

日期：2026-09-19

状态：**纯调研与设计提案，未改动任何产品代码。** 本文不描述 `crates/` 下的现有行为——当前树中没有任何 scripts / tasks / hooks 实现（核查命令见 §11.1）。所有实测均在 `E:\tmp\` 下的临时工程中完成，仓库工作区始终保持 clean。

配套可视化：[`osdk-scripts-tasks-2026-09-19.visual.html`](./osdk-scripts-tasks-2026-09-19.visual.html)（候选体积矩阵、能力对照、四档分层、落地阶段图）。

外部事实核查基准日：2026-09-19。每条外部断言附来源 URL；每个实测数字附测量命令、口径与环境。**凡未实测又无法从一手来源确证的，一律显式标注为估算或「待验证」，不得在实现时当作既定事实。**

> **2026-09-19 修订记录。** 初稿发布后经追问补充了四处，其中一处是对初稿结论的**推翻**：
> - **§6.5（推翻）**：初稿建议「pwsh 自动加 `-NoProfile`」照抄 mise，**该结论对 osdk 是错的**。osdk 的 shims 与 shell activation 是两套独立机制，禁用 profile 会丢掉 `JAVA_HOME`/`GOROOT` 与未生成 shim 的工具。改为「osdk 主动注入任务环境 + profile 钩子检测 `OSDK_TASK` 自避让」。§执行摘要四、§3.1、§6.2、§7 与 §8.1 的相关表述已同步更正。
> - **§6.4.1（新增）**：osdk 已内置 zig backend，`zig cc` 很可能直接化解 mlua 交叉编译这一阻塞项。仍属待验证。
> - **§6.6（新增）**：`&` 在 cmd / PowerShell 7 / PowerShell 5.1 下的语义实测，结论是 `run` 内不应依赖 `&`，「失败继续」与「真并行」各给专门字段。
> - **§6.7 / §6.8（新增）**：补齐 Lua 5.4 版本选择理由，以及「为什么不要 Luau 沙箱」的完整论证。

---

## 执行摘要

用户的诉求是三件事：给 `osdk.toml` 增加 scripts 能力、参考 mise tasks、能代理 makefile，并且「既可简单使用，又可复杂编写」；同时预感到可能需要内嵌一款轻量脚本语言。核查与实测之后，有六个结论决定了整个设计的形状：

**一、内嵌脚本语言这一步应该做，但不是第一步，而且它解决的问题比预想的窄。** 实测 13 个候选（§5），最便宜的可用引擎是 mlua + Lua 5.4 vendored，在**统一 profile**（与 osdk 完全相同的 `opt-level="z"` + `lto="fat"` + `strip`）下比同构基线仅增 **421.5 KiB**。但把它真正接进 osdk 量出的代价是 **+304,640 B（+2.37%）**——比独立试验更小，因为 osdk 已经链接了 mlua 需要的大部分 C 运行时支撑。这远在 AGENTS.md 的 10% 红线之内。**然而**：Lua 能表达的东西，90% 的任务场景用「声明式 TOML + shell」就够了；内嵌语言真正不可替代的场景只有条件分支、循环生成、跨平台路径拼接这三类。所以它应该是第四档的逃生舱，而不是地基。

**二、体积的真实约束不在 CLI 而在 shim，而这个约束正好是免费的。** 实测证实：把脚本引擎放在一个**默认开启但 shim 关闭**的 `scripts` feature 后面，`osdk-shim.exe` 的字节数**与基线完全一致（3,743,744 B，一字节不差）**。这不是推断，是两次独立 `cargo build` 的实测结果（§5.4）。这条路径与 AGENTS.md 对 sigstore / rattler 的处理完全同构，是仓库里已被验证过的做法。

**三、「可代理 makefile」这个目标必须拆成两半回答，因为其中一半是陷阱。** make 的能力里，`.PHONY`、变量、并行 `-j`、任务依赖这四项是刚需且都不难；**文件级时间戳增量**也是刚需，mise 用 `sources`/`outputs` 覆盖了；但 **pattern rule（`%.o: %.c`）与自动变量（`$@`/`$<`）不该抄**——它们是「为每个文件生成一条规则」的语言，一旦引入，osdk 就从任务运行器变成了构建系统，而这条路上等着的是 make 全部的复杂度。mise、just、Task、cargo-make **没有任何一个**提供 pattern rule，这是一致的行业判断，不是遗漏。osdk 应当覆盖到「任务级增量」这一层然后停下（§4）。

**四、Windows 一等公民这条约束直接否掉了一批看起来很自然的设计。** `just` 在 Windows 上**默认要求 PATH 里有 `sh`**，官方文档原话是「After installation, `sh` must be available in the `PATH`」——即需要额外装 Git for Windows / Cygwin。这对 osdk 是不可接受的：osdk 的价值主张就是「装上就能用」。正确做法是 mise 的做法：默认 shell 在 Windows 上是 `cmd /c`，另配 `run_windows` 让同一任务给出 Windows 变体。**但 mise 的 pwsh `-NoProfile` 处理不能照抄**——osdk 的 shims 与 shell activation 是两套机制，禁用 profile 会丢掉 `JAVA_HOME`/`GOROOT` 与未生成 shim 的工具，与本仓库 AGENTS.md 的明确要求冲突；正确做法是由 osdk 主动注入任务环境 + 钩子自避让（§6.5，本次修订）。另外 `&` 在 cmd / Unix sh / PowerShell 下语义三不相同（实测见 §6.6），`run` 内不应依赖它，「失败继续」与「真并行」各有专门字段。

**五、交互延迟路径完全不受影响，但有一个必须主动避开的陷阱。** 任务定义只在 `osdk run` 时读取，`hook-env` 与 `osdk-shim` 两条热路径不解析 `[tasks]`、不初始化引擎。**但** `sources`/`outputs` 的 glob 扫描是个真实的性能地雷：我在实测中亲自踩了——`shellonly` 对照组在 `target/` 旁边运行时单次 **2,827.91 ms**，换到干净目录（201 个文件）后是 **61.26 ms**，相差 46 倍（§5.5）。AGENTS.md 里「扫描下探进 conda prefix 一次多走 1000 个目录」记的是同一类坑。结论：freshness 扫描必须从 glob 的**字面前缀**起步，不能从 cwd 全量 walk。

**六、「既简单又复杂」必须落成四档且档与档之间不改写已有配置。** 设计为：`build = "cargo build"`（单行）→ `run = ["a", "b"]` + 多行字符串（多行 shell）→ `file = "scripts/release.ps1"`（独立脚本）→ `lua = """..."""`（内嵌语言）。关键约束是**升档不推翻**：第一档写的东西在第四档存在时仍然合法且语义不变，用户永远不会因为某个任务变复杂而被迫重写其他任务（§7）。

由此得到的主线是：**先做「声明式 TOML + shell + 任务级增量」，把 mise tasks 已验证的核心搬过来；内嵌 Lua 作为可选 feature 在第二阶段落地，仅服务于逃生舱场景；坚决不做 pattern rule 与构建系统语义。**

---

## 1. 项目现状与约束边界

### 1.1 现状（已核查）

| 项目 | 事实 | 核查方式 |
| --- | --- | --- |
| 工作区结构 | 3 个 crate：`osdk-core`（库）、`osdk-cli`（`osdk`）、`osdk-shim`（`osdk-shim`） | `Cargo.toml` `[workspace].members` |
| 配置层次 | CLI flags → env(`OSDK_*`) → 项目配置（`osdk.toml`/`.osdk.toml`，向上查找）→ 用户全局 `$OSDK_CONFIG_DIR/config.toml` → 内置默认 | `crates/osdk-core/src/config/mod.rs:3-5` |
| 现有 TOML 段落 | `[settings]`、`[tools]`、`[sources]`、`[registries]`、`[containers]`、`[aliases]`，另兼容 `.tool-versions` | `ConfigFile` 结构体，`config/mod.rs:820-829` |
| scripts 能力 | **完全不存在**，无任何相关代码 | §11.1 的 grep 核查，0 命中 |
| 信任机制 | `trust.rs` 按 key 分级：`ExecutesCode` / `WeakensVerification`，**未知顶层表 fail-closed 归为 `ExecutesCode`** | `trust.rs:204-217` |

最后一条对本设计至关重要，下面 §8 会展开。

### 1.2 实测的体积基准（不引用 AGENTS.md 中的数字）

AGENTS.md 明确要求「基准线会随功能增长而变，引用前先按下面的命令实测当前值」。照做：

```powershell
$env:CARGO_TARGET_DIR="target\size-check"
cargo build --release -p osdk-cli
cargo build --release -p osdk-shim
```

| 二进制 | 实测字节 | MiB | AGENTS.md 记载值（2026-09-16） |
| --- | --- | --- | --- |
| `osdk.exe` | **12,870,144** | 12.27 | 12,797,952（已过时，本次未采用） |
| `osdk-shim.exe` | **3,743,744** | 3.57 | 3,709,952（已过时，本次未采用） |

环境：Windows x64，`rustc 1.98.0 (88d9e12ae 2026-08-18)`，host `x86_64-pc-windows-msvc`，workspace `[profile.release]`（`opt-level="z"`、`lto="fat"`、`codegen-units=1`、`strip=true`、`panic="abort"`）。两次构建分开调用，未使用 `--workspace`。

**本文后续所有百分比都以这两个数为分母。** 10% 红线因此是 osdk 约 1.23 MB、shim 约 0.36 MB。

---

## 2. mise tasks 详细能力清单

来源：https://mise.jdx.dev/tasks/task-configuration.html 、https://mise.jdx.dev/tasks/toml-tasks.html 、https://mise.jdx.dev/tasks/file-tasks.html 、https://mise.jdx.dev/tasks/running-tasks.html 、https://mise.jdx.dev/tasks/task-arguments.html 、https://mise.jdx.dev/tasks/monorepo.html （均于 2026-09-19 抓取）

### 2.1 两种形态

**TOML tasks**：写在 `mise.toml` 的 `[tasks.<name>]`。四种等价简写：

```toml
tasks.a = "echo hello"
tasks.b = ["echo hello"]
tasks.c.run = "echo hello"
[tasks.d]
run = "echo hello"
```

**File tasks**：`mise-tasks/`、`.mise-tasks/`、`mise/tasks/`、`.mise/tasks/`、`.config/mise/tasks/` 下的可执行脚本，用 `#MISE key=value` 注释携带配置、`#USAGE` 注释声明参数。子目录自动成为 `:` 分隔的任务名前缀（`mise-tasks/test/units` → `test:units`，`_default` → `test`）。

### 2.2 完整字段表

| 字段 | 类型 | 默认 | 语义 |
| --- | --- | --- | --- |
| `run` | string \| 数组（可含 `{task=…}` / `{tasks=[…]}`） | — | 命令序列，逐条串行；`{tasks=[…]}` 条目内部并行 |
| `run_windows` | 同上 | — | Windows 专用变体 |
| `file` | string | — | 执行外部脚本；支持 HTTP(S) 与 `git::` 远程源 |
| `description` / `alias` | string / string\|数组 | — | 帮助文本 / 别名 |
| `depends` | string \| 数组（支持 `{task, args, env, optional}`） | — | 前置任务，加入执行图；共享依赖只跑一次 |
| `depends_post` | 同上 | — | 本任务及其依赖完成**之后**运行；父任务失败也跑，但父任务未启动则跳过 |
| `wait_for` | 同上 | — | 「若已被调度则等它」，**不主动加入执行图** |
| `env` | map | — | 任务级环境变量，**不传给 `depends`** |
| `vars` | map | — | 模板变量，不导出为环境变量 |
| `tools` | map | — | 任务专属工具版本，不作用于依赖 |
| `dir` | string | `{{config_root}}` | 工作目录；`{{cwd}}` 表示用户当前目录 |
| `sources` / `outputs` | string\|数组 | — / `{auto=true}` | 增量判据，见 §2.3 |
| `shell` | string | `sh -c`（Unix）/ `cmd /c`（Windows） | 解释器覆盖 |
| `usage` | string（KDL） | — | 参数规格，见 §2.4 |
| `hide` / `quiet` / `silent` / `output` | bool / bool / bool\|"stdout"\|"stderr" / string | false | 可见性与输出风格（风格与静默是**正交两轴**） |
| `raw` / `raw_args` / `interactive` | bool | false | 直连 stdio / 完全透传参数 / 独占 stdio 锁 |
| `confirm` | string \| `{message, default}` | — | **只守 `run`，`depends` 已经跑完了**（官方明确警告） |
| `timeout` | string | — | `30s`/`5m`/`1h`；与全局取**较短者** |
| `deny_all` / `deny_read` / `deny_write` / `deny_net` / `deny_env` / `allow_*` | bool / 数组 | false / [] | 沙箱，平台相关 |
| `cache`（实验） | table | `{enabled=false}` | 产物级缓存，可远程 |

### 2.3 sources / outputs 增量语义（值得精读的部分）

- 判据是：**最旧的 output 的 mtime 比最新的 source 的 mtime 新，则跳过**。用的是 mtime 而非内容哈希（`task.source_freshness_hash_contents = true` 可切到 blake3）。
- **任务定义本身自动算作 source**——改了定义就重跑。这条很聪明，osdk 应当直接抄。
- `outputs = {auto = true}` 是定义了 `sources` 时的默认：mise 按任务定义的哈希摸一个内部文件，省去用户手动 `touch`。
- `!` 前缀排除，按顺序求值、后者胜，`\!` 转义字面量。
- **依赖失效传播**：依赖任务因自身 sources 变化而运行时，下游任务即使自己的 sources 没变也会重跑；但**没有 sources 的依赖（总是运行）不触发这个传播**——否则下游的 sources 就完全失效了。这个例外是正确的，且不直观。
- 官方对 glob 有一句警告值得注意：「Don't go overboard with globs that match a huge number of files—mise has to scan each and every one to check its timestamp.」这与我在 §5.5 实测到的 46 倍差距是同一件事。

### 2.4 参数：usage spec

现行推荐方式是 `usage` 字段（KDL 语法），解析后以 `usage_<name>` 环境变量 + Tera 模板里的 `usage.<name>` 两种形式暴露。支持位置参数、可选/必需、默认值、`choices` 枚举、变长（`var=#true` 带 `var_min`/`var_max`）、计数 flag（`-vvv`）、取反 flag（`--no-color`）、环境变量兜底（优先级 CLI > env > default）、自定义补全（`complete "x" run="..."`）。同一份 spec 同时驱动 `--help`、shell 补全和 `mise generate task-docs`。

旧的 Tera 模板函数 `{{arg()}}` / `{{option()}}` / `{{flag()}}` **已废弃，2027.5.0 移除**。官方给的废弃理由对 osdk 有直接借鉴价值：两遍解析时模板函数返回空串、shell 转义规则不可预测、TOML 与 file task 行为不一致。**osdk 不应重走这条路，应当一步到位做 spec 式声明。**

### 2.5 并行与执行顺序

默认 4 并发（`--jobs` / `MISE_JOBS`）。`depends` / `wait_for` / `depends_post` 构成声明式图，`mise tasks deps` 可视化的是这张图。而 `run` 数组里的 `{task=…}` / `{tasks=[…]}` 是**执行步骤，不是图的边**——它们不出现在 `mise tasks deps` 里。官方特意解释了两者不能互相替代：改写成 `depends` 会丢掉「先 A 后 (B‖C)」的顺序约束，因为 `depends` 只要求「跑完」不要求顺序。

输出默认 `prefix`（按行加任务名前缀，避免并行交错）；`--jobs 1` 时自动切 `interleave`。

### 2.6 Windows 处理（osdk 最该精读的一节）

- File task 在 Windows 上没有执行位可看，判定规则是：**扩展名属于 `windows_executable_extensions`（默认 `exe/bat/cmd/com/ps1/vbs`）或文件以 shebang 开头**，二者有其一即可。「两者都没有」的文件在 Linux/macOS 上能用、在 Windows 上完全隐形。
- Windows PowerShell 拒绝执行非 `.ps1` 的脚本，所以 mise 把 `#!/usr/bin/env pwsh` 任务复制成临时 `.ps1` 再跑，并提供 `MISE_TASK_DIR` 让脚本定位自身目录（`$PSScriptRoot` 会指向副本）。
- File task **没有** `run_windows` 的等价物（脚本本身就是命令，没地方放第二个）。跨平台写法是同目录同词干配对：`build`（shebang）+ `build.ps1`，Windows 优先取原生的。
- `windows_default_inline_shell_args` 默认 `cmd /c`，而 **cmd 不能直接启动 `.ps1`**，所以 TOML 任务要写成 `run_windows = "pwsh -File ./scripts/windows-build.ps1"` 而不是直接写路径。
- **shell 为 pwsh/powershell 时 mise 传 `-NoProfile`**，官方理由：「防止会修改 PATH 的 profile（例如 mise 激活片段）遮蔽任务自己安装的工具」。

### 2.7 monorepo 与发现规则

`monorepo_root = true` + `[monorepo].config_roots`（支持单层 `*`，不支持 `**`）。任务命名空间 `//path/to/project:task`，`:task` 表示当前 config_root。路径通配用 `...`（对齐 Bazel/Buck2），任务名通配用 `*`/`**`。`task_config.includes` 替换（而非追加）默认目录。自动文件系统遍历发现**已废弃**，改为显式声明——理由是「Fast discovery: No filesystem walking needed」，与 §5.5 的性能教训一致。

---

## 3. 横向对照：其他任务运行器

| 维度 | make | mise tasks | just | Task (go-task) | cargo-make | npm scripts | deno task | Turborepo / moon |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 配置格式 | Makefile | TOML + 脚本文件 | justfile | YAML | TOML | JSON | JSON | JSON |
| 文件级增量 | ✅ 核心机制 | ✅ mtime（可切 blake3） | ❌ 明确不做 | ✅ checksum（默认）/ timestamp / none | 部分 | ❌ | ❌ | ✅ 内容哈希 |
| pattern rule | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| 任务依赖 | ✅ | ✅ 三种（depends/wait_for/depends_post） | ✅ | ✅ | ✅ | pre/post 钩子 | ✅ | ✅ 拓扑 `^build` |
| 并行 | ✅ `-j` + jobserver | ✅ 默认 4 | ⚠️ 有限 | ✅ | ✅ | 需 npm-run-all | ✅ `--jobs` | ✅ |
| 参数化 | ❌（只能靠变量） | ✅ usage spec | ✅ 原生参数 | ✅ vars | ✅ | ❌ | ❌ | ❌ |
| Windows 无需额外依赖 | ❌ | ✅ | **❌ 默认需 PATH 里有 `sh`** | ✅ | ✅ | ⚠️ cmd 语法差异 | ✅ 内置 sh 子集 | ✅ |
| 内嵌脚本语言 | ❌ | ❌（靠 shebang 调外部解释器） | ❌ | ❌ | ✅ duckscript | ❌ | ✅ JS 本身 | ❌ |
| 远程任务定义 | ❌ | ✅ HTTP + `git::` | ❌ | ✅ 实验 | ❌ | ❌ | ❌ | ❌ |

来源：https://taskfile.dev/reference/schema/ 、https://taskfile.dev/docs/guide 、https://sagiegurari.github.io/cargo-make/ 、https://docs.npmjs.com/cli/v11/using-npm/scripts/ 、https://docs.deno.com/runtime/reference/cli/task/ 、https://turborepo.dev/ 、https://pypi.org/project/rust-just/1.52.0/

### 3.1 该抄什么

1. **mise 的 `sources`/`outputs` + 「任务定义自动算 source」**——增量能力的性价比最高点。
2. **mise 的 usage spec**（且直接做成 spec 式，不走已被废弃的模板函数弯路）。
3. **mise 的 `run_windows`**。（mise 的 pwsh `-NoProfile` 处理**不该抄**，理由见 §6.5——这是初稿的一处错误结论。）
4. **Task 的 `method: checksum|timestamp|none`**——把判据做成可选项比钉死一种好；mise 事后也补了 `source_freshness_hash_contents`，说明单一 mtime 不够。
5. **mise 的 `{task}`/`{tasks}` 执行步骤与 `depends` 图的区分**——它解决了「先 A 后 B‖C」这个 `depends` 表达不了的需求。
6. **deno task 的内置跨平台 shell 子集**思路作为长期备选（§6.3）。

### 3.2 该避什么

1. **`just` 的「Windows 默认需要 `sh`」**——对 osdk 是致命的，它的卖点正是免外部依赖。
2. **make 的 pattern rule 与自动变量**——见 §4。
3. **npm scripts 的「直接把字符串丢给平台默认 shell」**——这是 `cross-env`/`rimraf` 这类补丁包存在的根本原因。
4. **mise 的 Tera 模板参数函数**——上游自己已废弃并写明了三条理由。
5. **mise 的 `confirm` 语义**——它只守 `run`，`depends` 已经执行完了，官方用 WARNING 标注。这是个设计缺陷，osdk 若做确认应当守住整个子树。
6. **cargo-make 内嵌 duckscript 的选择**——实测 duckscript + SDK 是 **3,415 KiB**（§5），比 Lua 贵 8 倍，语言本身表达力却更弱。

---

## 4. 「可代理 makefile」：能力逐项裁决

| make 能力 | 是否刚需 | mise 覆盖 | osdk 建议 |
| --- | --- | --- | --- |
| `.PHONY` | ✅ 刚需 | 隐含（任务默认非文件目标） | **默认即如此**，不需要这个概念 |
| 任务依赖与拓扑排序 | ✅ 刚需 | `depends` | **做**，并区分 `depends` / `wait_for` |
| 文件级时间戳增量 | ✅ 刚需 | `sources`/`outputs` | **做**，任务级粒度，判据可选 mtime/hash |
| 并行 `-j` | ✅ 刚需 | `--jobs`，默认 4 | **做**，复用 `settings.jobs` |
| 变量 | ✅ 刚需 | `[vars]` + `[env]` | **做**，与现有 `[settings]` 层次一致 |
| 自动变量 `$@`/`$<`/`$^` | ❌ | 不提供 | **不做**——它们只在 pattern rule 语境下有意义 |
| pattern rule `%.o: %.c` | ❌ | 不提供 | **不做**，理由见下 |
| 隐式规则库 | ❌ | 不提供 | **不做** |
| order-only 前置 `\|` | ❌ | 不提供 | **不做**，`wait_for` 覆盖了实际需求 |
| 递归 make / jobserver | ❌ | 不提供 | **不做** |

**为什么 pattern rule 不该做。** 它的本质是「用一条规则声明 N 条文件级规则」，一旦提供，用户合理期待的下一步就是：自动推导依赖链（`.c` → `.o` → 可执行文件的传递闭包）、VPATH 搜索、双冒号规则、`.SECONDARY`/`.INTERMEDIATE` 中间文件生命周期管理。这不是「多做一个功能」，是变成构建系统。而**现代任务运行器无一提供它**（表 §3 中该行全是 ❌），这是七个独立项目的一致判断。

真实世界里，需要 pattern rule 的场景（编译一棵源码树）已经由各语言自己的构建器（cargo / tsc / go build）承担了，任务运行器的职责是**以正确的工具版本和环境调用它们**，并在输入没变时跳过。osdk 覆盖到「任务级增量」正好命中这个职责边界。mise 官方在 monorepo 文档里把这件事讲得很直白：「declare prerequisites in mise, or delegate the complete build graph to an existing build system. Avoid duplicating conflicting graphs.」

---

## 5. 嵌入式脚本语言候选：实测

### 5.1 测量方法

**所有数字来自同一台机器、同一次会话、同一套 profile。** 试验工程在 `E:\tmp\osdk-script-size\`（仓库外），13 个 crate 共用一个 workspace，`[profile.release]` **逐字复制自 one-sdk 的 workspace manifest**：

```toml
[profile.release]
opt-level = "z"
lto = "fat"
codegen-units = 1
strip = true
panic = "abort"
```

每个 crate 共享同一个 harness（`harness.rs`）：从 argv 指定的路径读 TOML、解析出带 `run`/`depends`/`sources`/`outputs` 的任务表、消费每个字段。差别只在「拿到 `run` 字符串之后做什么」：

- `baseline`：只打印（serde + toml 的地板成本）
- `shellonly`：**对照组**，无内嵌语言——globset 匹配 sources/outputs 做 freshness、手写拓扑排序、spawn 进程
- 其余 11 个：用各自引擎求值，并注册一个 `sh()` 宿主函数（确保 FFI 双向都被链接，链接器无法裁掉）

构建与测量：

```powershell
$env:CARGO_TARGET_DIR = "E:\tmp\osdk-script-size\target"
cargo build --release -p <crate>
(Get-Item "...\release\<crate>.exe").Length
```

**「能编译」不算证据。** 每个二进制都跑了冒烟测试，确认解释器真的求值了（各自语法的 `40+2`），见 §5.3。

环境：Windows x64，`rustc 1.98.0`，host `x86_64-pc-windows-msvc`，MSVC 2019 BuildTools。

### 5.2 结果

| 候选 | crate 版本 | 许可证 | 字节 | 相对 baseline | 依赖 crate 数 | 需 C 编译器 |
| --- | --- | --- | --- | --- | --- | --- |
| `baseline`（serde+toml 地板） | — | — | 288,256 | — | 17 | 否 |
| **mlua + Lua 5.4 vendored** | mlua 0.10.5 / lua-src 547.0.0 | **MIT** | **719,872** | **+421.5 KiB** | 32 | **是** |
| **不嵌语言（shell + 声明式）** | globset 0.4.20 + walkdir 2.5.0 | Unlicense OR MIT | **979,968** | **+675.5 KiB** | 29 | 否 |
| mlua + Luau | luau0-src 0.12.3+luau663 | MIT | 1,051,136 | +745.0 KiB | 32 | 是（C++） |
| rquickjs (QuickJS-NG) | rquickjs 0.13.0 | MIT | 1,159,680 | +851.0 KiB | 22 | 是 |
| Koto | koto 0.16.1 | MIT | 1,133,056 | +825.0 KiB | 43 | 否 |
| Rhai | rhai 1.26.1 | MIT OR Apache-2.0 | 1,421,824 | +1,107.0 KiB | 35 | 否 |
| Steel (Scheme) | steel-core 0.8.3 | MIT OR Apache-2.0 | 2,888,704 | +2,539.5 KiB | 126 | 否 |
| Starlark | starlark 0.14.2 | **Apache-2.0** | 3,099,136 | +2,745.0 KiB | 153 | 否 |
| Gluon | gluon 0.18.4 | MIT | 3,133,952 | +2,779.0 KiB | 96 | 否 |
| Duckscript + SDK | duckscript 0.10.0 / sdk 0.11.1 | **Apache-2.0** | 3,785,216 | +3,415.0 KiB | 148 | 否 |
| Boa (JS) | boa_engine 0.22.0 | Unlicense OR MIT | 4,361,728 | +3,978.0 KiB | 143 | 否 |
| Nickel | nickel-lang-core 0.19.0 | MIT | 5,478,912 | +5,069.0 KiB | 226 | 否 |

许可证来自 `cargo metadata` 解析出的 lockfile 实际版本（命令见 §11.3），非 crates.io 页面印象。**全部为宽松许可（MIT / Apache-2.0 / Unlicense OR MIT），无 GPL/MPL，与 osdk 的 MIT 分发相容。** 唯一需要注意的是 Starlark 与 Duckscript 是纯 Apache-2.0（无 MIT 双授权），带专利条款与 NOTICE 要求——这不构成阻碍，但需要在分发物里保留声明。

顺带核查的被排除项：`hematita` 是 **GPL-3.0**（对 osdk 这种分发型产品直接出局），`quick-js` 0.4.1 文档写明「Windows 仅支持 MSYS2 环境与 `x86_64-pc-windows-gnu` 目标」，与 osdk 的 MSVC 主线冲突。

### 5.3 冒烟验证（确认不是「编译通过就算数」）

```
baseline         exit=1  out=t: echo hi deps=0 src=0 out=0
eng-mlua-lua54   exit=0  out=t => Integer(42)
eng-mlua-luau    exit=0  out=t => Integer(42)
eng-rquickjs     exit=0  out=t => Int
eng-koto         exit=0  out=t => Number
eng-rhai         exit=0  out=t => 42
eng-steel        exit=0  out=t => [42]
eng-starlark     exit=0  out=t => None
eng-gluon        exit=0  out=t => 42
eng-boa          exit=0  out=t => 42
eng-nickel       exit=0  out=t => NickelValue{...Number(42)}
eng-duckscript   exit=0  out=duck-ran | t => ok
shellonly        exit=0  out=prep-ran | build-ran   （依赖链真的跑了）
```

starlark 的 `=> None` 看起来可疑（模块级求值本就返回 None），所以**额外做了反向验证**：喂它 `fail("boom")`，退出码 1；喂正常脚本，退出码 0。两个方向都对，说明求值器确实在工作，不是空转。

### 5.4 接进 osdk 的真实代价（关键实测）

独立试验的 +421.5 KiB 只是参考值——osdk 已经链接了大量运行时支撑，真实增量只能在 osdk 里量。做法：`robocopy` 一份工作区到 `E:\tmp\osdk-mlua-probe`（排除 `target`/`.git`），在副本里：

1. `osdk-core` 增加 `scripts = ["dep:mlua"]` feature，`default = ["install", "scripts"]`
2. 新增 `#[cfg(feature = "scripts")] pub mod scripts;`，内含 `eval_task()` 与 `sh()` 宿主函数
3. `osdk-cli` 增加一个**活的调用点**（读 `OSDK_PROBE_SCRIPT` 环境变量并求值）——没有活调用点的话链接器可以裁掉 mlua，量出来的省是假的
4. 两次独立 `cargo build --release -p osdk-cli` / `-p osdk-shim`

| 二进制 | 基线 | 加 mlua 后 | 增量 | 占比 |
| --- | --- | --- | --- | --- |
| `osdk.exe` | 12,870,144 | **13,174,784** | **+304,640 B** | **+2.37%** |
| `osdk-shim.exe` | 3,743,744 | **3,743,744** | **0 B** | **0.00%** |

**shim 一字节未变**，证实 feature 门控确实把引擎挡在了 shim 的依赖图之外。osdk 侧 +2.37%，是 10% 红线的约四分之一。

**同样不能只看体积就下结论**，所以验证了 Lua 路径真的活着（且第一次验证方法是错的：用 `--version` 时 clap 在到达调用点前就退出了，三个用例全返回 0，看起来「全对」其实什么都没测到。换成 `osdk config path` 后）：

```
exit=42    expect=42    body=return 7 * 6
exit=3     expect=3     body=return sh("exit 3")     ← 宿主函数双向可用
exit=-1    expect=-1    body=this is not lua (       ← 语法错误被捕获
no_env_exit=0                                        ← 不设环境变量时常规路径不受影响
```

### 5.5 交互延迟实测，与一个必须记录的陷阱

方法遵循 AGENTS.md：`pwsh -NoProfile`，用 `Diagnostics.Process` 直接起进程不经 shell 管道，12 次取中位数、丢弃前 2 次预热，并断言退出码与输出字符数（防止测到报错路径）。

| 候选 | 中位数 | 说明 |
| --- | --- | --- |
| `baseline` | 12.85 ms | 进程创建占绝对主导 |
| `eng-mlua-luau` | 11.64 ms | 与基线无法区分 |
| `eng-rhai` | 12.57 ms | 同上 |
| `eng-mlua-lua54` | 12.59 ms | 同上 |
| `eng-duckscript` | 12.43 ms | 同上 |
| `eng-koto` / `eng-boa` / `eng-starlark` | 12.69 / 12.72 / 12.87 ms | 同上 |
| `eng-rquickjs` | 13.27 ms | 略高 |
| `eng-nickel` | 15.47 ms | 加载 stdlib |
| `eng-gluon` | 17.45 ms | 同上 |
| `eng-steel` | **180.00 ms** | **异常，见下** |
| `shellonly` | **2,827.91 ms → 61.26 ms** | **我的测量缺陷，见下** |

**两个数字必须解释，因为直接引用会得出错误结论：**

其一，`shellonly` 的 2,827.91 ms **不是这个设计的成本，是我的 harness 的缺陷**：`WalkDir::new(".")` 从 cwd 全量遍历，而 cwd 旁边就是几 GB 的 `target/`。换到干净目录（201 个 marker 文件）重测是 **61.26 ms**，相差 46 倍。这正是 AGENTS.md 里「扫描下探进 conda prefix，一次多走 1,000 个目录」那条坑的同形再现。**设计结论：freshness 扫描必须从 glob 的字面前缀起步（`src/**/*.rs` 只进 `src/`），绝不能从 cwd 全量 walk。**

其二，排查过程中还暴露了 glob 的一个静默失败：`sources = ["**/*.marker"]` **一个文件都没匹配上**，因为 `WalkDir` 产出的路径带 `./` 前缀而 pattern 不带。表现形式是「freshness 检查悄悄什么都没做，任务每次都跑」——不报错、看起来完全正常。改成精确相对路径后两个方向都验证了：

```
should_skip_exit=0   out=build: up to date    （output 比 source 新 → 跳过）
should_run_exit=0    out=build-ran            （touch 了 source → 重跑）
```

**设计结论：glob 规范化（是否带 `./`、分隔符、大小写）必须有双向测试——「该匹配的匹配到」和「不该匹配的没匹配」，否则失效是静默的。** 这正是 AGENTS.md「前缀匹配过宽，与写窄同样危险」那一条的形态。

其三，`eng-steel` 的 180 ms 是引擎自身的初始化成本（Scheme 运行时启动），非测量缺陷——它与其他候选跑的是同一套测量代码。这使 Steel 在「每次 `osdk run` 都要付」的场景下不可接受。

### 5.6 逐个候选的裁决

| 候选 | 裁决 | 理由 |
| --- | --- | --- |
| **mlua + Lua 5.4** | **推荐** | 最小（+421.5 KiB 独立 / +304,640 B 接入实测）、MIT、启动零开销、语言成熟、嵌入 API 是 Rust 生态最成熟的。代价：需 C 编译器（见 §6.4） |
| 不嵌语言（对照组） | **第一阶段采用** | 无 C 依赖，能力足够覆盖 90% 场景。它不是「候选之一」而是「地基」，内嵌语言是在它之上的可选层 |
| rquickjs | 备选 | JS 语法用户熟悉，+851 KiB 可接受。但官方兼容表把 `x86_64-pc-windows-msvc` 标为「experimental!」，对 Windows 一等公民的产品是风险 |
| Rhai | 备选 | 纯 Rust 无 C 依赖是真优势，+1,107 KiB 也还行。落选原因是比 Lua 贵 2.6 倍而语言生态小得多 |
| mlua + Luau | 否 | 比 Lua 5.4 贵 323.5 KiB，且需 C++ 工具链。多出的沙箱能力在本场景**不成立**——任务定义来自已过 trust 门禁的 `osdk.toml`，同一文件的 `run` 本就能执行任意 shell，详见 §6.8 |
| Koto | 否 | +825 KiB 尚可，但语言过于小众，用户学习成本无法用「它很小」抵消 |
| Steel | 否 | 180 ms 启动开销 + 2,539.5 KiB；Scheme 语法对目标用户不友好 |
| Starlark | 否 | +2,745 KiB、153 个依赖。它的核心价值是确定性求值（Bazel 语境），而任务运行天然有副作用，这个价值在此处不成立 |
| Gluon | 否 | +2,779 KiB、17.45 ms，静态类型函数式语言用于写任务是错配 |
| Duckscript | 否 | +3,415 KiB（Lua 的 8 倍），表达力反而更弱。cargo-make 选它是历史路径依赖 |
| Boa | 否 | +3,978 KiB；纯 Rust JS 的代价太高，要 JS 应选 rquickjs |
| Nickel | 否 | +5,069 KiB、226 依赖。它是配置语言不是脚本语言，不适合有副作用的任务执行 |
| hematita | 否（硬性） | **GPL-3.0**，与 osdk 的 MIT 分发不相容 |
| quick-js | 否 | 官方文档：Windows 仅支持 MSYS2 + `windows-gnu` 目标 |

---

## 6. Windows 专章

### 6.1 各方案的 Windows 代价

| 方案 | Windows 能否开箱即用 | 需要什么 |
| --- | --- | --- |
| `cmd /c`（默认 inline shell） | ✅ | 系统自带 |
| `pwsh`（可选 shell） | ⚠️ | PowerShell 7 需单独安装；Windows PowerShell 5.1 系统自带 |
| `sh`（just 的默认） | ❌ | 需 Git for Windows / Cygwin / MSYS2 |
| 内嵌 Lua | ✅ 运行时 | 运行时零依赖；**构建期**需 C 编译器 |
| 内嵌 Rhai / Koto / Starlark 等纯 Rust | ✅ | 构建期也无额外要求 |

### 6.2 osdk 的 shell 策略建议

1. 默认 inline shell：Windows `cmd /c`，Unix `sh -c`。
2. `run_windows` 提供同任务的 Windows 变体（抄 mise）。
3. `shell` 字段允许覆盖。**关于 `-NoProfile`：初稿此处建议「解释器是 `pwsh`/`powershell` 时自动追加 `-NoProfile`」（照抄 mise），该结论已被推翻，正确做法见 §6.5。**
4. **用户为本项目写的脚本需兼容 PowerShell 5.1**，所以 osdk **生成**的任何 `.ps1` 片段（脚手架、`osdk task add` 产物）必须限制在 5.1 语法内：不用 `&&`/`||`、不用三元 `?:`、不用 `??`、不用 `ConvertFrom-Json -AsHashtable`。这是产品约束而非风格偏好。
5. File task 的 Windows 可见性规则照抄 mise：扩展名在白名单内、或有 shebang。并在 `osdk task list` 里**显式提示**「此任务在 Windows 上不可见」，而不是让它静默消失。

### 6.3 一个长期备选：内置跨平台 shell 子集

deno task 的做法是自带一个 sh/bash 子集解析器（`deno_task_shell`），使 `FOO=bar cmd`、`a && b`、`> /dev/null` 这类写法在 Windows 上也成立，还内置了 `rm`/`cp`/`mkdir` 等常用命令。这从根本上消灭了 `cross-env`/`rimraf` 那一类补丁。

**落地后结论：这条路线已废弃，不再是待办。** 写下它时四档还没定形，当时判断它与内嵌 Lua「在能力上部分重叠」；四档全部实现后，重叠变成了**完全覆盖**：

| 原本要靠内置 sh 子集解决的 | 四档里的对应做法 |
| --- | --- |
| `a && b` 的顺序与短路 | `run = ["a", "b"]`，数组本身即是语义（§6.6） |
| 条件分支、循环生成命令 | 第四档 `lua` |
| 跨平台路径拼接 | `osdk.path.join` |
| `FOO=bar cmd` | 任务的 `env` 表 |
| 平台差异 | `run_windows` + `build`/`build.ps1` 配对 |

再引入一个 sh 解析器只会让两套语义并存，并为此付出体积和维护成本。**因此不再需要实测 `deno_task_shell` 的体积**——这个数字不会改变任何决定。

### 6.4 C 编译器依赖的诚实评估

mlua 的 `vendored` feature 通过 `cc` crate 从源码编译 Lua 5.4 并静态链接，不需要系统 Lua 也不需要 pkg-config，但**需要一个 C 编译器**。影响面：

- **本机开发**：已有 MSVC 2019 BuildTools（本次实测即在此环境完成），无额外成本。
- **CI（Windows GNU + Wine 测试套件）**：AGENTS.md 要求的 `./scripts/windows-wine-tests.sh` 已经依赖 `mingw-w64`，其中就有 C 编译器，**不新增前置条件**。
- **交叉编译**：需要目标平台的 C 交叉工具链。这是真实成本，**且我没有实测**（本机只有 msvc 目标 + 若干 Android/Apple/Linux target，未配置对应的 C 交叉编译器）。**标注为待验证：落地前必须在 CI 的每个发布目标上验证 mlua vendored 能构建。** 若某个目标验证失败，退路是把 `scripts` feature 在该目标上关闭（能力降级为前三档），或改用纯 Rust 的 Rhai。

**这一条是整个推荐里风险最高的部分，不应在未验证的情况下进入实现。**

### 6.4.1 用 osdk 自带的 zig backend 化解这个风险（2026-09-19 修订补充）

初稿把交叉编译列为阶段四的阻塞项，但**漏看了本仓库已有的一件事**：osdk 已经内置 zig backend，且 `crates/osdk-core/src/backend/zig.rs:3-7` 的注释正是在论证 zig 的这项能力：

> Zig earns a built-in backend because it is the one toolchain that makes cross compilation work without a separate sysroot per target: it bundles musl, several glibc versions and mingw-w64 headers and libraries, so `zig cc --target=aarch64-linux-musl` produces a working binary on a host that has no cross toolchain installed at all.

也就是说，mlua vendored 所需的「目标平台 C 交叉编译器」，**osdk 自己就有能力提供**：用 `zig cc` 作为 `CC`（配合 `cargo-zigbuild`，或给 `cc` crate 设 `CC_<target>` / `CFLAGS_<target>`），一台机器即可覆盖全部发布目标，不必在 CI 上为每个目标配一套交叉工具链。

额外的好处是形成闭环：**用 osdk 管理的工具链构建 osdk 自己**，zig 版本写进 `osdk.toml` 的 `[tools]`，而不是散落在 CI 配置里。

支持这个判断的两点：Lua 是纯 C89、无平台特有依赖，属于 `zig cc` 最容易成功的一类；mingw-w64 与 musl 头文件 zig 已自带，正好覆盖本仓库的 Windows GNU 与 Linux 目标。

### 6.4.2 zig 路线的实测结果（2026-09-19 补测）

上面的推断已实测，结论是**部分成立**，且边界与预期不同。

环境：Windows x64，zig 0.16.0（由 osdk 自己安装，`E:\osdk-data\data\shims\zig.cmd`），mlua 0.10.5 + lua-src 547.0.0，profile 与 osdk release 一致。

| 目标 | 构建 | 真实运行 | 结论 |
| --- | --- | --- | --- |
| `x86_64-unknown-linux-gnu` | ✅ 661,416 B | ✅ **`lua says 21`**（WSL Ubuntu） | **成立** |
| `aarch64-apple-darwin` | ❌ 链接失败 | — | 缺 macOS SDK，与预期一致 |
| `aarch64-linux-android` | ❌ `string.h` not found | — | 需 NDK sysroot，非 zig 能力问题 |

**Linux 目标完整走通**：产物经 `file` 确认为 `ELF 64-bit LSB pie executable, x86-64 ... dynamically linked`，在 WSL Ubuntu 中执行输出 `lua says 21`——21 是 `1..6` 求和的唯一正确答案，因此这不只是「进程启动了」，而是嵌入的 Lua 解释器真正求值了。判据是产物运行结果而非构建退出码，符合 §6.4.1 自己定的标准。

**两个失败目标的原因都不是 zig 不支持该平台：**

- **macOS**：链接期需要 `libSystem`、`-liconv`，这些在 macOS SDK 里，而 zig 不附带（Apple 的 SDK 有授权限制）。rustc 的报错也指向同一处：`invoking xcrun ... failed: program not found`。解决路径是提供 SDK（`SDKROOT`）或换用 `cargo-zigbuild` 配合已有 SDK，属于独立的一步。
- **Android**：bionic libc 的头文件由 NDK 提供，zig 自带的是 musl/glibc，所以 `string.h` 找不到。给出 NDK sysroot 即可，osdk 本身就有 android-ndk backend。

### 6.4.3 实测过程中两个方法论教训

**其一，前两轮「全部失败」是我的包装器写错，不是 zig 的问题。** 第一轮四个目标全挂，错误是 `unable to parse target query 'x86_64-unknown-linux-gnu': UnknownOperatingSystem`——看起来像 zig 不认识这个平台，实际是 `cc` crate 自己会传 `--target=<rust triple>`，与包装器硬编码的 zig 三元组冲突。**错误信息指向的位置与根因相距甚远**，若就此得出「zig 不支持」的结论，会直接把整条路线误判掉。

**其二，第三轮失败暴露了一个「在被污染状态上验证」的典型。** 包装器用 Python 实现，构建报 `Compiler family detection failed`，而手工运行同一个包装器却成功。差别在于：`python` 在本机是 osdk 的 shim，交互 shell 里有激活所以能跑，而 cargo 的 build script 环境没有，于是 `osdk-shim: no version of python selected`。**手工验证通过恰恰掩盖了问题**——改用纯 `.cmd` 包装器后 Linux 立刻走通。这与 AGENTS.md「复用了被污染的状态」记的是同一类。

### 6.4.4 对阶段四的结论

**mlua 路线可行，但交叉编译不是「一个 zig 解决全部」。** 现实的落地方式是分目标处理：

- Linux（gnu/musl）：`zig cc` 直接可用，已实测。
- Windows：本机 MSVC 或 CI 的 mingw-w64，本就不需要交叉。
- macOS：需 SDK，建议在 macOS runner 上原生构建，而不是硬凑交叉。
- Android：给 NDK sysroot，osdk 已有该 backend。

这与「退回 Rhai」相比仍然划算：Rhai 省掉的是全部 C 工具链问题，但体积代价是 **+1,107 KiB vs +421.5 KiB**（3.6 倍）。macOS 与 Android 在 CI 上原生构建是常规做法，不构成阻塞。

**因此阶段四继续走 mlua**，但 §12 的「阻塞性前置条件」降级为「CI 矩阵的配置工作」——它不再是可行性未知，而是已知可行、需要逐目标配置。

### 6.5 `-NoProfile` 的修正：不要照抄 mise（2026-09-19 修订）

初稿在 §3.1 第 3 条与 §6.2 第 3 条建议「shell 是 pwsh/powershell 时自动追加 `-NoProfile`」，理由是抄 mise。**这条结论对 osdk 是错的，此处修正。**

mise 加 `-NoProfile` 是为了防止 profile 里的激活片段**遮蔽**任务声明的工具版本。但 osdk 的 PATH 注入有两套独立机制（`crates/osdk-core/src/activate/mod.rs:3-9`），它们对 profile 的依赖程度完全不同：

| 机制 | 如何生效 | `-NoProfile` 下 |
| --- | --- | --- |
| **shims**（默认） | `data/shims` 目录写进持久 PATH，由 osdk 安装工具时写入 | ✅ 不受影响，子进程照常继承 |
| **shell activation** | `osdk activate powershell` 往 profile 注入钩子，每个提示符跑 `osdk hook-env` | ❌ 完全失效 |

后果是分层的，既不是「全丢」也不是「无影响」：

| 能力 | `-NoProfile` 下 | 原因 |
| --- | --- | --- |
| 调用 `node` / `cargo` / `go` | ✅ 正常且版本正确 | shim 在被调用时现场解析项目配置 |
| `$env:JAVA_HOME` / `GOROOT` 等 | ❌ 不会被设置 | 由 hook-env 导出 |
| 被 `ShimSettings` 排除、未生成 shim 的工具 | ❌ 找不到 | 只有 activation 会把真实 bin 目录前置 |

因此本仓库 `AGENTS.md`「与 sdk 有关的命令不要使用 `-NoProfile`，这会导致缺失 sdk 注入」是准确的，与 mise 的做法**真实冲突**，不能两者都抄。

**修正后的建议：默认不加 `-NoProfile`，改由 osdk 主动控制任务环境。**

1. osdk 在启动任务子进程前**自己计算并注入该任务的环境**（`hook-env` 的计算逻辑已存在，复用即可），包括 PATH 前置与 `JAVA_HOME`/`GOROOT` 一类导出变量。
2. 同时设置 `OSDK_TASK=1` 一类标记；`osdk activate` 生成的 profile 钩子片段**检测到该标记就跳过自身激活**。
3. 如此既不丢 hook-env 才有的环境变量，又消除了「外层 profile 遮蔽任务声明版本」的风险——两个目标同时满足，而 `-NoProfile` 只能满足后者。
4. 用户若在 `shell` 字段里显式写 `pwsh -NoProfile -Command`，尊重用户选择，不做改写。

**这一条需在实现前实测验证**：构造「工具 A 有 shim、工具 B 被 `ShimSettings` 排除、且依赖 `JAVA_HOME`」的场景，分别在加与不加 `-NoProfile` 下运行同一任务，比对三项能力的实际可用性。判据是任务内 `Get-Command` 的解析结果与环境变量实际取值，不是任务退出码。

### 6.6 `&` 与并行：为什么不能交给 shell 操作符（2026-09-19 补充）

`&` 在三种 shell 下语义互不兼容。**本机实测**（pwsh 7.6.6，Windows x64，2026-09-19）：

| shell | `a & b` 的含义 | 实测证据 |
| --- | --- | --- |
| Unix `sh` | a 放入后台，b 立即开始 | 并发（通识，本机未验证） |
| **Windows `cmd`** | **a 跑完再跑 b，无条件执行** | `cmd /c "dir Z:\no-such >nul 2>nul & echo RAN_ANYWAY"` → 输出 `RAN_ANYWAY`、`exit=0`，**前一条的失败被吞掉** |
| **PowerShell 7** | 尾随 `&` = 后台 Job | `Start-Sleep -Milliseconds 500 &` 在 260 ms 返回，确为异步 |
| **PowerShell 5.1** | **parse error** | `不允许使用与号(&)。& 运算符是为将来使用而保留的` |

对照组：同一条命令把 `&` 换成 `&&` 时 `exit=1` 且后续不执行，确认上表测的确实是 `&` 与 `&&` 的语义差异，而不是命令本身没失败。

即 cmd 的 `&` 与 Unix 的 `&` **语义完全不同**：前者是「无条件顺序」，后者是「并发」。同一份 `run` 在两个平台上会做两件不同的事，而且都不报错——属于 AGENTS.md「只看表象，不看产物」那一类静默失效。

**结论：`run` 数组内不应依赖 `&`，两个诉求各给专门字段。**

**（一）失败也继续**——即 cmd `&` 的真实语义。make 用 `-` 前缀、Task 用 `ignore_error`、cargo-make 用 `ignore_errors`/`force`，三家都用专门字段而非 shell 操作符，是一致判断：

```toml
[tasks.ci]
run = [
  { cmd = "cargo clippy -- -D warnings", ignore_error = true },
  "cargo test",
]
```

**（二）真并行**——用 mise 已验证的 `{ tasks = [...] }` 执行步骤，官方原话「a `{ tasks = [...] }` entry runs its listed tasks in parallel」：

```toml
[tasks.check]
run = [
  "cargo fmt --check",                    # 串行
  { tasks = ["clippy", "test", "doc"] },  # 三者并行
  "echo all-green",                       # 等上面全部完成
]
```

相比 `&`，这种写法让 osdk 能**等待子任务并收集退出码**；`&` 扔出的后台进程无人回收，失败会被静默丢弃。

`depends` 不能替代它：官方明确 prerequisites 的顺序不构成序列（「their order in `depends` does not establish a sequence」），因而表达不了「先 A，再 B‖C」。两种机制职责不同，都需要。

来源：https://mise.jdx.dev/tasks/task-configuration.html 、https://mise.jdx.dev/tasks/ 、https://taskfile.dev/usage/ 、https://docs.rs/crate/cargo-make/0.3.35 、https://docs.w3cub.com/gnu_make/errors

**一个实测中发现的陷阱**：本机 `Get-Command sh` 解析到 `E:\osdk-data\data\shims\sh.cmd`——是 osdk 自己的 shim。这意味着在装了 osdk 且装过某个提供 `sh` 的工具的机器上，`sh` 确实可用。**但这恰恰让「默认 shell 选 sh」更危险**：开发机上因为这个 shim 悄悄能跑，裸机用户却会失败，问题被推迟到用户侧才暴露。§6.2 第 1 条「默认 `cmd /c`」的结论因此更稳固。

### 6.7 Lua 版本选择：为什么是 5.4（2026-09-19 补充）

初稿直接用了 Lua 5.4 而未说明理由，此处补全。对 osdk 的场景而言，5.4 / 5.3 / 5.1 / LuaJIT 之间的差异只有以下几条真正相关：

- **5.4 是当前维护版本**，也是 `mlua` 的默认档与 `lua-src` vendored 构建最顺畅的一档。
- **5.3 引入的整数子类型**（5.4 延续）对写任务有实际价值：路径拼接与数值计算不会莫名产生 `1.0` 这类浮点表示。
- **LuaJIT / 5.1 的唯一优势是生态**（大量老库、OpenResty 系）。osdk 的场景是写任务脚本，不消费第三方 Lua 库生态，这个优势为零；而 LuaJIT 引入汇编后端，在 aarch64-windows 一类目标上反而是负担。
- **体积差异不构成选型依据**：几个版本都在数百 KiB 量级，差异远小于候选引擎之间的差距。

结论：**选 5.4 是「采用默认值」，而非权衡后的结果——但这个默认值没有需要推翻的理由。** 若将来 zig 交叉编译路线（§6.4.1）在某目标上受阻，换版本并不能解决问题（受阻的是 C 工具链而非 Lua 版本），届时应走 Rhai 退路而不是降级 Lua 版本。

### 6.8 为什么不要 Luau 的沙箱（2026-09-19 补充）

§5.6 以「多出的沙箱与类型能力 osdk 用不上」否掉了 Luau，理由过简，此处补全。

Luau 沙箱提供的是：只读全局表、屏蔽 `debug`/`io`/`os` 危险面、指令数与内存配额、可中断执行。实测代价为 **+323.5 KiB**（1,051,136 vs 719,872 B），且把构建期依赖从 C 抬升到 **C++**。

**这套能力的目标场景是「执行不可信的第三方代码」**（Luau 的来源 Roblox 要跑玩家脚本）。而 osdk 的任务脚本来自 `osdk.toml`，该文件**已经过 trust 门禁**（§8.4：未知顶层表 fail-closed 归为 `ExecutesCode`，`[tasks]` 一出现即要求显式 trust）。也就是说用户已明确授权这个文件可以执行代码，且同一文件里的 `run` 字段本就能执行任意 shell 命令。

**在一个已经能执行任意 shell 命令的文件里，给其中的 Lua 加沙箱不构成任何实际保护**——攻击者不会去绕过 Lua 沙箱，直接写 `run` 即可。这 323.5 KiB 买到的是一个在本场景下不成立的保护。

反过来说，若将来 osdk 要支持**远程任务定义**（§8.4 明确列为不做），信任边界会发生根本变化，那时才需要重新评估沙箱——但届时真正的答案多半是进程级隔离而非语言级沙箱，因为 `run` 字段的问题依然存在。

---

## 7. 四档分层设计

核心约束：**升档不推翻。** 每一档都是上一档的严格超集，低档写法在高档存在时语义不变。

### 第一档：单行字符串

```toml
[tasks]
build = "cargo build --release"
test = "cargo test"
```

覆盖大多数场景。与 mise 的 trivial task 完全一致。

### 第二档：多行 / 多条命令 + 声明式元数据

```toml
[tasks.build]
description = "构建发布产物"
run = ["cargo fmt --check", "cargo clippy -- -D warnings", "cargo build --release"]
run_windows = "pwsh -File scripts/build.ps1"
depends = ["fetch-deps"]
sources = ["Cargo.toml", "crates/**/*.rs"]
outputs = ["target/release/osdk.exe"]
env = { RUST_BACKTRACE = "1" }
dir = "{{cwd}}"
```

或多行字符串：

```toml
[tasks.release]
run = """
cargo build --release
./scripts/package.sh
"""
```

### 第三档：独立脚本文件

```toml
[tasks.release]
file = "scripts/release.ps1"
```

外加文件任务目录（`osdk-tasks/`），脚本用注释头携带元数据。好处是编辑器能提供语法高亮与 lint。

### 第四档：内嵌 Lua（可选 feature）

只在需要条件分支、循环、跨平台路径计算时才用：

```toml
[tasks.sync-models]
lua = """
local targets = osdk.config("models.targets")
for _, t in ipairs(targets) do
  if osdk.platform.os == "windows" then
    osdk.sh(("osdk model pull %s --dest %s"):format(t, osdk.path.join(osdk.project_root, "models", t)))
  else
    osdk.sh(("osdk model pull " .. t))
  end
end
"""
```

**字段名 `lua` 而非 `run`，是刻意的**：一眼可辨这是哪一档，且 `run` 与 `lua` 互斥时可以给出明确报错，而不是靠猜测内容格式来分派。

### 升级路径

```
build = "cargo build"                              第一档
  ↓ 加元数据，run 字符串原样保留
[tasks.build] { run = "cargo build", sources=[...] }   第二档
  ↓ 命令变长，run 换成数组或多行字符串
[tasks.build] { run = ["...", "..."] }                 第二档（同档内演进）
  ↓ 脚本超过 ~20 行，挪进文件
[tasks.build] { file = "scripts/build.ps1" }           第三档
  ↓ 确实需要程序化逻辑
[tasks.build] { lua = "..." }                          第四档
```

四档之间**没有一步是「必须先重写别的东西」**。项目里 20 个任务，19 个停在第一档、1 个用第四档，是完全正常且不别扭的状态。

---

## 8. `osdk.toml` schema 草案

### 8.1 顶层结构

```toml
# 与现有 [settings] / [tools] / [aliases] 平级的新顶层表
[tasks.<name>]
# ... 见下表

# 任务运行器的作用域级默认值
[task_config]
shell = "pwsh -Command"              # 本配置作用域内的默认解释器（不加 -NoProfile，见 §6.5）
dir = "{{config_root}}"
includes = ["osdk-tasks", "tasks.toml"]

# 模板变量（不导出为环境变量）
[vars]
profile = "release"
```

### 8.2 `[tasks.<name>]` 字段表

| 字段 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `run` | string \| 数组 | — | 命令。数组元素**串行且任一失败即中止**；元素可为 `{cmd="…", ignore_error=true}`（失败继续）、`{task="x"}`（串行子任务）或 `{tasks=["a","b"]}`（并行）。**不要用 shell 的 `&`**，其语义跨平台不一致（§6.6） |
| `run_windows` | 同 `run` | — | Windows 变体；存在时在 Windows 上完全取代 `run` |
| `file` | string | — | 外部脚本相对路径（相对配置文件所在目录）。**首版不支持远程 URL**，理由见 §8.4 |
| `lua` | string | — | 内嵌 Lua（需 `scripts` feature）。与 `run`/`file` 互斥 |
| `description` | string | — | `osdk task list` 与 `--help` 显示 |
| `alias` | string \| 数组 | — | 别名 |
| `depends` | string \| 数组 | `[]` | 前置任务；支持 `{task, args, env}` |
| `wait_for` | string \| 数组 | `[]` | 已调度则等待，不主动加入 |
| `depends_post` | string \| 数组 | `[]` | 本任务完成后运行 |
| `env` | map<string,string> | `{}` | 任务级环境变量，不传给 `depends` |
| `dir` | string | `{{config_root}}` | 工作目录 |
| `sources` | string \| 数组 | — | 增量输入，glob；`!` 前缀排除 |
| `outputs` | string \| 数组 \| `{auto=true}` | 定义 sources 时为 `{auto=true}` | 增量输出 |
| `freshness` | `"mtime"` \| `"hash"` \| `"always"` | `"mtime"` | 判据（借鉴 Task 的 `method`） |
| `shell` | string | 继承 `task_config.shell` | 解释器覆盖 |
| `usage` | string | — | 参数 spec |
| `tools` | map<string,string> | `{}` | 任务专属工具版本，复用现有 `[tools]` 解析 |
| `hide` | bool | `false` | 从列表与补全中隐藏 |
| `quiet` | bool | `false` | 抑制 osdk 自身输出（不影响任务输出） |
| `timeout` | string | — | `30s` / `5m` / `1h` |
| `platforms` | 数组 | — | 复用现有 `[tools]` 的平台过滤语义 |

### 8.3 合并语义（必须与现有层次一致）

现有 `ConfigFile::apply_file` 的既定模式是：**`[settings]` 整体替换、`[tools]` 逐条合并、`[sources]` 顶层旋钮替换而 per-tool 映射合并、`[registries]`/`[containers]` 整表替换、`[aliases]` 逐条 extend**。

`[tasks]` 应当采用**与 `[tools]` 同构的逐条合并**：

- 同名任务：高优先层（项目配置）**整体替换**低优先层（用户全局）的该任务定义，**不做字段级合并**。理由：字段级合并会产生「项目配置只想改 `description`，结果继承了全局的 `run`」这类难以推理的结果；而整体替换的语义是「这个名字归我了」，一眼可判。
- 不同名任务：并集。
- `[task_config]`：**整表替换**，与 `[registries]`/`[containers]` 一致。
- `[vars]`：逐条 extend，与 `[aliases]` 一致。
- `platforms` 过滤：在 merge 阶段就剔除不匹配的条目，并记入类似 `excluded_tools` 的结构，使 `osdk run <name>` 能回答「这个任务在本平台被平台过滤排除了」而不是「未知任务」。这与 `apply_tool_configs` 现有做法一致。

环境变量覆盖：新增 `OSDK_TASK_JOBS`、`OSDK_TASK_OUTPUT`、`OSDK_TASK_TIMEOUT`，遵循现有 `apply_env` 模式。

### 8.4 信任模型（这一节是安全边界，不是配置细节）

**现状核查结果：`trust.rs` 的 `collect_requirements` 对未知顶层表 fail-closed，归为 `TrustReason::ExecutesCode`。** 这意味着：**一旦 `osdk.toml` 出现 `[tasks]` 表，现有 osdk 会立即要求用户显式 trust——无需改动任何代码，默认就是安全的。** 这是个非常好的起点。

但需要主动做三件事：

1. **把 `tasks` 加进 `TRUST_REQUIRING_TABLES` 并显式标注 `ExecutesCode`。** 不是为了改变行为（fail-closed 已经这么做了），而是为了让拒绝信息能说清「`tasks` —— 会在本机执行任意代码」，而不是笼统的未知表。`trust.rs` 的注释已经点明这个设计意图：「A person told only 'this config is untrusted' has to diff it against nothing」。
2. **`task_config` 与 `vars` 同样要显式分类。** `task_config.shell` 直接决定用什么解释器跑所有任务，是彻头彻尾的 `ExecutesCode`。`vars` 会被插值进命令，同样如此。**不要因为「看起来只是变量」就放进安全列表。**
3. **首版不支持远程任务定义（`file` 的 HTTP/`git::` 形式、`includes` 的 `git::` 形式）。** mise 支持这些，但它们把「信任一个本地文件的内容哈希」变成了「信任一个会变的远程端点」，与 osdk 现有的内容绑定信任模型（`normalized_hash`）根本冲突。要做的话必须先解决「远程内容变了怎么重新征求同意」，这是独立的设计工作。

### 8.5 CLI 管理入口（用户不手改管控目录下的配置）

用户明确要求配置一律走 osdk 自身命令。现有 `config_edit.rs` 已提供 `osdk config set/unset` 的 `toml_edit` 保序写入能力，任务管理应复用它：

| 命令 | 作用 |
| --- | --- |
| `osdk task list [--hidden] [--json]` | 列出任务（名称、描述、来源文件、是否被平台过滤） |
| `osdk task add <name> --run <cmd> [--depends x] [--desc s]` | 写入 `[tasks.<name>]`（对齐 `mise tasks add`） |
| `osdk task rm <name>` | 删除任务 |
| `osdk task edit <name>` | 在 `$EDITOR` 打开（file task 打开脚本，toml task 定位到段落） |
| `osdk task info <name>` | 展示解析后的完整定义、来源层、生效的合并结果 |
| `osdk task deps [<name>]` | 打印依赖图（区分 `depends` 边与 `run` 内的执行步骤） |
| `osdk run <name> [-- args]` | 执行 |
| `osdk run <name> --dry-run` | 只打印将执行什么，包括 freshness 判定结果 |

**`osdk run` 而非 `osdk task run` 作为主入口**，因为它是最高频操作。但要注意 mise 踩过的坑：`mise <task>` 这种省略形式会被未来新增的子命令遮蔽，官方明确建议脚本里不要用。osdk 应当**只提供 `osdk run <name>`，不提供 `osdk <name>`**，从一开始就避免这个问题。

---

## 9. 落地阶段

### 阶段一：声明式基础（无新依赖，无体积增长）

- `[tasks]` 解析与分层合并；`trust.rs` 显式分类
- `run` / `run_windows` / `description` / `alias` / `dir` / `env` / `depends` / `hide` / `quiet`
- 拓扑排序 + 并行执行（复用 `settings.jobs`）
- `osdk run` / `osdk task list` / `osdk task info` / `osdk task deps`
- 输出前缀化（并行时避免交错）

体积影响：**接近零**。所需的 toml/serde 已在依赖图内。

**立刻可用**：这一阶段结束后就能替代大多数 Makefile 与 npm scripts 的用途。

### 阶段二：增量与参数

- `sources` / `outputs` / `freshness`；任务定义自身自动算作 source
- **glob 扫描从字面前缀起步**（§5.5 的教训），并补双向测试
- `usage` spec：解析、校验、`--help` 生成、shell 补全
- `timeout` / `depends_post` / `wait_for`
- `osdk task add` / `rm` / `edit`

体积影响：`globset` 与 `walkdir` **已实测确认在 `osdk-cli` 的依赖图内**（`cargo tree -e normal -p osdk-cli`，450 个不同 crate，两者均命中），因此本阶段不引入新 crate，增量仅为新增代码自身。

### 阶段三：文件任务

- `osdk-tasks/` 目录发现（显式 includes，不做递归自动遍历——mise 已把自动遍历废弃）
- 注释头元数据（`#OSDK` / `#USAGE`）
- Windows 可见性规则 + `.ps1` 配对 + `OSDK_TASK_DIR`
- `file` 字段

### 阶段四：内嵌 Lua（可选）

- `osdk-core` 的 `scripts` feature，`default` 开启、shim 关闭
- `lua` 字段 + `osdk.*` 宿主 API（`sh`、`platform`、`path.join`、`config`、`project_root`）
- **前置门槛：先在 CI 的每个发布目标上验证 mlua vendored 能构建**（§6.4）。**优先尝试 `zig cc` 路线**（§6.4.1）——osdk 已内置 zig backend，一台机器即可覆盖全部目标；判据是产物在目标平台真实运行，不是构建退出码为 0。任一目标失败则该目标关闭 feature 并在文档中说明，或整体退回 Rhai

体积影响：实测 osdk **+304,640 B（+2.37%）**，shim **0**。

### 明确不做（不是「暂未实现」）

1. pattern rule 与自动变量（§4）
2. 远程任务定义（§8.4）
3. 产物级远程缓存（mise 自己还标着 experimental；且需要先有可信的输入声明，「A task wrapper does not make an arbitrary command deterministic」）
4. 沙箱（`deny_*`）：平台差异巨大，Windows 侧支持有限，投入产出比低
5. monorepo 任务命名空间：等有真实需求再做，`config_roots` 这套东西是 mise 迭代了很久才稳定的

---

## 10. 与 AGENTS.md 三节硬约束的对账

| 约束 | 本设计的应对 | 证据 |
| --- | --- | --- |
| 二进制体积是产品指标 | 阶段一至三零增长；阶段四实测 +2.37%（红线的 1/4） | §1.2、§5.4 实测 |
| shim 必须单独构建 | 全部实测分两次 `cargo build` 调用；未用 `--workspace` | §1.2、§5.4 |
| 新依赖判断属于哪一侧 | mlua 置于 `scripts` feature，shim 侧实测 **0 字节增长** | §5.4 |
| `Backend` trait 不加方法 | 任务系统完全不碰 `Backend`，不进 vtable | 设计约束 |
| `hook-env` / shim 延迟 | 两条热路径不解析 `[tasks]`、不初始化引擎；引擎初始化实测 <1 ms | §5.5 |
| 深度上限不是可自由收窄的旋钮 | 同理，freshness 扫描不能为了快而缩范围；正确做法是按 glob 前缀裁剪子树 | §5.5 |
| 验证失效模式：探针须落在被测机制内 | 每个引擎注册 `sh()` 宿主函数、CLI 侧加活调用点，否则链接器裁掉后测的是假的 | §5.1、§5.4 |
| 验证失效模式：绿色不构成证据 | starlark 做了失败方向验证；Lua 探针第一次验证方法是错的并已记录修正 | §5.3、§5.4 |
| 前缀匹配两个方向都要测 | glob 静默失配已实测复现，写入阶段二的强制要求 | §5.5 |

---

## 11. 核查与实测命令附录

### 11.1 确认当前树无 scripts/tasks 实现

```
Grep pattern: \[tasks\]|\btasks\s*:|struct Task\b|mod scripts|mod tasks
path: E:\Projects\one-sdk\crates    glob: *.rs
→ 0 命中
```

搜索范围限定在 `crates/`（代码路径），未包含 `docs/`——避免 AGENTS.md 里记的那个坑：「命中数里可能全是文档在描述这个符号」。

### 11.2 体积实测

```powershell
# 基线（仓库内，独立 target 目录）
$env:CARGO_TARGET_DIR="E:\Projects\one-sdk\target\size-check"
cargo build --release -p osdk-cli
cargo build --release -p osdk-shim

# 候选对比（仓库外临时 workspace）
$env:CARGO_TARGET_DIR="E:\tmp\osdk-script-size\target"
cargo build --release -p <crate>

# 接入代价（仓库副本，robocopy 排除 target/.git）
$env:CARGO_TARGET_DIR="E:\tmp\osdk-mlua-probe\target-probe"
cargo build --release -p osdk-cli
cargo build --release -p osdk-shim
```

### 11.3 许可证核查

```powershell
$meta = cargo metadata --format-version 1 -q | ConvertFrom-Json
$meta.packages | Where-Object { $want -contains $_.name } |
  ForEach-Object { "{0} {1} {2}" -f $_.name, $_.version, $_.license }
```

读的是 lockfile 解析出的**实际版本**的 license 字段，不是 crates.io 页面上的最新版。

### 11.4 延迟实测

`pwsh -NoProfile`，`Diagnostics.Process` 直接起进程，12 次取中位数、丢弃前 2 次预热、断言退出码与输出字符数。

### 11.5 仓库清洁性

```
git status --porcelain  →  （空）
git log --oneline -1    →  847b8f2
```

所有试验产物位于 `E:\tmp\osdk-script-size\` 与 `E:\tmp\osdk-mlua-probe\`，仓库工作区未被改动。

---

## 12. 本文未做的核查（诚实清单）

1. **~~mlua vendored 的交叉编译验证~~ —— 已补测，见 §6.4.2。** zig cc 在 `x86_64-unknown-linux-gnu` 上完整走通，产物于 WSL Ubuntu 实际运行并输出 `lua says 21`；macOS 因缺 SDK、Android 因缺 NDK sysroot 未通过，两者均非 zig 能力问题，建议在对应平台原生构建。阶段四据此继续走 mlua，该项从阻塞性前置降级为 CI 配置工作。
2. **~~`deno_task_shell` 的体积~~ —— 该项已取消。** 四档落地后其能力被完全覆盖（见 §6.3 的对照表），路线本身废弃，体积数字不再影响任何决定。
3. **~~`globset` 在 osdk 现有依赖图中的实际增量~~ —— 已补测。** `cargo tree -e normal -p osdk-cli` 显示 `globset` 与 `walkdir` 均已在图内（450 个不同 crate），阶段二不引入新 crate。
4. **mise 的部分行为未实机验证。** §2 的所有断言来自 2026-09-19 抓取的官方文档，未在本机安装 mise 复现。文档与实现不符的情况是可能的，实现前对关键语义（尤其 sources/outputs 的依赖失效传播）建议实测确认。
5. **~~PowerShell 5.1 兼容性未实测~~ —— 已补测，且抓到一个真实缺陷。** 原表述里「本设计将来会生成的 `.ps1` 脚本」这一前提**并不成立**：四档落地后 osdk 不生成任何 `.ps1`，第三档只是启动用户自己写的脚本。但由此查出了真正的问题——`launch_argv` 曾硬编码 `pwsh`，而 **PowerShell 7 在 Windows 上是独立下载项，系统自带的只有 5.1 的 `powershell.exe`**，于是在未装 PS7 的普通 Windows 上每个 `.ps1` 任务都会以「找不到程序」失败，且错误指向任务而非真正的原因。现改为运行时探测：有 `pwsh` 用 `pwsh`，没有则回退 `powershell`（5.1 同样支持 `-File`，argv 其余部分不变）。实测两个方向：本机装有 PS7 时报告 `PSVersion=7 Edition=Core`；把 `pwsh` 移出 PATH 后报告 `PSVersion=5 Edition=Desktop` 且任务照常完成。此外 5.1 下尾随 `&` 为 parse error 一事已在 §6.6 实测。
6. **`-NoProfile` 的三项能力差异未做端到端实测。** §6.5 的结论基于 `activate/mod.rs:3-9` 的机制阅读与 shims/activation 的职责划分，逻辑链完整，但**未构造真实场景跑一遍**（工具 A 有 shim、工具 B 被 `ShimSettings` 排除、且依赖 `JAVA_HOME`）。实现前必须补这个验证，判据见 §6.5 末段。
7. **`{ tasks = [...] }` 的并行语义来自 mise 官方文档，未实机复现。** §6.6 中 `&` 的跨平台差异是本机实测的，但「mise 如何调度 `{tasks=[…]}`」一节仅有文档依据，与第 4 项同属一类。
