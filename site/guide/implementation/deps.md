# 应用依赖的实现

本页讲 `osdk deps` 内部怎么组织、以及几处取舍为什么是这样。用法见
[应用依赖](../deps.md)。

## 为什么是独立子系统

osdk 原本只有「装一个工具」这一种语义。即便处理得最深的 npm 也是如此：
`npm:<pkg>` backend 会装某个工具自身的完整依赖树，而项目级编排
（`npm_tools.rs`）做的是「把一个包加进 `package.json` 并装好」——粒度是
**逐个依赖**，不是「读整份清单一次兑现应用环境」。

`deps` 补的正是后者。它不重做包管理器已经做得很好的事（解析、下载、写 lock），
只接管 osdk 更适合拿的那几处判断：选哪个 installer、能不能冻结、registry 策略、
把结果读回来、把身份记进 lock。

## 模块划分

```
crates/osdk-core/src/deps/
├── mod.rs      公共层：provider 静态表、discover、select_installer、plan 分派
├── node.rs     Node 系四个 provider 的命令构造与 yarn 分线
└── state.rs    新鲜度状态的读写与判定
crates/osdk-cli/src/deps_cmd.rs   CLI 编排与呈现
```

整个 `deps` 模块挂在 `install` feature 之后（`lib.rs` 里
`#[cfg(feature = "install")] pub mod deps;`）。shim 从不兑现依赖闭包，
所以它的构建里根本没有这些代码。实测 shim 体积因此几乎不变
（+0.05%，属噪声量级）。

同样的理由，`deps` **没有给 `Backend` trait 加任何方法**。
`Registry::new()` 会把全部 backend 实例化成 `Arc<dyn Backend>`，
trait 上每多一个方法就多一条 vtable 条目、链接器无法证明它不可达——
这条曾让 shim 白背 5.15 MB。取真实 bin 路径用的是已有的
`Backend::bin_paths`。

## 为什么不走 shim

最初的想法是通过 osdk 的 shim 调包管理器。实测行不通：

```
$ <data>/shims/pnpm.cmd install
osdk-shim: no version of 'pnpm' selected (set one with 'osdk use pnpm@<version>')
```

shim 要求当前目录已经选定版本，而 `deps` 的典型场景恰恰是「刚把 pnpm 装好、
项目还没 `use` 过」。所以 `deps` 用 `Backend::bin_paths` 拿真实 bin，
并把 node 的 bin 目录**前置进子进程 PATH**——后者是必需的，
依赖的脚本里会直接调 `node`。

## 冻结判据由 osdk 自己持有

这是本子系统最关键的一处设计，来自对四款 installer 的实测（版本见下）：

| installer | 缺 lockfile 时「冻结」命令的行为 |
| --- | --- |
| npm 10.9.8 | `npm ci` exit=1，`EUSAGE` |
| pnpm 12.5.1 | exit=1，`ERR_PNPM_NO_LOCKFILE` |
| yarn berry 4.6.0 | exit=1，`YN0028` |
| **yarn classic 1.22.19** | **exit=0，照常安装，不写 lock** |
| **bun 1.4.2** | **exit=0，照常安装，不写 lock** |

后两行是整个设计的理由：把「是否冻结」交给包管理器判断，会在四款里有两款上
**静默失效**——命令成功、没有警告、只是没有冻结。而 `yarn@1` 还会静默接受 berry 的
`--immutable` 并且既不冻结也不禁脚本，是其中最糟的一种失败形态，因为它看起来
像成功了。

所以 osdk 先自己看 lockfile 在不在，据此决定用哪套参数；退回时
**显式报告**而不是静默降级。`--frozen` 把这个退回变成错误。

## yarn 必须按 major 分线

classic 与 berry 在两个要紧的参数上都不一样，而且不是「名字不同」而是
「一方会静默接受另一方的参数并忽略」：

| | 冻结 | 禁脚本 |
| --- | --- | --- |
| classic 1.x | `--frozen-lockfile` | `--ignore-scripts` |
| berry 2+ | `--immutable` | `YARN_ENABLE_SCRIPTS=false` |

berry 4.6.0 **不接受** `--ignore-scripts`，会直接
`Unknown Syntax Error: Unsupported option name`（其 `install` 只认
`--json` / `--immutable` / `--immutable-cache` / `--refresh-lockfile` /
`--check-cache` / `--check-resolutions` / `--inline-builds` / `--mode`），
所以禁脚本只能走环境变量。

版本来源是 `packageManager` 字段；取不到时按 berry 处理——因为 berry 在缺
lockfile 时会**明确失败**，而 classic 会静默继续，猜错的代价不对称。

顺带一个实测差异：pnpm 12.5.1 与 bun 默认就拦依赖的构建脚本
（pnpm 12 因此会 `ERR_PNPM_IGNORED_BUILDS` 并提示 `pnpm approve-builds`），
但 osdk 仍然显式传 `--ignore-scripts`，不依赖某个版本的默认值。

## 深度校验：判据由实测筛出，而不是由合理性筛出

设计最初写的 L2 判据是「`pyvenv.cfg` 的 creator/解释器仍是记录的那个」加「关键包
元数据存在」。实现前先把每条候选判据对着真实篡改测了一遍，结果这两条**淘汰了**：

| 篡改 | pyvenv.cfg | dist-info 存在 | **RECORD 逐文件** |
| --- | --- | --- | --- |
| 删掉包内一个文件 | 未发现 | 未发现 | **发现** |
| 改一个文件的内容 | 未发现 | 未发现 | **发现** |

原因很直接：它们检查的是**环境怎么建的**，而篡改动的是**环境里装了什么**。这正是
AGENTS.md 说的「断言与被测机制脱离」——保留它们只会让 `--verify` 通过一个已经坏掉的
环境，比没有这层更糟。

留下的是包管理器**自己写的收据**：Python 的 `dist-info/RECORD`（实测 idna 3.10 的
15 行里 14 行带 size 与 sha256）、Node 的 `node_modules/.package-lock.json`
（逐包 version 与 integrity）。osdk 读它们而不另造第二份依赖图——与「原生 lock 才是
真相」是同一个判断。

比较 size 而不是 sha256，是因为它抓到的是同一类篡改（实测追加一行让 13239 变
13251）而成本低得多；一个因为慢而没人跑的校验保护不了任何东西。sha256 仍在 RECORD
里，将来若要 `--verify --deep` 可以再用。

### 反向控制暴露出的更严重的一件事

「干净环境必须全部通过」这一条第一次**没通过**：新建的 venv 仍报 1 处尺寸不匹配。
排查结论不是判据误报，而是 **`uv` 把被篡改的文件写进了它的全局缓存**，此后每个新
venv 都从坏缓存复制。换全新 `UV_CACHE_DIR` 后归零。

这件事有两层含义。方法论上，它正是 AGENTS.md「复用了被污染的状态」那一族——若当时把
「干净环境也报错」当成判据不可靠而放弃 L2，结论会完全反过来。产品上，它说明
**`uv pip sync` 不校验已装文件的内容**（实测被改过的 `core.py` 在 sync 之后仍带着
篡改），所以「重跑一遍就好」并不成立，`--verify` 失败时给出的建议是清缓存 + 重装。

### 「无法检查」不等于「检查通过」

没有收据可读时，L2 报 `ReceiptMissing` 而不是返回一份空的干净报告。`checked == 0`
且没有 finding 会是一次空洞通过——把「什么都没检查」伪装成「没有问题」。报告里因此
一并给出检查了多少条。

`--verify` 在任何安装动作之前短路返回：它是一次检查，不是会改变环境的命令。

## Python：两处与 Node 不对称的地方

两条都来自 2026-09-22 的实测（uv 0.12.17 / CPython 3.12.14），判据同 Node 那轮——
造一个只有 sdist 的包、其 `setup.py` 写标记文件，用**标记文件是否存在**判定源码构建
是否真发生，不看退出码。

**一、禁源码构建只能用 flag，不能用环境变量。** `UV_NO_BUILD=1` 在
`uv pip install` 上被**静默忽略**（exit=0、标记文件存在、源码照样构建），只有
`uv sync` 识别它。`uv pip install --help` 里 `--no-build` 没有 `[env:]` 标注，
而同页的 `--no-build-isolation` 有——同一个变量名在两个子命令上行为不同。

这与 yarn **恰好相反**：berry 拒绝 `--ignore-scripts`，只能靠
`YARN_ENABLE_SCRIPTS=false`。所以 Node 侧「env 是等效手段」的经验**不能平移**到
Python。若按对称性推导，写出来会是一个看起来生效、实际每次都在本机构建源码的开关。

**二、`--frozen` 不等于「lock 当令」。** lock 与 `pyproject.toml` 不一致时
`uv sync --frozen` 仍 exit=0 并按旧 lock 装（新加的依赖没装上），因为它的语义只是
「不更新 lock」。要校验一致性得用 `--locked`。`npm ci` 在同样情形下会失败，所以两个
生态不对称，osdk 对 uv 两个 flag 都传。

## prelude：为什么需要一个「前置命令」概念

`uv sync` 会自建项目环境，而 `uv pip sync` 在没有环境时直接拒绝
（`No virtual environment found`）。这个不对称如果藏进 runner 里隐式补一句
`uv venv`，那么 `--dry-run` 看不到它、freshness 哈希也不包含它。

所以 `RunPlan` 有 `prelude: Vec<Vec<String>>`：同一个程序、同一个 cwd、同一份 env，
按序先跑。它出现在打印出来的命令串里，因此也进入哈希——否则「先建环境再 sync」与
「往已有环境里 sync」会被哈希成同一件事。

prelude 必须**幂等**：`uv venv` 在环境已存在时 exit=2（`Failed to create virtual
environment`），于是第一次之后每次都会在到达 sync 之前失败。用
`--allow-existing` 复用，这也是正确行为——`uv pip sync` 本来就负责让环境内容与
文件一致，重建只会丢掉缓存。

## `[deps.<p>].dir` 曾经声明了却不生效

`dir` 一直在 schema 与文档里，但 provider 直接用了 `project.root`，配置被完全忽略。
一个**声明了却什么都不做的设置比没有这个设置更糟**：项目看起来配好了，命令却跑在别处。

现在两个 provider 共用 `effective_cwd`，并且 `dir` **按两种分隔符读取**——它来自被
提交的 `osdk.toml`，写它的机器不一定是读它的机器，而 `Path::components()` 只认宿主
自己的分隔符。该函数的测试两种写法都在所有平台上跑，没有 `#[cfg(windows)]`：
一旦加了 cfg，就等于声明另一半永不验证，而那正是缺陷的藏身处。

## 工具的获取是委派的

缺包管理器时，`deps` 调 `install_one_without_shims`——就是 `osdk install` 用的那条。
不另造一套的理由很直接：那条路径上已经有源选择、校验、attestation 与 CAS，
第二份实现要同样可靠就得把这些全抄一遍，而抄出来的那份必然先腐烂。

版本来源分两种角色：installer 的版本可以被清单的 `packageManager` 钉住，
runtime 的不行，所以后者取自 `[tools]`，再退到「本机已装的哪个」。

`--no-install-tools` 划的界是**获取**而不是**使用**：已经装好的照常用，
缺的则报错并给出该跑的命令。CI 需要这个区分——工具应当来自一次被审阅过的显式
`osdk install`，而不是某次运行顺手取到的另一个版本。

## PATH 是前置而非追加

`run_plan` 把解析出的 bin 目录**插到 PATH 最前面**。这不是风格问题：npm 与 pnpm
本身是 node 脚本，依赖的生命周期钩子也会直接调 `node`，若 PATH 上先出现另一个
node，安装就会在 osdk 没有选定的运行时下进行——而且一切看起来正常。

程序查找先搜 installer 自己的目录，再搜全部。后一步是必需的：npm 随 node 发布，
住在 node 的 bin 目录里而不是自己的。

## 记录只发生在成功之后

`record` 在命令成功返回后才调用，顺序是承重的。把它挪到执行前不会让当次运行变得
正常——当次照样响亮地失败——但**下一次**会对着一个从未被填充的目录报告「已是最新」，
于是一次可见的失败变成一棵静默损坏的工作树。

`osdk.lock` 的 `native_lock` 段记的是**安装后**磁盘上的那个文件的摘要，
所以它描述的是实际被消费的东西，而不是原本打算消费的东西。第一次安装时
lockfile 还不存在，该段因此缺席；lockfile 生成后的下一次运行才补上，
同时命令也从 `npm install` 转成 `npm ci`。

## 发现是 fail-closed 的

`discover()` 从当前目录向上逐层找，最近者胜，**不向下递归**——
盲扫子目录会在 monorepo 里把无关的包全都卷进来。

manifest 存在但不是常规文件、或解析失败，一律报错而不是跳过。
静默走过一个坏 `package.json` 的结果是「装了错的项目或什么都没装，
却报告成功」。

`select_installer()` 的优先级与 `npm_tools.rs` 既有的
`select_automatic_installer` 保持一致：声明 → 现存 lock → 项目配置 → 默认。
两处矛盾（声明与 lock 归属不符、同生态双 lock）都报出具体文件名后拒绝执行。

## trust 分三档

遵循 `trust.rs` 自己写下的哲学：一条依赖声明不会执行发布者没随包发出的东西，
所以声明本身不该进门禁——「每加一个包都要重新批准却什么都没教会用户」。

| 情形 | 是否需要批准 | 理由 |
| --- | --- | --- |
| `[deps.pnpm]`、`sources`、`outputs`、`auto`、`dir`、`depends`、`installer` | 不需要 | 默认禁脚本，只是声明装什么 |
| `index` / `extra_index` / `registry` / `insecure` | 需要，`WeakensVerification` | 改了字节来源，**不是**因为执行代码 |
| `env` 里形如 `*_REGISTRY` / `*REGISTRY_SERVER` / `*_INDEX_URL` / `*_DEFAULT_INDEX` / 含 `:REGISTRY` 的变量 | 需要，`WeakensVerification` | 与上一行等效，不能漏 |
| `allow_build_from_source` | 需要，`ExecutesCode` | 真的在本机执行任意代码 |
| 自定义 provider 的 `run` | 需要，`ExecutesCode` | 任意命令，且 `auto` 会自动触发 |

第三行是实现过程中补上的一个**真实漏洞**：第一版只查 provider 的直接 key，
于是 `index` 要批准、而 `env = { NPM_CONFIG_REGISTRY = "…" }` 达到同样的重定向
却完全免审。一个写法要批准、另一个绕过去，等于没有门禁。

判据用**后缀**而不是前缀。前缀匹配过宽的代价被记在仓库 AGENTS.md 里
（`UV_INDEX_` 会把 `index_strategy`、`NPM_CONFIG_FUND` 一类无关设置扫进来），
而且这个方向的损害是**不可见**的——多一次无谓的批准提示，或者某个功能被静默关掉。
所以两个方向都有测试：该命中的命中，不该命中的不命中。

`deps` 的 trust 要求**不影响工具分派**。`affects_tool_dispatch` 里
`deps` 与 `syspkg`、`task_config`、`models` 同列返回 `false`。
否则一个只是声明了 provider 的项目会连 `cargo --version` 都跑不了——
`[syspkg]` 踩过的正是这个坑。

## 新鲜度

沿用 `tasks::freshness` 的形状：输入哈希写进
`<cache>/deps/<blake3前16位>.toml`，不写进项目。

哈希包含 `sources` 的内容**和实际命令串（含环境变量）**。
环境变量必须算进去，因为 yarn berry 靠 `YARN_ENABLE_SCRIPTS` 禁脚本——
不算的话，禁脚本与不禁脚本这两次材料完全不同的运行会哈希成同一个。

未声明 `sources`、或声明后匹配 0 个文件，一律不算 fresh。
`tasks/freshness.rs` 里记过这个空洞成立的陷阱：一个匹配不到任何文件的判据恒为真，
会把「什么都没检查」伪装成「没有变化」。

## lock 记录什么

`[deps.<provider>]` 段记的是「另一台机器复现同一次安装需要什么」：
installer 及其版本、驱动它的 runtime、manifest 路径与 sha256、
原生 lockfile 的 kind/path/sha256、实际命令、registry。

刻意**不记**的：绝对路径（那是某台机器的位置）、`node_modules` 的内容
（那是原生 lockfile 的职责，osdk 不维护第二份依赖图）、凭据。

写进 lock 的相对路径归一成 `/`，因为这个值会被提交、并在别的平台上读取。
读取侧用 `split(['/', '\\'])` 同时接受两种分隔符。

该段带 `skip_serializing_if`，空时完全不落盘，所以既有 lock 的字节不变、
旧二进制也不会因为多出一段而失败。读回路径有测试守着——
AGENTS.md 记过 pypi 曾经「只写不读」，导致重锁时静默覆盖 lock 里记录的承诺。

## 实测版本

上面所有关于外部工具行为的断言，来自 2026-09-22 在 Windows x64 上的实测：
node v22.23.2、npm 10.9.8、pnpm 9.15.1 与 12.5.1、yarn 1.22.19 与 4.6.0
（经 corepack 0.34.6）、bun 1.4.2。

判定脚本是否真的执行，用的是「脚本往标记文件追加一行、然后读标记文件内容」，
不看退出码——退出码为 0 不能说明脚本没跑。
