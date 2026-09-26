# `osdk reshim`

根据所有已安装工具的 inventory 重新生成 shim 启动器。

```bash
osdk reshim
```

无命令专属参数。它不会重新下载或重装工具。命令冲突、include/expose/exclude 策略来自
`[settings.shims]`，见 `reference/configuration/settings.md`。

典型场景：安装目录已恢复但 shim 缺失，或更改 shim 暴露策略后需要重新发布入口。
若 `doctor --verify` 报告安装文件漂移，应先 `install --force`，不要把 reshim 当修复安装内容。
