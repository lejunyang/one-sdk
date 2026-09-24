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

## 列举分层，兑现不分层

`--list` 默认只列当前 config root，`--all` 才展开 `[deps].roots`。这个不对称是刻意的，
而且方向不能反过来。

列举是**可读性**问题：几十个包的仓库全量列出来，会把「我这一层现在是什么状态」顶出
屏幕。mise 的 `mise tasks` 与 `mise tasks --all` 是同一取舍（前者只列当前 config_root
及其父级），可以互为印证。

兑现是**正确性**问题：裸 `osdk deps` 从第一天起就覆盖所有声明的 root。默认少做一部分，
表现是「命令成功了但某个子项目的依赖没装」——没有报错、没有警告，只有之后某次运行在一
个不完整的环境里失败。这正是这份代码库反复拒绝的那类失败：比报错更糟，因为它看起来像
成功。

所以 `wants_rooted` 的默认答案是**展开**，只有「纯 `--list`」这一种情况才不展开。两个
例外仍然展开：

- `--list --all`，这就是这个 flag 的用途。
- 带 rooted 操作数（`//apps/api:uv`）的列举。点名要一个子项目却被告知它不存在，是对
  配置的误报，而不是一次更简洁的输出。

对应的测试**两个方向都注入过**：把 `wants_rooted` 改成恒真，「纯 `--list` 不得展开」
那条断言红；改成列举时恒假，`--all` 与 rooted 操作数两条断言红。只测一个方向的话，
一个「永远不展开」的实现也能让前者通过。

测试里还有一处值得记：`--dry-run` 的输出打的是**目录路径**而不是 rooted id，所以证明
「兑现覆盖了子项目」要匹配路径、并数 `would run in` 的出现次数。第一版断言匹配的是
`//apps/api:npm`，失败原因与被测性质无关——输出格式，不是覆盖范围。

分层带来一个如实记下的副作用：**纯 `--list` 不再展开 roots，所以子项目里一份坏清单在
纯列举下不会被发现**。fail-closed 本身没有变松——兑现、`--list --all`、以及点名某个
rooted id 时，坏清单一律报错。但「列举一下看看」不再顺带是一次全仓库校验；要那个效果
就写 `--all`。接线时这条正是由一个既有测试抓出来的：
`a_broken_manifest_inside_a_root_fails_closed` 靠 `--list` 才能碰到坏的兄弟目录，
分层后它必须显式要求展开。

## monorepo roots：逐段匹配，而不是「扫完再过滤」

`discover_in_roots` 按 pattern 的段逐层下降：遇到字面段就 `join`，遇到通配段才
`read_dir` **那一层**。这与「先遍历子树再用 pattern 过滤」在结果上可能一样，在性质上
完全不同——后者必须读遍每个目录才能决定，而且少一个过滤条件就静默变成全仓库爬取。
逐段下降让「只看声明过的地方」成为结构性事实，而不是依赖某个过滤分支永远正确。

不支持 `**`。它会把声明的 root 还原成任意子树遍历，恰恰是这个功能存在的理由，所以是
**不支持**而非「支持但受限」。要更深的层级就显式写出来（`apps/*/*`）。

`glob_matches` 拒绝任何含分隔符的名字。当前所有调用方都只传单个目录名，所以这条今天
不改变任何行为——但一个**能**跨分隔符的匹配器意味着下一个传多段字符串的调用方会静默
得到子树爬取。这个保证属于函数本身，不该依赖每个调用方都记得。

### 一条「注入不变红」的排查，以及它改进了什么

核心不变量的注入验证第一次**没红**。排查后发现有两层原因，都值得记下来：

1. 我最初的注入是「把字面段也走通配分支」。这在语义上**不构成缺陷**——
   `glob_matches("apps", name)` 仍然只接受 `apps`，改的只是「多读一次目录去确认
   `join` 已经知道的事」。
2. 真正承重的分支是 `glob_matches` 那次调用。但把它换成 `true` 也没红，因为当时所有
   fixture 用的都是 `apps/*`，其唯一通配段是裸 `*`——它**本来就该**匹配每个目录。
   于是「接受任意名字」与「正确匹配」产出完全相同的结果，**过滤器从未被真正测到**。

补了一条用 `api-*` 的测试（`api-v1`/`api-v2` 该中，紧邻的 `web-v1` 该不中），
把 `glob_matches` 换成 `true` 后它确实变红。这是 AGENTS.md 「探针必须落在被测机制
之内」的一个具体形态：**一个只用了裸 `*` 的 fixture，测不出通配过滤器。**

## 自定义 provider：三个字段改 Cow，而不是整体改 String

自定义 provider 的名字来自配置，是运行时的 `String`；而内置 provider 表是静态的
`&'static str`。全面改成 `String` 会让每个内置条目都要分配，且 diff 波及 40 余处。

实际只有三个字段真的承载「provider 名字」：`DetectedProject::provider`、
`InstallerChoice::provider`、`RunPlan::tool`。这三处改成
`Cow<'static, str>`——内置路径仍是 `Borrowed`、不分配，自定义路径是 `Owned`。
静态 schema 表完全不动。

`Resolved::schema` 相应变成 `Option`。这不是为了绕开类型，而是**没有内置 schema
恰恰就是「自定义」的定义**，用 `Option` 表达比用一个哨兵值或报错更贴近事实。
自定义 provider 的 `sources`/`outputs` 因此直接取配置里声明的那些——它没有默认值可
回落。声明为空则意味着「无法确立新鲜度」，于是每次都跑；而不是当成「永远新鲜」，
那正是空洞成立的陷阱。

自定义 provider 的 `outputs` 一律按 **required** 处理：用户把它们写下来就是宣称这一步
会产出它们，缺了就说明这一步没有兑现承诺。内置 provider 保留自己那套
required / optional-once-seen 的混合。

## `run` 不经过 shell

命令按空白切分后直接执行。交给 `cmd.exe` 或 `sh` 会让同一条 `run` 在不同机器上含义
不同，而这个字符串是要提交的；引号规则也会变成一个可移植性陷阱。需要 shell 特性的，
放进脚本再由 `run` 调用。

程序解析上自定义 provider 走另一条路：它不带自己的工具，所以没有 install 目录可搜，
直接把名字交给操作系统按 PATH 查找——而子进程拿到的 PATH 是「先解析出的工具，再继承
的那份」，所以 `depends` 装好的东西能被找到。

## depends 决定顺序，环是错误

`order_by_depends` 做拓扑排序。环报错而不是随便挑一个顺序：挑了就会让某一步在它的
输入还不存在时运行，而失败信息会指向错误的 provider。指向未配置的 provider 的依赖被
忽略——那是空操作而非矛盾，一个被禁用的 provider 没有什么可等的。

## 一处「注入不变红」反而揭示了代码问题

`plan_custom` 最初有两道检查：先 `command.is_empty()`，再 `parts.next()` 返回 `None`
时报错。给任一道注入缺陷都**看不出变化**，因为另一道仍会拒绝。

这不是测试写得不好，而是**一道移除后不可见的防线不能被信任它还在**。已收敛为单一判据，
并验证注入后确实变红。

同类地，`plan` 的生态分派去掉了兜底分支：现在每个生态都有实现，新增一个应当在**编译期**
失败，而不是在运行时打印一句「这个生态还没实现」。

## go / cargo / deno：为什么没有「禁脚本」开关

Node 与 Python 都需要显式关掉构建脚本，这三个不需要——不是省略，而是**取依赖阶段
没有这个口子**。实测（go 1.27.1 / cargo 1.98.0 / deno 2.9.6）：

- `cargo fetch` 之后 `target/` **不存在**，说明 `build.rs` 根本没跑；构建脚本是
  `cargo build` 的事。
- `go mod download` 在项目里不留任何产物。
- `deno install` 不创建 `node_modules`。

所以为了「与 npm 对称」给它们加一个 `--ignore-scripts` 一类的参数，结果只会是被工具
拒绝、或者静默什么都不做——两者都比不加更糟。`no_script_suppressing_flag_is_invented`
这条测试专门守着这件事。

三者的冻结模式都是**真的**，这与 Node 那轮形成对照（yarn classic 与 bun 在缺 lock 时
exit=0 照常安装）。而且 **cargo 的 `--locked` 比本子系统里任何别的冻结都严**：lock
仅仅过期它也 exit=101，而 uv 的 `--frozen` 会按旧 lock 静默装完。这说明「`--locked`
在各生态语义一致」是个错误假设，每个生态都必须单独核准。

### GOTOOLCHAIN 与 UV_PYTHON_DOWNLOADS 是同一类问题

`GOTOOLCHAIN` 默认 `auto`：`go.mod` 要求更新的 Go 时，go 会主动
`go: downloading go1.99.0` 去换工具链（实测行为，本例因该版本不存在而失败，但**尝试
下载这件事已经发生**）。那就意味着实际跑的 Go 不是 osdk 选定、也不是 osdk 校验过的
那个——与 uv 自行下载解释器完全同构。两处都显式钉住：`GOTOOLCHAIN=local`、
`UV_PYTHON_DOWNLOADS=never`。

`CARGO_HOME` 无需新增约定：`backend/rust.rs:39-40` 已经把它指向 `<data>/cargo`，
deps 沿用即可，也因此不会与用户自己的 `~/.cargo` 混在一起。

### 一条被淘汰的测试

最初写了一条「`go.mod` 不会被当作 Node 清单解析」。它**永远不会失败**——
`node::declared_manager` 开头就有 `ecosystem != Node` 的早退，所以即使分派错了也无害。
这正是「探针落在被测机制之外」：注入 catch-all 分派后它仍然全绿。

换成了「这些清单确实被校验、坏清单不会被跳过」，并验证了它在注入「不校验」时会红。
不过分派本身还是改成了**穷尽 match、去掉 `_` 分支**：编译器强制补一条 arm，比将来从
用户那里发现漏接要便宜。

### 暂不支持 bundler / composer 是硬约束，不是取舍

`backend/registry.rs:21-36` 里没有 ruby、没有 php，所以 D2 那条「缺包管理器就自动装」
对这两个生态**走不通**。把无法兑现前置条件的 provider 列进表，用户会得到「声明了、
探测到了、装工具那步失败」——比明确不支持更糟。要支持得先加语言后端，那是另一件事。

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
15 行里 14 行带 size 与 sha256）、npm 的 `node_modules/.package-lock.json`
（逐包 version 与 integrity）、pnpm 的 store 索引（逐文件 size 与 sha512，见下）。
osdk 读它们而不另造第二份依赖图——与「原生 lock 才是真相」是同一个判断。

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

### pnpm：收据在 store 里，地址从 lock 算出来

pnpm 的形状**没有一处像 npm**，所以它的读法是实测出来的而不是照 npm 推的
（pnpm 9.15.1）：

- **没有** `node_modules/.package-lock.json`。
- `node_modules/.modules.yaml` 存在，但只有 `storeDir` / `virtualStoreDir` /
  `nodeLinker` 这类布局信息，**不含任何逐包数据**——它是定位器，不是收据。
- `node_modules/<pkg>` 是 junction（Windows）或 symlink，指向
  `node_modules/.pnpm/<name>@<version>/node_modules/<name>`，其中的文件再硬链接进
  内容寻址 store。
- `pnpm-lock.yaml` 记的 integrity 是**tarball** 的摘要，无法用解包后的目录重算。

差点由此得出「pnpm 没有可离线核对的收据」。真正的收据在 store 里：
`files/<xx>/<...>-index.json`，带**逐文件** `integrity`(sha512) + `size` + `mode`。
实测 ms@2.1.3 的 4 个文件，每条 sha512 都与磁盘字节完全一致。

关键是索引**不需要搜索**：tarball integrity 的 base64 解出来转成十六进制，第一个
字节就是子目录名、其余是文件名主干。实测
`sha512-6Flzub...` 精确映射到 `files/e8/5973b9...-index.json`。这条映射单独有一个
测试钉住，用的就是实测出的那个真实路径——否则把前缀从 1 字节改成 2 字节这种改动
不会被任何测试发现。

由此得到一个必须如实说明的限制：**校验链需要两半**——项目里的 lockfile 提供地址，
本机的 store 提供摘要。从别的机器 clone、或 store 被 prune 过时，只能报
`ReceiptMissing`。

反过来，pnpm 的收据比 npm 的**更强**：npm 只到包粒度，所以「改动后长度不变」这类
篡改它抓不到；pnpm 逐文件有摘要，翻转一个字节也会被报出来。这一条单独有测试，因为
若只测「改内容能被抓到」，尺寸检查就足以让它通过，摘要比较可以被删掉而无人察觉。

dispatch 顺序也是承重的：先看 `.pnpm` 目录再回落 npm。一次 npm 安装之后再用 pnpm
装，旧的 `.package-lock.json` 会留在原地；读它就是在校验一份**已经不是当前安装**的
树，而且是静默地校验。

yarn 维持不支持：Berry 的 PnP 把依赖放进单个 zip 支撑的存储，没有可逐包比对的目录
树。加一个不管环境怎样都会通过的判据，比承认不支持更糟——这一节开头淘汰
`pyvenv.cfg` 判据用的就是同一条理由。

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

**三、顶层 requirements 不能交给 `pip sync`。** 实测只有
`aiohttp==3.14.3` 的文件经 `uv pip sync` 后只装 aiohttp，不装 multidict/yarl；在一个
完整环境上重跑还会主动卸载这些传递依赖。`==` 只能固定顶层版本，不能证明文件含完整闭包。
因此 `pip-requirements` 使用 `uv pip install -r`，以保留解析传递依赖的语义；代价是它不再
清理额外包，也永远不声称 frozen。严格复现走 `uv` provider + `uv.lock`。

## prelude：为什么需要一个「前置命令」概念

`uv sync` 会自建项目环境，而 `uv pip install` 在没有目标环境时不能把依赖装进
项目自己的 `.venv`。这个不对称如果藏进 runner 里隐式补一句
`uv venv`，那么 `--dry-run` 看不到它、freshness 哈希也不包含它。

所以 `RunPlan` 有 `prelude: Vec<Vec<String>>`：同一个程序、同一个 cwd、同一份 env，
按序先跑。它出现在打印出来的命令串里，因此也进入哈希——否则「先建环境再 install」与
「往已有环境里 install」会被哈希成同一件事。

prelude 必须**幂等**：`uv venv` 在环境已存在时 exit=2（`Failed to create virtual
environment`），于是第一次之后每次都会在到达 install 之前失败。用
`--allow-existing` 复用环境，避免每次重建并丢掉缓存。

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

## auto 前置：只判新鲜度，绝不深扫

`auto` 默认 `true`，覆盖裸 `install` / `run` / `exec` 三个入口。这个默认值不是
随手选的，它依赖一个实测前提：新鲜度判定本身足够便宜到可以放在每条命令前面。

实测（.NET SHA256，blake3 更快，故为保守上界）：20KiB 的 lock 0.16ms，200KiB
0.39ms，2MiB 的巨型 monorepo lock 2.06ms。命中时除这次哈希比对外不做任何事——
不启动包管理器，不列目录，不读已装文件。

由此划出一条硬线：**auto 前置永远不跑 `--verify`**。深度校验要逐条读包管理器的
收据（Python 的 `dist-info/RECORD` 逐文件 size/sha256，Node 的
`node_modules/.package-lock.json` 逐包 integrity），量级是秒。把秒级扫描放在每次
`osdk run` 前面，结果不会是「更安全」，而是用户把整个机制关掉。所以
`materialize_auto` 传 `verify: false`，`--verify` 保持显式。

这条约束一开始没有测试守着。变异测试把 `verify: true` 注进 auto 路径，整个套件
**全绿**——特性里最贵的那个承诺没有任何东西拦着它。为此把选项集从
`materialize_auto` 里抽成 `auto_options()`：它从「藏在一次调用里的字面量」变成
「一个测试能读的值」，四条断言（不 verify、不 force、只覆盖 auto、允许装工具）
各自能被对应变异打红。

三个安全阀集中在 `wants_auto_deps` 一处，而不是散在三个 match 臂里：

| 情况 | 为什么不触发 |
| --- | --- |
| `osdk install node@22` | 带 operand 是「装这个工具」，顺手重写项目依赖树是没人要求的副作用；与「显式 operand 跳过 lock replay」同一条理由 |
| `--no-deps` | 单次逃生口，三个入口都有 |
| `run --dry-run` | 这个标志的全部意义就是没有副作用 |

`auto = false` 只关自动触发，不影响显式 `osdk deps`——点名调用这个命令本身就是
授权。反过来说，`auto` 也不是 trust 事项：触发时机变了，装的东西没变危险，所以
分档仍按「默认不需要信任、自定义 index 归 `WeakensVerification`、放开源码构建归
`ExecutesCode`」，自定义 provider 的 `run` 依旧一律 `ExecutesCode`。

`DepsProviderEntry` 的 `Default` 是手写的而不是 derive 的，因为 derive 会给
`auto: false`，而字段的 serde 默认是 `true`。两者不一致时，「程序构造出来的
entry」与「从配置解析出来的 entry」含义不同，而这种分歧要等到某个测试构造了
entry、并据此对真实配置得出错误结论时才会暴露。

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
