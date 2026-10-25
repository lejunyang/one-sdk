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
`[settings.shims]` 暴露策略后。Go/Cargo 的托管输出目录在 SDK 激活时加入 PATH；`reshim` 也会为其中的原生应用生成入口，项目外不要求选择编译器。rustup proxy 和 SDK 命令仍要求工具链选择。原生输出覆盖的表外目录不会扫描。

托管 npm 共享 prefix 的包在切换 Node 后仍保留；全局 JS 命令按当前 tool_order 选择 Node，不要求独立 npm，不任取已安装版本。项目外缺少选择时可用 `osdk current node --sources` 排查，再主动 `osdk use --global node@<version>`。system 管理器使用原生目录；native addon、固定解释器 wrapper 和 Python 工具环境仍需各自兼容处理。`override_env` 控制缓存覆盖，不控制托管全局目录。


显式配置 `tool_order=["system"]` 可使用受支持的已有 PATH/home 工具；默认不启用。显式版本及托管锁/frozen 仍保持权威。参见[system 配置、适配器和限制](../configuration/settings.md#system-执行来源)。

自动对账也比较命令集合，快速卸载再安装不依赖目录时间戳一定变化。
