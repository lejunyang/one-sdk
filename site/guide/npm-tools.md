# npm 开发工具

osdk 可以把 npm Registry 中发布的命令行包作为独立开发工具管理。请求使用
`npm:<package>` 命名空间，安装、固定、临时执行、查询、升级和卸载都复用统一的
osdk 命令。

::: tip 先区分两个名字
`npm@11.5.2` 安装的是 npm 包管理器；`npm:prettier@3` 安装的是 npm Registry
中的 Prettier 工具。若确实要把 Registry 中名为 `npm` 的包当作动态工具，请写
`npm:npm`。
:::

## 快速开始

将 Prettier 安装并固定到当前项目：

```bash
osdk use npm:prettier@3
eval "$(osdk activate bash)"
prettier --check .
```

`use` 会在需要时先安装受管 Node，再安装包、生成其私有 `.bin` 命令的 shim，并把
`"npm:prettier" = "3"` 写入最近的项目配置。使用 `--global`（或 `-g`）
可改为用户级固定。若只想运行一次、不修改配置：

```bash
osdk exec --tool npm:prettier@3 -- prettier --check .
```

## 包名与版本语法

```text
npm:<package>[@VERSION]
npm:@<scope>/<package>[@VERSION]
```

普通包与 scoped 包都受支持：

```bash
osdk install npm:prettier@3
osdk install 'npm:@antfu/ni@0.21.12'
osdk exec -t 'npm:@antfu/ni@0.21.12' -- ni
```

scoped 包中的第一个 `@` 属于 scope，最后一个 `@` 才分隔版本。Shell 通常不会
特殊处理它，但加单引号可以避免命令被其他包装层误解。省略版本会解析最新稳定版；
`3`、`3.6` 等前缀会选取最高的匹配稳定版。

## 完整生命周期

以下示例覆盖 npm 工具可用的通用命令面：

```bash
# 安装或固定
osdk install npm:prettier@3
osdk use npm:prettier@3

# 临时运行，不写项目固定版本
osdk exec -t npm:prettier@3 -- prettier --check .

# 查询
osdk list npm:prettier
osdk current npm:prettier
osdk where npm:prettier
osdk where npm:prettier@3.6.2

# 检查和升级当前项目，或只操作指定工具
osdk outdated
osdk outdated npm:prettier@3
osdk upgrade
osdk upgrade npm:prettier@3

# 删除精确版本；--yes 适合非交互环境
osdk --yes uninstall npm:prettier@3.6.2

# 重建所有已安装工具的 shim
osdk reshim
```

| 命令 | npm 工具行为 |
| --- | --- |
| `use` | 必要时安装，生成 shim，并保存项目或用户级版本；输入的版本前缀会保留 |
| `install` | 显式请求直接安装；无参数且无 `-o` 时优先使用当前平台的 `osdk.lock` |
| `exec` | 必要时安装，在精确工具环境中执行包实际导出的 bin，不写 pin |
| `list` | 列出已安装版本；参数是 `npm:<package>` backend id，不带版本 |
| `current` | 显示当前目录配置解析到的请求；它不表示该版本一定已安装 |
| `where` | 输出匹配的安装目录；传精确版本可避免选择歧义 |
| `uninstall` | 删除安装并重新协调 shim；自动化建议传精确版本和全局 `--yes` |
| `outdated` | 重新解析目标并报告该精确版本是否尚未安装，不读取旧 lock |
| `upgrade` | 重新解析并安装，再刷新当前 host 的 lock，不以旧 lock 为解析输入 |
| `reshim` | 根据已安装工具的 inventory 重建命令入口，无工具参数 |

还可用 `osdk list-remote npm:prettier [FILTER]` 查看 Registry 中的稳定版本。

## 项目配置与构建脚本

简单的版本固定可以使用字符串；带安装策略时使用结构化 `[tools]` 项。包含 `:`
或 scoped 包名的 TOML key 应加引号：

```toml
[tools]
node = "22"
"npm:prettier" = "3"

[tools."npm:@scope/native-tool"]
version = "1.2.3"
allow_builds = ["@scope/native-tool", "esbuild"]
```

npm 工具的 lifecycle/build scripts **默认全部禁用**。`allow_builds` 有三种有效策略：

| 配置 | 效果 |
| --- | --- |
| 省略或 `false` | 禁止所有依赖的构建脚本；默认且推荐 |
| `["pkg-a", "pkg-b"]` | 只允许列出的包运行构建脚本 |
| `true` | 允许整个依赖图运行构建脚本；危险，只应在完整审阅后使用 |

一次性 CLI 调用可写成 `-o allow_builds=esbuild,sharp`，或显式使用危险的
`-o allow_builds=true`。团队配置建议使用数组形式，让允许范围清晰可审阅。

## 受管 Node 与来源选择

动态 npm 工具始终使用 osdk 管理的 Node，不依赖 PATH 上碰巧存在的系统 Node。请求
中没有 Node 时，osdk 会自动加入当前项目解析到的 Node；项目也没有声明时则加入
`node@latest`。Node 会先于 npm 工具安装。若需要团队可重复的运行时，请在项目中
显式固定 Node。

每个 `npm:<package>` backend 默认在 npmmirror 与 npmjs 官方源之间使用通用
`sources.selection = "auto"` 策略：并发探测、按吞吐和首字节时间排序，并在 TTL
内复用结果。缓存与候选集合指纹绑定；URL、顺序、优先级、启用状态或认证 header
发生变化时，旧排序不会被误用。

这里的 `Source.headers` 只用于 osdk 自己执行的 metadata 请求和 probe：header 受
index/download origin 约束，同源 redirect 保留，跨源后移除。Aube 2.1 package fetch
不会接收任意 `Source.headers`；私有 npm Registry 的认证应配置在 Aube/npm 原生可信
配置或环境变量中。

```bash
osdk source list npm:prettier
osdk source test npm:prettier
osdk --refresh-sources install npm:prettier@3
osdk --source npm install npm:prettier@3
```

这里选择的是**工具自身的下载 source**。它与运行项目中的 `npm install` 前执行的
`[registries.npm]` Registry 预检是两套控制面；后者见
[JavaScript 包管理器](./package-managers#registry-预检)。

## 锁定与离线重装

schema 2 `osdk.lock` 为每个动态 npm 工具保存 package、精确 Node 版本、graph 格式、
SHA-256 和内容寻址路径；完整 Aube 图写入相邻的
`osdk.lock.d/npm/<sha256>.yaml`。graph 携带传递依赖的 integrity，生成过程不会执行
lifecycle scripts。`osdk.lock` 与 `osdk.lock.d/` 必须一起提交到仓库。

离线重装前需要同时准备主 lock、graph sidecar 和 Aube package cache/store：

```bash
# 联网阶段：生成完整 graph，并下载 graph 引用的包
osdk lock
osdk install

# 之后保留 osdk.lock、osdk.lock.d/ 与同一份缓存，冻结重装
osdk --offline install
```

只有主 lock 或 sidecar 不代表包内容已经缓存。sidecar 缺失、损坏、超过 16 MiB，或
graph 引用的缓存内容缺失时，离线安装都会明确失败。显式
`osdk --offline install npm:prettier@3.6.2` 会绕过项目 lock；需要从 lock 恢复时应
使用无参数 `install`。详情见[可复现锁文件](./lockfiles)。

## 命令发现与冲突

安装后，osdk 从合成安装项目的 `node_modules/.bin` 发现命令并记录 inventory；其中
可能包含根包或传递依赖创建的 bin。`.bin` 为空、命令目标无法解析或目标逃出安装根
目录都会失败。若不同 backend 导出同名命令，shim 发布和运行时路由会 fail closed，
不会按扫描顺序任意选择。运行时若当前配置只选中一个 owner，可以据此消除路由歧义；
当前 CLI 生成或 `reshim` 仍会对多个已安装 owner 移除歧义 shim 并报错。同一 backend
的多个版本由当前版本选择规则处理。

内部的 Aube 安装、graph sidecar 校验、缓存布局和路由算法见
[npm 工具实现](./implementation/npm-tools)。
