# `osdk reshim`

根据所有已安装工具的 inventory 重新生成 shim 启动器。

```bash
osdk reshim
```

无命令专属参数。它不会重新下载或重装工具。命令冲突、include/expose/exclude 策略来自
`[settings.shims]`，见 `reference/configuration/settings.md`。

典型场景：安装目录已恢复但 shim 缺失，或更改 shim 暴露策略后需要重新发布入口。
若 `doctor --verify` 报告安装文件漂移，应先 `install --force`，不要把 reshim 当修复安装内容。

通过受 osdk 重定向的包管理器（npm/pnpm/yarn/bun/deno）做全局安装或卸载时，
osdk-shim 会在该工具进程退出后自动对账其全局 bin 目录：为新装命令生成 shim、为卸载
命令清理 shim（安装/卸载、甚至手动删除都一样），因此通常**不需要**再手动跑
`osdk reshim`。reshim 仍可作为手动兜底，例如恢复了安装目录、批量重建入口，或更改
`[settings.shims]` 暴露策略后。Go/Cargo 这类自包含原生工具不生成 shim：其输出目录
（`GOBIN` / `CARGO_HOME/bin`）直接放在 PATH。


显式配置 `tool_order=["system"]` 可使用受支持的已有 PATH/home 工具；默认不启用。显式版本及托管锁/frozen 仍保持权威。参见[system 配置、适配器和限制](../configuration/settings.md#system-执行来源)。
