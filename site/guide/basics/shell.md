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
排除只是不生成 shim——工具仍然装着，激活后仍在 PATH 上，`osdk exec` 也可用。
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
