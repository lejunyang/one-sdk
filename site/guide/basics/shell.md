# Shell 激活与补全

shim 让任何进程都能按当前目录使用正确的版本，shell 激活则在提示符处切换整套环境。本页说明二者的工作方式、接入命令和补全脚本；决定哪些命令进入 PATH 见[控制哪些命令进 PATH](./path-control)。

## Shim 与 Shell 激活

安装和 `use` 会生成 shim；如果安装目录变化，可重建：

```text
osdk reshim
```

对动态工具，`reshim` 只为配置精确匹配的 `.osdk-install.json` schema 1 身份发布 launcher，
不会选择同版本的其他根，旧 `.osdk-tool.json` 状态也永远不可执行。

shim 在每次执行时按当前目录解析版本，因此 IDE、CI 和未安装 prompt hook 的进程也
能使用项目 pin。它会避免递归调用自身；Windows `.cmd`/`.bat` 目标通过
`%ComSpec% /D /S /C call` 执行以保留参数、stdin/stdout 和状态。生成的 `.cmd` 包装
按系统 OEM 代码页写入安装路径，让没有控制台、回退到 OEM 代码页的 cmd 也能进入非
ASCII 安装路径；遇到 OEM 代码页表达不了的字符时，再在引用行前用 `chcp 65001` 切到
UTF-8 兜底。

Shell 激活命令：

```text
osdk activate bash|zsh|fish|powershell|pwsh
osdk deactivate bash|zsh|fish|powershell|pwsh
```

常见接入方式：

```bash
eval "$(osdk activate bash)"
eval "$(osdk activate zsh)"
osdk activate fish | source
osdk activate powershell | Invoke-Expression
```

hook 在 prompt/目录变化时重新计算环境。PATH 顺序为：

```text
已生成 shims > 独立 npm/pnpm/yarn > Node > 其他 backend > 原 PATH
```

它还导出 backend 环境、下游缓存和已启用的模型 adapter。osdk 保存所有被管理变量
的原值；`deactivate` 移除 hook 并恢复环境。PowerShell hook 带重入保护。

### 没有 shell 钩子时

IDE、CI 步骤、容器、计划任务和 `pwsh -NoProfile` 都不会运行钩子。shim 在持久 PATH
上，所以命令本身仍按目录选对版本；缺的是钩子导出的变量——backend 环境
（`JAVA_HOME`、`ANDROID_HOME`……）、下游缓存目录和模型 adapter——以及没有 shim 的
命令。两个入口补上这部分，都与钩子共用同一份计算：

```bash
osdk env                           # dotenv，默认格式
osdk env --format json
osdk env --format bash             # 或 zsh、fish、powershell
osdk exec -- cargo build           # 不带 --tool，在这个环境里运行
```

- **dotenv / json** 写出完整的 PATH，因为读文件的一方没法表达「在原 PATH 前追加」。
  PATH 以激活之前的那份为基础，所以在已激活的 shell 里导出，也不会把别的目录的工具
  写进文件。dotenv 的值不加引号：`docker --env-file` 会把引号当成值的一部分，而大多
  数 dotenv 解析器会去掉它。含换行的值写不进 dotenv，会报错并提示改用 json。
- **shell 格式**只做在原 PATH 前追加的 `export`，没有钩子的 `OSDK_ORIG_*` /
  `OSDK_MANAGED_ENV` 簿记，适合在未激活的 shell 或 CI 脚本里执行一次，
  例如 `eval "$(osdk env --format bash)"`。不要放进 rc 文件代替 `activate`：它不会
  随目录切换。
- **`osdk exec -- <命令>`** 不带 `--tool` 时什么都不安装，选中但未安装的工具会像在
  激活的 shell 里一样报错，请先 `osdk install`。从已激活的 shell 调用时，外层激活设置
  而当前目录不设置的变量，会恢复成激活前的值。带 `--tool` 时行为不变。

导出的是**当前目录**的环境，项目选择随目录不同而不同；IDE 打开别的项目时需要重新
导出。

### 挑选要生成的 shim

默认给每个可执行文件都生成 shim。`[settings.shims]` 可以收窄范围：

```toml
[settings.shims]
include = []                      # 非空时只生成匹配项——对**所有**工具生效
exclude = ["android-ndk:*"]       # 最后生效，因此总是胜出
expose = []                       # 增量放行默认被挡下的命令，不影响其他工具

[settings.shims.tools."conda:m2-base"]
expose = ["make", "sh"]           # 按工具覆盖，未写的字段继承全局
```

`include` 是**全体工具**的白名单，`expose` 只做加法。要取回某个被挡下的命令用
`expose`；要收窄某个工具，优先用 `[settings.shims.tools."<id>"]` 下的 `include`，它
的作用域限定在该工具内。完整语义与模式语法见
[控制哪些命令进 PATH](./path-control)。

模式支持 `*` 与 `?`，忽略大小写；带 `backend:name` 前缀时只作用于该 backend。
排除只是不生成 shim——工具仍然装着，激活后仍在 PATH 上，`osdk exec` 也可用。例外是
conda 这类与依赖闭包共用 prefix 的安装：激活不把它的目录放上 PATH，只经 shim 触达，
详见[依赖闭包里的命令](./path-control)。
改完执行 `osdk reshim` 生效。

生成与路由读的是同一份判定，因此被排除的 backend 也不会在运行时抢到某个
共享的命令名。

## 补全

```text
osdk completions bash|elvish|fish|powershell|zsh
```

命令把补全脚本写到 stdout；请按 shell 的惯例保存或 source。例如：

```bash
osdk completions bash > ~/.local/share/bash-completion/completions/osdk
osdk completions zsh > ~/.zfunc/_osdk
osdk completions fish > ~/.config/fish/completions/osdk.fish
```
