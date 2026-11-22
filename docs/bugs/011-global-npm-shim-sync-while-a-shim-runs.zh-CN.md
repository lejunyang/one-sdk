# 011 — Windows 上有任一 shim 在运行时，`use -g npm:` 在落盘 shim 时失败并整体回滚

**状态**：已修复 · **严重度**：高 · **实测环境**：Windows 11 x64、osdk 0.0.13、NTFS（2026-11-22）

## 现象

一台机器上开着一个经 shim 启动的长驻命令（这里是一个 `omem peer open` 转发），这时任何全局 npm 包都装不上：

```
$ osdk --yes use -g npm:@google/gemini-cli@latest
error: syncing shim E:\osdk-data\data\shims\gemini: 另一个程序正在使用此文件，进程无法访问。 (os error 32)
$ osdk where -g npm:@google/gemini-cli
error: npm:@google/gemini-cli is not installed
```

`@github/copilot`、`@qwen-code/qwen-code` 同样失败。安装本身已经完成，失败发生在最后的发布阶段，于是整次安装连同配置、lock 一起回滚。

## 根因

Windows 上无扩展名的 shim 是 `osdk-shim.exe` 的**硬链接**（`write_windows_posix_launcher`，见 005），所有 shim 共用同一份文件数据。`use -g npm:` 发布新 shim 后，`sync_shim_publication` 对每个受影响的 shim 调 `sync_file_if_present`，后者以**读写方式**打开文件再 `sync_all()`。

Windows 不允许以写方式打开正在运行的映像文件，换一个名字（硬链接）也一样。所以只要任意一个 shim 在跑，这一步就报 `ERROR_SHARING_VIOLATION`（32）。

在同一台机器上对一个正在运行的 exe 建硬链接实测：

| 操作 | 结果 |
| --- | --- |
| 新建硬链接 | 可以 |
| 只读打开 | 可以 |
| 读写打开（含 `FILE_SHARE_DELETE`） | 失败，os error 32 |
| 改名、删除这个链接 | 可以 |

可见“先删再建链接”的 shim 重新生成不受影响，只有落盘这一步会撞上。

## 影响范围

- **只有全局 npm 包的发布路径**（`use -g npm:`、`upgrade -g npm:`）会落盘 shim，其他 backend 的安装不调用 `sync_shim_publication`。
- **只有 Windows。** Unix 上 shim 是指向 `osdk-shim` 的符号链接，`sync_file_if_present` 遇到符号链接直接跳过，再对 shims 目录本身 `fsync`。
- 退回字节复制（跨卷、文件系统不支持硬链接）的 shim 是一份新文件，没有在运行，落盘照常成功。

## 修复

硬链接到启动器的 shim 没有自己的数据要刷：内容就是早已在磁盘上的 `osdk-shim.exe`，新目录项属于文件系统元数据，对文件调 `FlushFileBuffers` 也覆盖不到它。所以在 Windows 上，`sync_shim_publication` 先用 `osdk_core::shim::is_launcher_link` 判断这个 shim 是不是启动器本身（`same_file` 比较文件身份）；是就跳过它，只落盘旁边新写的 `.cmd`。字节复制的 shim 仍然落盘。

修法不靠识别错误码：错误码只说明“现在打不开”，不说明“本来就不需要刷”。

## 怎么抓到它 / 回归防线

单元测试从来没有在“有 shim 在运行”的条件下发布过 shim，所以一直全绿。新增的 `shim_publication_sync_skips_launcher_links_while_a_shim_runs`（仅 Windows）把 `ping.exe` 复制成临时数据目录里的 `osdk-shim.exe` 并让它运行，给它建一个硬链接作为 shim，先断言以写方式打开确实失败（证明测试落在被测条件之内），再要求 `sync_shim_publication` 成功。撤掉修复后，这条测试报的正是现场的 `os error 32`。

真机上用修复后的构建，在那个转发仍在运行时装上了 `@google/gemini-cli` 0.70.0，`gemini` 和 `gemini.cmd` 都已发布，转发不受影响。
