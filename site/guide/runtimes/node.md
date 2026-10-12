# Node.js

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

`arch` 可用于生成其他架构的 lock 区段，但 osdk 没有只下载模式；实际安装会拒绝
不能在当前 host 执行的 Node artifact。

Node 版本列表会合并所有可达 source 的 index，并保留排序靠前来源对重复版本的 LTS
标记；因此一个响应正常但同步落后的镜像不会隐藏后续 source 已发布的版本。下载阶段再
按同一顺序逐个校验 `SHASUMS256.txt` 对应的归档。

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
