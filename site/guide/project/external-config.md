# 外置项目配置

默认情况下，osdk 的项目配置是写在项目目录里的 `osdk.toml`。外置项目配置把
项目的工具声明与锁文件存到 osdk 自己的配置目录下，只把一个目录「绑定」为项目，
不在项目目录里写任何东西。

适用场景：

- 只读检出的第三方仓库，或者不能随意改动文件的代码库；
- 想把多个项目的 osdk 配置集中放在自己的配置目录里统一管理；
- 临时工作副本、归档目录，不希望在里面留下 `osdk.toml` / `osdk.lock`。

## 命令一览

```text
osdk project add [PATH]       # 绑定目录；PATH 默认当前目录；--dry-run / --json
osdk project show [PATH]      # 显示该目录生效来源与绑定状态；PATH 默认当前目录；--json
osdk project list             # 列出全部外置绑定；--json 输出数组
osdk project relocate OLD NEW # 迁移绑定；OLD 与 NEW 必填；--dry-run / --json
osdk project remove PATH       # 删除外置绑定目录；PATH 必填；--dry-run / --json
```

```bash
osdk project add .                 # 绑定当前目录
osdk project add C:\repos\frontend # 绑定任意目录，按真实 canonical 路径计入
osdk project show .                # 这个目录实际走哪份配置
osdk project list                   # 全部绑定一览
osdk project list --json            # 数组输出，含完整状态字段
osdk project remove ~/tmp/old-checkout --dry-run
osdk project remove ~/tmp/old-checkout
```

## 绑定行为

- `add` 把用户给出的目录经 OS canonicalize 解析为真实绝对路径后绑定：
  相对路径、junction、8.3 短名、大小写别名都按解析后的真实路径计入；不会
  向上猜测 Git 根，不会安装任何工具，也不会改动该目录。
- 已绑定的目录再次 `add` 不会覆盖或改写已有绑定。
- 目录里已经存在本地 `osdk.toml` / `.osdk.toml` 时，仍然可以绑定；运行时以
  本地文件为准，osdk 会在 stderr 提示外置配置被本地文件遮蔽（shadowed）。

## 外置配置存放位置

每个绑定在配置目录下占一个子目录。`add` 成功时（非 dry-run）立即生成三件套：

```text
<OSDK_CONFIG_DIR>/projects/<root-id>/
  project.toml    # 锚点文件；root 记录真实项目根的绝对路径
  osdk.toml       # 外置项目的工具与任务声明（初始为占位注释，直接编辑即可）
  osdk.lock       # 外置项目的锁文件；初始内容即当前普通锁的 schema（与普通 writer 同一 schema_version 权威值）
```

- `root-id` 由真实项目根路径经 OS canonicalize 解析后的完整 BLAKE3 决定：
  junction、8.3 短名、大小写别名都归到同一身份；大小写按文件系统语义处理，
  osdk 不额外全小写，也不做 Unicode 规范化。它**不基于项目名，也不基于
  Git remote**；只改项目显示名称或 remote 不影响绑定。目录改名或移动会
  改变路径身份，需要使用 `project relocate`。
- 这里没有另一份可手工编辑的索引：`<config>/projects/` 目录与其中的
  `project.toml` 就是全部注册表。
- 用 `osdk project show .` 可以直接看到绑定对应的物理目录，再去编辑里面的
  `osdk.toml`。

## 发现与优先级

在某个目录下执行命令时，osdk 从当前目录逐祖先向上查找：

1. 最近的项目胜出——这里的「项目」既包括目录里带本地 `osdk.toml` 的项目，
   也包括已绑定的外置项目；
2. 同一个项目根上，本地 `osdk.toml` / `.osdk.toml` 优先于外置配置；
3. 更近的外置子项目可以胜过更远祖先的本地父项目。

## 配置与锁成套选择

一次命令只选定**一份**生效来源：外置项目的 `osdk.toml` 与 `osdk.lock` 永远
成套使用，本地项目同理。

- 配置目录里的锁缺失（例如被人为删除）时，按当前选中的来源在同一份目录内
  重新生成，绝不会跨来源借用另一份的锁；
- 选中的配置或锁损坏时直接报错，不会静默回退到另一份来源；
- 被本地文件遮蔽（shadowed）的外置配置，正文不会被读取；
- 当存在多份候选时，osdk 在 stderr 警告里说清本次实际采用了哪份配置与锁、
  忽略了哪一份。

现有的读写命令——`current`、`where`、`exec`、shim、shell hook，以及
`use`、`config`、`source`——统一走这套来源解析，没有为外置项目新增参数；
`--global` 仍然表示「用户全局默认」，与项目绑定无关。

## 逻辑项目根与物理配置路径是分离的

外置绑定后，**逻辑项目根**仍是被绑定的那个目录：

- 任务、deps 与相对资源路径按逻辑项目根（或声明所在文件的位置）解析；
- `extends` 按逻辑项目根寻找 `.git` 仓库边界，越过边界的引用照旧报错；
  找不到 `.git` 时保留原有继承行为；
- 信任仍按真实配置文件审批，且仍是「项目级信任」，关联配置真实路径、
  所在项目根与受管内容；`relocate` 移动了物理路径，信任记录会随之失效，
  需要重新审批，而不是自动跟随。

外置配置本身可以位于 Git 仓库外；`extends` 指向的文件仍须位于逻辑项目的
仓库边界内。继承任务在各自声明目录执行，根目录任务名称以最上层继承配置
为基准，例如 `//apps/svc/worker:build`。

`use` 同时更新选中外置配置及其锁；若锁更新失败，恢复之前的配置，
不会在项目目录另建替代锁。

## 外置项目里的 use、run 与 deps

- 外置项目下执行 `osdk use npm:xxx` 走 osdk 隔离安装，**不会**改动项目里的
  `package.json`，也不会读写包管理器的原生 lock——因为这份配置本来就不在
  项目目录里。
- 但 `osdk run`、`osdk deps` 仍会按逻辑项目根正常写项目输出（构建产物、
  依赖目录等），因此外置配置**不承诺项目目录完全零写入**。

目标绑定目录中已有的未登记文件会保留并报告冲突。`add` 一次发布完整的
三件套；`relocate` 先准备并发布新绑定，再撤销旧登记，准备失败不会拆散
旧配置与锁。若进程在发布后中断，可能留下两个绑定；用 `project list`
检查并核实文件后，再移除过时绑定。

## 迁移与删除绑定

```bash
osdk project relocate OLD NEW      # 项目根路径变了（换机器、移动目录）时使用
osdk project remove PATH          # 不再需要这个外置绑定
```

- `relocate` 把绑定迁到新根路径，外置目录里的 `osdk.toml` 与 `osdk.lock`
  保留；旧根路径在磁盘上已不存在（missing）时不会被自动删除。
- `remove` 只删除外置绑定目录本身：**不删**项目文件，也**不删**共享安装池里的
  工具。先用 `--dry-run` 预览将删除的内容。

`add` 与 `relocate` 的新目标拒绝普通文件。`show` 支持相对路径，
从子目录向上查找；`root`、`binding_dir`、配置和锁均属于实际选中的项目。

## show 与 list 能看到什么

- `show [PATH]`（PATH 默认当前目录）打印 `project root`、`selected` 来源
  （`local` / `external binding` / `none`）、生效的 `config:` 与 `lock:`
  路径、绑定目录，以及状态行：`WARNING recorded root no longer exists`、
  `WARNING binding config missing`、`shadowed by a local osdk.toml/.osdk.toml`。
  `--json` 输出 `root`、`source`（`local` / `external` / `none`）、
  `config_path`、`lock_path`、`binding_dir`、`shadowed_local_config`、`missing`。
- `list` 没有位置参数。纯文本输出每个绑定一块：项目根一行，其下缩进
  `source: external binding`、`status:`（`active`，或 `project root missing`、
  `config missing`、`shadowed by local osdk.toml` 的组合），以及 `config:`、
  `lock:` 路径。`--json` 输出数组，含 `root`、`binding_dir`、`config_path`、
  `lock_path`、`root_exists`、`config_exists`、`shadowed_by_local`。

## 明确不做的事

外置绑定就是路径绑定：它没有 Git remote、分支或多套 profile 的概念，也不会
因为切换分支而换配置。需要多套配置并存时，仍按 osdk 的常规方式在配置文件里
组织。
