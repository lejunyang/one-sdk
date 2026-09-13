# 004 — PowerShell 钩子挂在命令查找上，一条命令触发 22 次激活

**状态**：已修复 · **严重度**：高 · **实测环境**：Windows x64、PowerShell 7、osdk 0.0.2

## 现象

在 `E:\Projects\omy` 里启动 PowerShell 极慢，且**慢的不只是启动**：进入之后每敲一条命令都要等一秒以上。

三个当时无法解释的点：

- 用户主目录和项目目录一样慢，所以不像是「项目里 conda 工具太多」。
- 单独跑 `osdk hook-env` 只要 ~400 ms，解释不了一条命令 1.1 秒的延迟。
- `-NoProfile` 启动只要 329 ms，含 profile 要 1527 ms，差额远大于一次 hook 的代价。

## 根因

`activate/mod.rs` 的 PowerShell 片段把 hook 挂在 `PostCommandLookupAction` 上：

```powershell
$ExecutionContext.SessionState.InvokeCommand.PostCommandLookupAction = {
  if ($script:OsdkHookRunning) { return }
  ...
}
```

这个回调在**每次命令查找**时触发，不是每次提示符。于是：

- 一条命令内部只要解析多个命令名（管道、`ForEach-Object` 的脚本块），就会触发多次；
- 循环每迭代一次触发一次；
- 重入保护 `$OsdkHookRunning` 只能防止 hook 内部再次触发，**防不住相邻命令各触发一次**。

而 bash / zsh / fish 从一开始就是提示符级的（`PROMPT_COMMAND`、`precmd_functions`、`fish_prompt`），只有 PowerShell 这一处不同。

## 实测证据

用计数 wrapper 替换 osdk 统计真实 `hook-env` 启动次数（`-NoProfile`，避免自我污染）：

| 用户操作 | 旧（每次命令查找） | 新（每次提示符） |
| --- | --- | --- |
| 只启动 shell | 1 次 | 1 次 |
| 敲 3 条命令 | 7 次 | 1 次 |
| 循环里调 10 次命令 | **22 次** | **1 次** |

改后用真实构建的二进制做端到端对照（3 条命令 + 渲染 3 次提示符 / 循环 10 次 + 渲染 1 次）：

| 场景 | 旧 hook 次数 / 耗时 | 新 hook 次数 / 耗时 |
| --- | --- | --- |
| 3 条命令 | 36 次 / 19,977 ms | 5 次 / 3,384 ms |
| 循环 10 次 | 37 次 / 20,324 ms | **3 次 / 2,774 ms（12.3x）** |

单次 `hook-env` 的成本（供对照，属 002）：`E:\Projects\omy` 503 ms、无项目配置 389 ms；应用其输出仅 ~1 ms。

## 影响

所有在 PowerShell 里用 `osdk activate` 的用户，每条命令多等约 1.1 秒，且**越是脚本化、越是循环，代价越大**。这也是「omy 目录启动 pwsh 很卡」的真正主因——项目里 11 个 conda 工具只让单次扫描从 389 ms 涨到 503 ms，真正的放大器是触发次数。

## 修复

hook 改挂到 `prompt` 上，与其他三个 shell 对齐。两个必须一起做的配套：

1. **包裹而非覆盖已有 `prompt`**。Oh My Posh、Starship 和手写提示符都只是一个 `prompt` 函数，直接定义自己的会静默丢掉用户的。实现是激活时捕获现有 `prompt`（没有则用 PowerShell 内置默认），在 hook 之后 `& $script:OsdkOriginalPrompt` 调用它。捕获用 `if (-not $script:OsdkOriginalPrompt)` 守卫，避免重复 source 时把自己的包装层层嵌套。
2. **deactivate 要把 `prompt` 还回去**。只删不还会让会话彻底没有 `prompt` 函数——用户为了修「慢」而执行 deactivate，结果丢了提示符，比原缺陷更糟。

hook 内部的异常用 `catch { }` 兜住：hook 失败不该让用户失去提示符。

`deactivation_script` 仍保留 `PostCommandLookupAction = $null` 一行，用于卸载**修复前已经激活**的老会话。

## 回归防线

三个静态断言 + 一个端到端断言，各拦不同的退化：

- `powershell_hook_runs_on_the_prompt_not_on_every_command_lookup` — 断言不含 `PostCommandLookupAction =`（匹配赋值而非裸名字：片段的注释里提到了这个机制，只查子串会误伤注释）。
- `powershell_hook_preserves_an_existing_user_prompt` — 断言会先 `Get-Command prompt` 探测、捕获到 `$script:OsdkOriginalPrompt`、并真的 `&` 调用它，且捕获有守卫。**漏掉「真的调用它」这一条，包装器可以捕获后不调用，用户提示符变空白而测试全绿。**
- `powershell_deactivation_restores_the_wrapped_prompt` — 断言 deactivate 会 `Set-Item Function:global:prompt` 装回并清掉捕获槽。
- `scripts/windows-runtime-smoke.ps1` — 原来断言 `PostCommandLookupAction -ne $null`，改后这条会**永远为真地失效**（因为它断言的机制已不存在），故改为断言 `Function:prompt` 存在且 `prompt` 返回非空，deactivate 后仍能返回非空。

这些都是字符串断言，只能证明生成的代码长什么样，**不能证明它跑得动**。所以另外在真 pwsh 里 source 了新代码实跑：无 stderr、自定义 `MYPROMPT>` 保留、deactivate 后 `prompt` 仍是用户自己那个。

## 待调研（用户已确认当前不使用任何 prompt framework）

包裹策略在真实 prompt framework 下的行为尚未实测，以下留待后续验证：

- **Oh My Posh / Starship**：它们通过 `Invoke-Expression (oh-my-posh init pwsh)` 安装 `prompt`。若用户在 osdk 激活**之后**才初始化，它们会覆盖掉 osdk 的包装（hook 失效但不报错）；若在**之前**，osdk 会正确包裹。需要确认是否应该检测并提示顺序。
- **PSReadLine 的 `ContinuationPrompt`**：多行输入时的续行提示符不走 `prompt` 函数，不受影响，但需确认包装不会干扰 PSReadLine 的渲染。
- **`$PROFILE` 里定义 `prompt` 的时机**：osdk setup 写入 profile 的位置若在用户自己的 `prompt` 定义之前，捕获到的会是内置默认而非用户的。需要核实 setup 的插入位置。
- **嵌套提示符**（`$nestedPromptLevel > 0`、`Enter-PSHostProcess`、调试器断点）：内置默认实现用 `'>' * ($nestedPromptLevel + 1)` 表达层级，包装后是否仍正确显示层级未验证。
- **PowerShell 5.1**：本仓库要求 pwsh 7，但用户机器上可能存在 5.1 会话。`Set-Item Function:global:prompt` 与 `Get-Command -CommandType Function` 在 5.1 的行为需确认。
