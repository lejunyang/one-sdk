# 应用依赖

`osdk install` 装的是**工具**：把 Node、pnpm、Python 放进 osdk 自己的隔离目录。
`osdk deps` 装的是**项目自己的依赖清单**：读 `package.json`，驱动项目自己的包管理器，
把整份依赖闭包装进项目。

两者职责不重叠：

| 你想做的事 | 用哪个 |
| --- | --- |
| 准备 pnpm 本身 | `osdk install pnpm` 或 `[tools]` |
| 把整份 `package.json` 兑现成 `node_modules` | `osdk deps` |
| 增加或删除单个依赖 | `osdk install npm:<包名>` |

## 启用

`deps` 不会因为目录里有 `package.json` 就自动动手——那属于「隐式的大副作用」，
和 `osdk install` 不会顺手去拉模型是同一个判断。你需要显式声明一个 provider：

```toml
# osdk.toml
[deps.pnpm]
```

一行就够了。声明之后有两种用法。

**显式调用：**

```bash
osdk deps --list            # 探测到的 provider 与新鲜度
osdk deps --dry-run         # 打印将执行的命令，不执行
osdk deps                   # 兑现整份清单
osdk deps --explain         # 附带说明每个新鲜度判定的理由
```

**自动兑现（默认开启）：** 裸跑 `osdk install`、`osdk run <任务>`、`osdk exec`
之前，osdk 会先看声明的依赖是否还新鲜，过期才兑现。所以 clone 一个项目之后
直接 `osdk run dev`，依赖已经就位。

自动检查本身很便宜：它只比对清单哈希，**不扫描已装文件**。20KiB 的 lock 约
0.16ms，2MiB 的巨型 monorepo lock 约 2ms——命中时除此之外不做任何事，不会启动
包管理器。深度校验是另一回事，只在你显式要求 `osdk deps --verify` 时才做。

三种情况不会自动触发：

```bash
osdk install node@22        # 带具体工具时只装那个工具，不碰项目依赖
osdk run build --no-deps    # 单次跳过，install / exec 同样支持
osdk run build --dry-run    # --dry-run 本就不该有副作用
```

要对某个 provider 永久关闭：

```toml
[deps.pnpm]
auto = false                # 只影响自动触发；显式 `osdk deps` 照旧兑现它
```

没有 `[deps]` 段时，`osdk deps` 只告诉你它找到了什么：

```
no `[deps]` section; found manifests osdk could manage:
  /path/to/project/package.json
    candidates: bun, npm, pnpm, yarn

enable one in osdk.toml, for example:
  [deps.bun]
```

## 支持的 provider

| provider | 生态 | 清单 | 原生 lock |
| --- | --- | --- | --- |
| `npm` / `pnpm` / `yarn` / `bun` | Node | `package.json` | 各自的 lockfile |
| `uv` | Python | `pyproject.toml` | `uv.lock` |
| `pip-requirements` | Python | `requirements.txt` | 无（普通 requirements 不是完整依赖 lock） |
| `go` | Go | `go.mod` | `go.sum` |
| `cargo` | Rust | `Cargo.toml` | `Cargo.lock` |
| `deno` | Deno | `deno.json` / `deno.jsonc` | `deno.lock` |

### go / cargo / deno 的两点不同

**它们取依赖时不执行依赖的代码。** 实测 `cargo fetch` 不创建 `target/`（说明
`build.rs` 没跑）、`go mod download` 在项目里不留任何产物、`deno install` 不建
`node_modules`。所以这三个 provider 不需要也没有 `--ignore-scripts` 之类的开关——
构建脚本要到真正 `cargo build` 时才是问题。

**冻结模式都是真的，而且 cargo 最严。** `cargo fetch --locked` 在缺 lock 与
**lock 过期**时都会失败；相比之下 `uv sync --frozen` 只保证不改 lock，过期时会按旧
lock 静默装。所以「`--locked` 一词在各生态含义相同」是不成立的假设。

::: tip go 的工具链被钉住
`GOTOOLCHAIN` 默认是 `auto`——`go.mod` 要求更新的 Go 时，go 会**自己下载**另一个
工具链（实测会打印 `go: downloading go1.99.0`）。那样跑起来的就不是 osdk 选定并校验过
的 Go 了，所以 osdk 显式传 `GOTOOLCHAIN=local`。要用更新的 Go，请用
`osdk install go@<版本>`。
:::

### 暂不支持：bundler / composer

osdk 目前没有 ruby / php 的工具后端，也就装不了 bundler / composer 本身。把这两个
provider 列进来，结果会是「声明了、探测到了、然后在装工具那一步失败」——比明确说不支持
更糟。要支持需要先给 osdk 加对应的语言后端。

### Python 的两点差异

**`uv` 用 `--locked` 而不只是 `--frozen`。** uv 的 `--frozen` 只保证「不更新
lock」，**不**校验 lock 与 `pyproject.toml` 是否一致——实测：lock 过期时它 exit=0
并按旧 lock 装，新加的依赖根本没装上。要「lock 必须当令」得用 `--locked`，所以 osdk
两个都传。这与 `npm ci` 不同（后者在不一致时会失败）。

**`requirements.txt` 是解析输入，不是完整依赖 lock。** 即使每一行都写成 `==`，
它通常仍只列顶层包；`uv pip sync` 会把文件当成环境的完整集合，只安装显式条目并卸载
未列出的传递依赖。osdk 因此使用 `uv pip install -r requirements.txt`：解析并安装完整
传递闭包，但不清理文件之外的包，也不允许 `--frozen` 把顶层版本钉死误报成完整锁。
需要严格可复现的 Python 环境时，改用 `[deps.uv]` 和 `uv.lock`。

依赖装进项目自己的 `.venv`，与「应用依赖在项目内、工具在隔离目录」的分层一致。
osdk 还会传 `UV_PYTHON_DOWNLOADS=never`，确保用的是 osdk 选定的解释器，
而不是 uv 自己悄悄下载的另一个。

## installer 如何选定

优先级从高到低，越靠上的越不会被下层覆盖：

1. `package.json` 的 `packageManager` 字段——项目自己的声明最权威。
2. 目录里现存的原生 lockfile——谁写的谁继续管。
3. `[deps.<provider>].installer`。
4. provider 自身。

两处矛盾会被拒绝而不是猜测：声明了 `pnpm` 却只有 `yarn.lock`，或同一目录里
同时存在两个生态相同的 lockfile。二者都会报出具体是哪两个，让你自己决定留哪个。

清单本身无法解析时同样报错而不跳过——静默走过一个坏 `package.json`，
结果会是装了错的项目或什么都没装，却报告成功。

## 冻结安装

osdk 自己检查原生 lockfile 在不在，而不是传一个 flag 就当作已经冻结：

- 有 lockfile → 冻结安装（`npm ci`、`pnpm --frozen-lockfile`、
  yarn berry `--immutable`、yarn classic 与 bun `--frozen-lockfile`）。
- 没有 lockfile → 退回普通安装，并打印一条 `warning:` 说明将会创建 lockfile。

**为什么不能交给包管理器判断**：`yarn@1.22.19` 和 `bun` 在缺少 lockfile 时
退出码为 0、照常安装，`yarn@1` 甚至会静默接受 berry 的 `--immutable` 然后既不冻结
也不禁脚本。只传参数而不自己检查，会在四款里有两款上静默失效。

CI 里要求必须有 lockfile：

```bash
osdk deps --frozen
```

这会把上面那个退回变成错误，而不是一条容易被忽略的警告。

## monorepo：显式声明子项目

osdk **不会**向下扫描子目录找项目。要管理一个 monorepo 的多个包，在 `[deps]` 里
显式声明它们所在的位置：

```toml
[deps]
roots = ["apps/*", "packages/*"]

[deps.npm]
```

只有匹配到的目录会被检查。**没被 `roots` 覆盖的包不会被发现**——即使它有一份完好的
清单、就躺在旁边。可被自动处理的集合必须是你声明过的集合，否则 `osdk deps` 会给一个
你根本没提到的包装依赖。

每个子项目有自己的地址 `//<路径>:<provider>`。列举时**默认只看当前这一层**，
`--all` 才展开所有子项目：

```
$ osdk deps --list
npm                 stale  /repo

$ osdk deps --list --all --explain
npm                 stale  /repo
//apps/api:npm      stale  /repo/apps/api
    from root: apps/*
//apps/web:npm      stale  /repo/apps/web
    from root: apps/*
//packages/ui:npm   stale  /repo/packages/ui
    from root: packages/*
```

分层只针对**列举**。几十个包的仓库全量列出来会把有用的部分顶出屏幕，而这是个可读性
问题、不是正确性问题。**无操作数的** `osdk deps` 仍覆盖所有声明的 root，避免批量兑现时
悄悄漏掉工作；但一旦给出 provider 名，它只作用于当前工作目录最近的项目 root。

按地址处理单个位置不需要 `--all`。空路径 `//:` 显式表示配置根：

```bash
osdk deps npm                     # 当前目录最近的 npm 项目
osdk deps //:npm                  # 只处理配置根
osdk deps //apps/api:npm          # 只处理这个子项目
osdk deps --skip //apps/web:npm   # 全量兑现时排除一个
osdk deps --list //apps/api:npm   # 只看它，无需 --all
```

### 按位置筛选：`--filter`

裸 provider 名选最近 root；`--filter` 显式扩大到匹配的子项目位置：

```bash
osdk deps --filter 'apps/*'          # apps/ 下所有子项目，不论用哪个包管理器
osdk deps --filter apps/api          # 就这一个
osdk deps --filter 'apps/*' --filter 'packages/ui'   # 可重复，取并集
osdk deps --list --filter 'apps/*'   # 筛选本身就意味着要展开，无需 --all
```

pattern 与 `roots` 是**同一套**写法（下一节），不是第二种方言：一个 `*` 不跨 `/`，
`**` 不支持。同一份配置里两套通配语法是纯粹的认知成本——mise 声明用 `*`、寻址用
`...`，pnpm 至今还留着一个 `legacyDirFiltering` 开关，就是因为在这件事上改过主意。

**匹配不到任何子项目是错误，不是安静地成功**：

```
$ osdk deps --filter 'services/*'
error: `--filter` matched no sub-project: services/*
```

这一点与 pnpm 相反（它的 `failIfNoMatch` 默认关）。理由和 `--verify` 拒绝把「没检查
过的环境」报成干净一样：**「什么都没做」不能读起来像「做完了，没问题」**。CI 里一个
筛到已改名目录的步骤应当失败，而不是什么都没构建却通过。

### 模式的匹配规则

- 支持 `*`（任意字符）与 `?`（单个字符），**逐段匹配**。
- **不支持 `**`**：那等于把声明的 root 变回任意子树遍历，正是这个功能要避免的事。
  需要更深的层级就把它写出来，例如 `apps/*/*`。
- 一个 `*` 永不跨越 `/`，所以 `apps/*` 不会匹配到 `apps/group/nested`。
- `..` 会被拒绝：一份被提交的配置不该能伸到项目外面。
- 匹配不到任何目录不算错误——`apps/*` 在还没有 `apps` 的仓库里是一句前瞻性的声明。

root 里的子项目与顶层项目遵循同一套规则：清单坏了就**报错**，不会跳过。少装一个包却
报告成功，是最难被发现的那种失败。

## 自定义 provider

除了内置的包管理器，`[deps]` 里任何别的名字都是一个自定义步骤：一条你自己的命令，
带上它的输入与产物。形状与 `[tasks]` 同构。

```toml
[deps.codegen]
sources = ["schema/schema.graphql"]   # 变了才重跑
outputs = ["src/generated"]           # 缺了就算过期
run = "pnpm run codegen"
depends = ["pnpm"]                    # 等 pnpm 先把依赖装好
dir = "apps/api"                      # 可选：在子目录里跑
env = { NODE_ENV = "development" }
```

自定义 provider 不需要清单文件——**声明本身就是发现**。它的根是声明它的那份
`osdk.toml` 所在目录，所以 `sources` 与 `outputs` 的相对起点和内置 provider 一致。

`run` **不经过 shell**：命令按空白切分后直接执行。否则同一条 `run` 在不同机器上含义
会不同，而这个字符串是要提交进仓库的。需要管道、重定向一类的写法，请放进一个脚本里
再由 `run` 调用它。

`depends` 决定顺序，`osdk` 会据此排序（声明顺序无关）。构成环时直接报错——随便挑一个
顺序会让某一步在它的输入还不存在时就运行，而报错会指向错误的那个 provider。

::: warning 自定义 provider 一律需要批准
`run` 是一条任意命令，所以它**总是**需要你批准配置——这与内置 provider 不同（声明
装什么不需要）。这也与 `[tasks]` 不同：task 是你显式 `osdk run <名字>` 触发的，
那次调用本身就是授权；而 deps 可以由 `auto` 前置触发。
:::

## 包管理器没装怎么办

`deps` 会自己把它装上，走的是 osdk 平常那条工具安装链——所以来源选择、校验、
CAS 都和 `osdk install` 完全一致，不存在第二套安装逻辑。

工具装进 osdk 的隔离目录，**不会**进你的项目。`deps` 往项目里放的是依赖，
而装依赖的那个包管理器属于工具。

```bash
$ osdk deps
installing node for deps provider `npm`
installing node@26.10.0 ...installed node@26.10.0
npm.cmd install --ignore-scripts
added 2 packages in 827ms
```

CI 里通常希望工具来自一次显式的 `osdk install`，以免某次运行悄悄取到别的版本。
用 `--no-install-tools` 把「自动获取」关掉——它只禁止**获取**，你自己装好的照常可用：

```bash
osdk deps --no-install-tools
```

工具缺失时它会直接失败，并告诉你该跑哪条命令补上。

## 构建脚本默认关闭

依赖的 `preinstall` / `install` / `postinstall` 默认不执行。仅声明装哪些包不会
执行发布者没随包发出的东西，所以声明本身不需要你批准配置。

要打开就需要批准了，因为这时才真的会在本机执行任意代码：

```toml
[deps.pnpm]
allow_build_from_source = true
```

## 换 registry

```toml
[deps.pnpm]
index = "https://registry.example.com/"
```

改变字节来源需要你批准配置（原因是来源变了，不是因为执行了代码）。
通过 `env` 设 `NPM_CONFIG_REGISTRY` 之类的变量达到同样效果时，同样需要批准——
否则一个写法要批准、另一个绕过去，等于没有门禁。

## 完整配置

```toml
[deps]
disable = ["npm"]           # 即使上层启用了也在此关闭

[deps.pnpm]
auto = true                 # 默认值：允许在 install / run / exec 前自动兑现
sources = ["package.json"]  # 参与新鲜度判定的文件（替换默认值）
outputs = ["node_modules"]  # 缺失即视为过期（替换默认值）
dir = "apps/api"            # 在子目录里运行
depends = ["npm"]           # 先兑现另一个 provider
timeout = "10m"
installer = "pnpm"
index = "https://registry.npmjs.org/"
allow_build_from_source = false
env = { CI = "1" }
```

## 校验已装环境：`osdk deps --verify`

新鲜度回答的是「输入变了吗」，它**看不见**别的工具动过 `node_modules` 或
`site-packages`——哈希是对输入算的。`--verify` 读包管理器**自己写的收据**来回答
「装的东西还是当初那些吗」：

```bash
osdk deps --verify
```

两层，便宜的先跑：

- **L1**：原生 lockfile 的 sha256 是否仍等于 `osdk.lock` 记录的值。能抓到「lock 没动
  所以哈希也没变，但环境被别的工具重建过」这种漂移。
- **L2**：收据里每一条是否还在、尺寸与摘要是否还对。收据按包管理器而定：

  | 包管理器 | 读的收据 | 粒度 |
  | --- | --- | --- |
  | pip / uv | `dist-info/RECORD` | 逐文件 size + sha256 |
  | npm | `node_modules/.package-lock.json` | 逐包 version + integrity |
  | pnpm | store 的 `*-index.json` | 逐文件 size + sha512 |
  | yarn | 不支持，如实报告 | — |

  pnpm 的收据比 npm 细：它是**逐文件**的，与 Python 同级。pnpm 不写
  `.package-lock.json`，`.modules.yaml` 里也只有布局信息；逐文件摘要在内容寻址
  store 的索引里，而索引地址正好能从 `pnpm-lock.yaml` 记的 integrity 算出来。
  所以 pnpm 项目要校验需要两样都在：项目里的 lockfile，和本机上的 store。从别的
  机器 clone 过来、或 store 被 prune 过时，`--verify` 会说「没有收据可读」，而不是
  报一切正常。

  yarn 明确不做：Berry 的 PnP 把依赖放在单个 zip 支撑的存储里，没有可逐包比对的
  目录树。与其加一个不管环境怎样都会通过的判据，不如如实说不支持。

发现问题时退出码非 0，可以直接当 CI 门禁。输出会同时报「检查了多少条」——
「0 个问题」和「什么都没检查」不该读起来一样，所以没有收据可读时它**报错而不是通过**。

实测能抓到的篡改：删掉包内一个文件、改一个文件的内容、删掉整个已装包、就地把
某个包的 `version` 换掉。倒数第二种最隐蔽——目录在、文件数对，只有版本不符。

pnpm 上还多抓到一种 npm 收据抓不到的：**改动后文件长度不变**。因为逐文件摘要在，
翻转一个字节也会被报出来；而只到包粒度的收据对此毫无反应。

::: warning 发现篡改后不要只是重跑安装
实测 `uv pip sync` **不会**修复被改过的文件，而且被篡改的内容可能已经进了工具的
全局缓存（一个被改过的文件曾让之后每个新建的 venv 都从坏缓存复制）。正确做法是
清缓存后重装。
:::

## 新鲜度

`deps` 记录上一次成功运行的输入哈希，下次比对。哈希包含 `sources` 的内容
**和实际执行的命令**（含环境变量——yarn berry 靠 `YARN_ENABLE_SCRIPTS` 禁脚本，
不算进去会把两次材料不同的运行判成同一次）。

状态写在 osdk 的缓存目录里，不写进你的项目。`--force` 可以跳过判定强制运行。

未声明 `sources`、或声明后一个文件都没匹配上，一律不算「新鲜」——
一个匹配不到任何文件的判据恒为真，那会把「什么都没检查」伪装成「没有变化」。
