# `osdk reshim`

根据所有已安装工具的 inventory 重新生成 shim 启动器。

```bash
osdk reshim
```

无命令专属参数。它不会重新下载或重装工具。命令冲突、include/expose/exclude 策略来自
`[settings.shims]`，见 `reference/configuration/settings.md`。

典型场景：安装目录已恢复但 shim 缺失，或更改 shim 暴露策略后需要重新发布入口。
若 `doctor --verify` 报告安装文件漂移，应先 `install --force`，不要把 reshim 当修复安装内容。

reshim 还会扫描受 osdk 重定向的包管理器全局 bin 目录（npm/pnpm 的受控全局目录），为你直接用 `npm i -g` / `pnpm add -g` 安装的命令生成 shim，并登记其全局来源，使这些入口在后续 reconcile 中不会被误删。运行时 shim 会把这些命令回退到对应包管理器的受控环境执行。所以直接做全局安装后，跑一次 `osdk reshim` 即可让新命令纳入 osdk 管理。
