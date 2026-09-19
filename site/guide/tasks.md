# 项目任务

在 `osdk.toml` 里用 `[tasks]` 声明项目命令，然后用 `osdk run <名字>` 执行。
目标是替代大多数 Makefile 与 npm scripts 的用途：任务默认就是「伪目标」，
不需要 `.PHONY`；依赖按拓扑顺序执行；用什么工具版本由 osdk 自己注入。

## 最简形式

```toml
[tasks]
build = "cargo build --release"
test = "cargo test"
```

```
osdk run build
```

这一档覆盖大多数场景，多数项目的多数任务应当停在这里。

## 多条命令

```toml
[tasks.ci]
run = [
  "cargo fmt --check",
  "cargo clippy -- -D warnings",
  "cargo test",
]
```

**数组元素依次执行，任一条失败就停止**，后面的命令不再运行。这等价于
`&&` 串联，但不依赖任何 shell 语法，因此在 Windows 与 Unix 上行为一致。

### 为什么不要用 `&`

`&` 在不同 shell 下是三件不同的事，同一份配置会在两个平台做两件事，而且
都不报错：

| shell | `a & b` 的含义 |
| --- | --- |
| Unix `sh` | a 放后台，b 立即开始（并发） |
| Windows `cmd` | a 跑完再跑 b，**无条件**，前一条的失败被吞掉 |
| PowerShell 7 | 尾随 `&` 表示后台 Job |
| PowerShell 5.1 | 语法错误 |

所以「失败也继续」和「并行执行」各有专门写法，见下面两节。

### 失败也继续

```toml
[tasks.ci]
run = [
  { cmd = "cargo clippy -- -D warnings", ignore_error = true },
  "cargo test",
]
```

被容忍的失败不会让任务失败，但**会打印一条警告**——容忍不等于无人知晓。

### 并行执行

```toml
[tasks.check]
run = [
  "cargo fmt --check",
  { tasks = ["clippy", "test", "doc"] },
  "echo all-green",
]
```

`{ tasks = [...] }` 里的任务一起执行，**全部完成后**才继续下一步。osdk 会
等待它们并收集退出码，任一失败就中止——这正是 `&` 做不到的事，后台进程
无人回收，失败会被悄悄丢弃。

## 依赖

```toml
[tasks.build]
run = "cargo build"
depends = ["fetch-deps"]
```

前置任务先执行，共享的依赖只跑一次。**顺序在 `depends` 之间不作保证**：
若确实需要「先 A，再 B 和 C 一起」，用上一节的 `run` 步骤表达，而不是
堆叠 `depends`。

依赖成环会在任何命令执行前报错，并打印出环的路径：

```
error: task dependency cycle: a -> b -> a
```

`depends` 里的名字写错同样在执行前报错，不会跑到一半才发现。

## 增量：输入没变就跳过

```toml
[tasks.build]
run = "cargo build --release"
sources = ["Cargo.toml", "crates/**/*.rs"]
outputs = ["target/release/osdk.exe"]
```

`sources` 全都比 `outputs` 旧时，任务被跳过：

```
$ osdk run build
build: 已是最新，跳过
```

判据默认比较修改时间。三种可选：

| `freshness` | 行为 | 适用 |
| --- | --- | --- |
| `"mtime"`（默认） | 比较修改时间 | 便宜，但 `git checkout` 与 CI 缓存恢复会刷新时间戳 |
| `"hash"` | 比较内容哈希 | 不受"内容没变但时间戳变了"影响，代价是读取全部输入 |
| `"always"` | 从不跳过 | 显式关掉增量 |

**任务定义本身也算输入**：改了 `run` 的内容，即使源文件没动也会重跑。

省略 `outputs` 时，osdk 用上次成功运行的时间作为基准，状态存在缓存目录里
（不写进项目，避免每个使用者都要加 `.gitignore`）。

### 两个容易踩的地方

**写错的 pattern 会报错，不会被当成"没有输入"。**

```
$ osdk run build
error: task `build`: sources: ["src/**/*.typo"] matched no files; a pattern that
matches nothing would make this task's freshness check silently meaningless
```

一个匹配不到任何文件的 `sources` 会让新鲜度判断变得空洞——任务要么永远跳过、
要么永远重跑，而配置看上去完全正常。所以这里选择直接失败。

**glob 的扫描从字面前缀开始。** `crates/**/*.rs` 只会进入 `crates/`，不会遍历
整个项目。这不是微优化：实测中一次从当前目录出发的全量遍历，在 `target/` 旁边
要 2,827.91 ms，而正确起点只需 61.26 ms，相差 46 倍。写 pattern 时尽量带上目录
前缀，`**/*.rs` 这种没有前缀的写法会走遍整棵树。

`!` 前缀表示排除：

```toml
sources = ["src/**/*.rs", "!src/generated/**"]
```

## Windows 变体

```toml
[tasks.build]
run = "make"
run_windows = "nmake"
```

`run_windows` 在 Windows 上**取代**（而非追加）`run`。

默认解释器是 Windows 上的 `cmd /c`、其他平台的 `sh -c`。之所以不选 `sh`
作为 Windows 默认，是因为那会要求用户额外安装 Git for Windows 或 Cygwin，
与 osdk「装上就能用」相悖。注意开发机上很可能**恰好**有 `sh`（甚至可能是
osdk 自己生成的 shim），所以依赖它的配置会在本机正常、到干净机器上失败。

要换解释器：

```toml
[tasks.deploy]
run = "./deploy.ps1"
shell = "pwsh -Command"
```

或者给整个配置作用域设默认值：

```toml
[task_config]
shell = "pwsh -Command"
```

## 任务能看到什么环境

osdk 在启动任务前**主动注入**该项目的环境，和 `osdk hook-env` 给 shell 的
是同一套：工具的 bin 目录前置进 `PATH`，`JAVA_HOME`、`GOROOT` 这类变量按
项目配置导出，再加上包缓存与模型适配的变量。

因此任务里看到的工具版本就是项目声明的版本，**无论从哪个目录、哪种 shell
调用**，也不要求用户先 `osdk activate`。

osdk 同时设置两个变量：

| 变量 | 含义 |
| --- | --- |
| `OSDK_TASK` | 恒为 `1`，表示当前进程是 osdk 任务 |
| `OSDK_TASK_NAME` | 当前任务名 |

前者的作用是让 profile 里的 osdk 激活钩子识别出「环境已经准备好了」，
不再叠加第二次激活。

任务自己的 `env` 优先级最高，可以覆盖上面任何一项：

```toml
[tasks.test]
run = "cargo test"
env = { RUST_BACKTRACE = "1" }
```

## 其他字段

```toml
[tasks.release]
description = "打包发布产物"      # 出现在 osdk task list
alias = ["rel"]                   # osdk run rel
dir = "packaging"                 # 相对配置文件所在目录
hide = true                       # 默认不出现在列表里
quiet = true                      # 抑制 osdk 自身输出，不影响任务输出
when = { os = "linux" }           # 平台过滤，与 [tools] 同一套写法
```

被 `when` 排除的任务不会被当成「不存在」：

```
$ osdk run linuxonly
error: task `linuxonly` is not available on this platform (os=linux)
```

`osdk task list` 也会把它单列出来，而不是让它悄悄消失。

## 命令

| 命令 | 作用 |
| --- | --- |
| `osdk run <名字>` | 执行任务 |
| `osdk run <名字> --dry-run` | 只打印将要执行什么 |
| `osdk task list` | 列出任务（`--hidden` 包含隐藏的） |
| `osdk task info <名字>` | 查看合并后的完整定义 |
| `osdk task deps <名字>` | 打印执行顺序 |

注意只有 `osdk run <名字>`，没有裸的 `osdk <名字>`：后者会被将来新增的
子命令遮蔽，是 mise 踩过并已建议脚本不要依赖的坑。

任务失败时 `osdk run` 以该任务的退出码退出，可以直接串进 CI 脚本。

## 信任

**声明任务不需要信任。** osdk 不会自作主张地跑任何任务：没有 postinstall、
没有生命周期钩子、没有任何自动调用，`[tasks]` 只被 `osdk run` 和 `osdk task`
读取。你敲下 `osdk run build` 这个动作本身就是授权，再要求一次 trust 等于
对同一件事问两遍——而一个总在你已经明确要求的事情上弹出的确认，只会训练人
不读就点同意。

对比 `syspkg` 就清楚了：它在 `osdk install` 期间动作，而用户并没有逐个包地
要求过，所以必须事先审阅；任务则永远是因为有人点名才运行。

**`task_config` 仍然需要信任**，因为它不是你点名的命令，而是一个环境设置：

```
$ osdk task list
error: project config is not trusted: /path/to/osdk.toml
these keys need review because they affect what runs on this machine:
  task_config -- 决定用什么解释器执行任务，任务的实际行为可能与写出来的不一致
```

它的 `shell` 字段决定作用域内**每个**任务用什么解释器。一份配置若悄悄写上
`shell = "evil --run"`，之后每次 `osdk run` 执行的都不再是任务文本写的东西，
而调用处看不出任何异样。审阅之后 `osdk trust` 即可。

## 不做什么

osdk 的任务是**任务运行器**，不是构建系统。明确不提供 make 的模式规则
（`%.o: %.c`）与自动变量（`$@` / `$<`）：它们是「为每个文件生成一条规则」
的语言，一旦引入就要接着提供依赖链推导、VPATH、中间文件生命周期，等于
把 make 的全部复杂度搬过来。mise、just、Task、cargo-make、npm scripts、
deno task、Turborepo 没有任何一个提供模式规则，这是一致的判断。

需要编译一棵源码树时，那是 cargo / tsc / go build 的职责；任务运行器的
职责是**以正确的工具版本和环境去调用它们**。
