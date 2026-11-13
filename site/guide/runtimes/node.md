# Node.js

配置参考：[适用的 tool_order 来源](../config/tool-selection#node)、[项目来源诊断](../config/tool-selection#inspect-sources)、[本工具的选择器、托管变量与缓存覆盖](../config/tool-environment#node)。

```bash
osdk install node@20
osdk install node@20.19.0 -o corepack=true
osdk lock node@20 -o arch=arm64
```

| 选项 | 值 | 作用 |
| --- | --- | --- |
| `arch` | `x64`、`arm64`、`x86`、`arm` | 选择目标 Node artifact；默认 host 架构 |
| `corepack` | `true|1|yes|on` 或 `false|0|no|off` | 是否用目标 Node 自带的 Corepack 执行 `enable --install-directory <node-bin>` |

`corepack` 未显式传入时取 `[settings.node].corepack`，默认 `false`。启用失败会删除
本次安装，不留下完成标记。Node 的 shim 只负责 `node` 和 `corepack`；npm/npx 由
独立 npm backend 或相应 Node 安装的路由 shim 协调。执行环境还会在用户未设置时
提供共享 `npm_config_cache`。

Node 随附的 npm 和独立安装的 npm 都默认把全局包写入 `data/global/npm`，
直接调用原始 npm、未加载 shell 激活时也适用。安装或复用版本时会设置此默认值；
升级 osdk 后运行 `osdk reshim` 可补齐已有安装。此操作不移动旧包。用户 `.npmrc`、
环境变量和 `--prefix` 仍按 npm 原生优先级覆盖默认值；经 shim 执行时另有托管
`npm_config_prefix` 注入。直接执行原始 npm 后，运行 `osdk reshim` 发布新命令。

`arch` 可用于生成其他架构的 lock 区段，但 osdk 没有只下载模式；实际安装会拒绝
不能在当前 host 执行的 Node artifact。

Node 版本列表会合并所有可达 source 的 index，并保留排序靠前来源对重复版本的 LTS
标记；因此一个响应正常但同步落后的镜像不会隐藏后续 source 已发布的版本。下载阶段再
按同一顺序逐个校验 `SHASUMS256.txt` 对应的归档。

Node 只声明 `node` 和实际存在的 `corepack`，不会把同目录第三方命令当作运行时自带能力。
旧全局包不会被 `reshim` 搬迁或删除；只有旧目录副本时，需先按下节迁往共享 prefix
再发布 shim。共享 npm 中的同名命令可以正常发布，不再与 Node 产生归属冲突。

## 将旧 Node 全局包迁往共享目录

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

Node 也提供全局包迁移：

```text
osdk node migrate-packages --from VERSION --to VERSION [--apply]
```

```bash
# 只生成计划
osdk node migrate-packages --from 20.19.0 --to 22.17.0

# 执行迁移
osdk node migrate-packages --from 20.19.0 --to 22.17.0 --apply
```

源、目标都必须是已安装且含 npm 的受管 Node。osdk 通过源版本的
`npm ls -g --depth=0 --json --long` 枚举，跳过 npm 自身、目标已存在的包，以及声明
`hasInstallScript=true` 或 `gypfile=true` 的原生/安装脚本包。`--apply` 按精确版本安装；
失败时恢复目标原有的全局包集合。

`reshim` 即使当前优先选择 system Node/npm，也会修复磁盘上已有托管安装的 npm
默认 prefix；只处理 osdk 安装目录，不修改系统 Node/npm 的配置。
