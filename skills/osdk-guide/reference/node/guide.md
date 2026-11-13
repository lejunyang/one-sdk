# `osdk node`

处理 Node 专属的全局 npm 包迁移。

```bash
osdk node migrate-packages --from <NODE_VERSION> (--to <NODE_VERSION> | --to-global) [--package NAME]... [--allow-scripts] [--apply]
```

| 参数 | 语义 |
| --- | --- |
| `--from` | 源受管 Node 版本，必填 |
| `--to` | 目标受管 Node 版本；与 `--to-global` 二选一 |
| `--to-global` | 迁入共享 npm prefix，校验后清理源版本中的对应包 |
| `--package NAME` | 可重复；只用于 `--to-global`，选择源包并明确替换共享目录中的不同版本 |
| `--allow-scripts` | 只用于 `--to-global` 且要求 `--package`；允许运行安装脚本，含依赖脚本 |
| `--apply` | 真正执行；省略时只生成迁移计划 |

```bash
osdk node migrate-packages --from 20.11.1 --to 22.4.0
osdk node migrate-packages --from 20.11.1 --to 22.4.0 --apply
```

项目依赖迁移不走该命令，使用 `[deps.npm]` / `[deps.pnpm]` 与 `osdk deps`。Node 安装后
是否启用 Corepack 由 `[settings.node].corepack` 控制；npm 动态工具的默认 installer
由 `[settings.npm].default_installer` 控制。

`osdk doctor` 会列出 Node 各版本目录里的旧全局包、精确版本和路径，以及共享目录中的
重复版本。先预览，再明确选择要保留的源包版本：

```bash
osdk node migrate-packages --from 24.18.1 --to-global
osdk node migrate-packages --from 24.18.1 --to-global --package agentbuddy --allow-scripts
osdk node migrate-packages --from 24.18.1 --to-global --package agentbuddy --allow-scripts --apply
```

`--to-global` 与 `--to` 二选一；`--package NAME` 可重复，只迁移选中的源包。共享目录已存在
不同版本时，必须用 `--package` 明确选择源版本，不能按版本号自动覆盖。省略 `--apply`
只读包元数据、打印计划，不调用 npm。npm/Corepack 本体不会迁移。含安装脚本或 gyp 标记的包
默认跳过并保留源副本；加 `--allow-scripts` 才纳入，并允许运行选中包及其依赖的 npm 生命周期脚本。
该选项要求同时指定 `--to-global` 和至少一个 `--package NAME`；仍需 `--apply` 才执行。
本地链接或别名包不会按 registry 包自动重装，需从原始来源单独处理。

已通过 `--package` 选中的包只显示“替换共享版本”，不再提示选择。包列表、目标目录和脚本策略
分行显示；`--apply` 显示执行状态和结果，不重复“加 --apply”等预览提示。

执行时按精确版本重新安装，默认禁用安装脚本；需要包源可用及适当的 npm 登录配置。
启用脚本后，脚本对共享目录之外的修改不在迁移回滚范围内。
请勿同时运行其他全局安装。安装或产物校验失败会保留源包和现有共享安装。成功后清理该源版本
中的对应包并重建 shim；其他 Node 版本里的副本需分别处理。共享目录里未选中的本地包链接及其
命令入口保持原样，包名与链接目录名不同也不会阻断迁移；命令名冲突仍会报错。源清理失败时
保留已安装的共享包，并报告旧共享目录的备份位置。
