# npm 开发工具实现

本页描述 `npm:<package>` 动态 backend 的内部边界。用户命令和配置示例见
[npm 开发工具](../npm-tools)。它与固定 npm CLI 的 `npm` backend、项目依赖命令的
Registry 预检是三个相邻但不同的概念。

## 身份、解析与生命周期编排

[`ToolRequest::parse`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs)
会先识别 `npm:`。普通包在包名后的 `@` 分隔版本；scoped 包先解析
`@scope/name`，再把其后的第二个 `@` 作为版本分隔符。请求解析与 inventory 身份都会
把包名规范为小写。空包名、只有 scope、多余路径层级、反斜杠、冒号或空白会被拒绝。裸 `npm`
继续映射内置 npm CLI backend，因此不会被动态 backend 遮蔽。

[`Registry::get`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/registry.rs)
按需构造 `NpmPackageBackend`。通用生命周期命令不需要为它添加专门的 Clap
子命令：`use`、`install`、`exec`、`outdated` 和 `upgrade` 都把同一个
`ToolRequest` 交给 backend；`list`、`current`、`where`、`uninstall` 与 `reshim`
则结合配置和磁盘 inventory 恢复动态 backend。

请求集合包含 npm 工具且没有 Node 时，
[`inject_node_dependency`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs)
加入当前目录解析到的 Node，未声明时加入 `latest`。安装编排先串行完成 Node，再并发
调度其余工具。Aube 收到的 runtime selector 只指向该受管 Node；安装和 shim 执行都不
以系统 PATH 中的 Node 作为隐式依赖。

`use` 在该旧流程之前增加两条作用域分支：非全局 `npm:*` 请求会检查最近的
`package.json`，存在时直接修改该真实项目；全局请求忽略项目状态并安装到 osdk 自有
前缀。本地查找不到 `package.json` 时，则回到原有隔离 backend 路径。

解析或安装 osdk 自有的动态 npm 工具前，
[`identity_options`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/dynamic.rs)
只接受 `installer` 和 `allow_builds` 作为公开身份输入；lock 重放注入的内部
`__osdk_*` 字段会被刻意排除。installer 会规范化；`allow_builds` 将 false-like 值规范为
省略默认 deny 策略、true-like 值规范为 `true`，包列表则转为小写、排序、去重的逗号分隔
值；默认的 `installer=auto` 也会省略。未知公开 key 会在安装前失败。这些规范 material
options 与 tool、精确 version、platform、scope、dependencies 和 materials 一起构成完整的
规范安装身份，其带 domain separation 的 BLAKE3 `install_id` 使用 `b3-v2:` 格式。

## 安装器规划与单次委托

[`npm_tools.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/npm_tools.rs)
会在修改前完成整个安装器规划。`auto` 对新项目或任何受支持的现有原生 lock 选择
Aube。目前支持 Aube/pnpm v9 与 npm lock v2/v3；已知但不支持的 npm 或 pnpm 格式则
选择它的原生 owner。`packageManager` 声明会与 lock owner 交叉校验；冲突或多个 lock
都会 fail closed。显式 `installer=aube|npm|pnpm` 可覆盖声明，但不能绕过 lock 兼容性。

首次发现会在不修改项目的前提下选择候选安装器。取得每项目 npm 锁后，osdk 会重新读取
manifest 与原生 lock，并用用户最初请求的安装器（`auto` 或显式选择）重新规划，避免并发
lock owner 变化留下过期的具体计划。锁内选出的具体计划随后固定用于调用。原生项目委托
使用精确的受管 npm 或 pnpm 可执行文件、受管 Node、预检后的 Registry 环境，并且只启动
一次子进程。非零退出会原样返回，不会改用 Aube 或另一原生管理器重放。依赖区段也在调用前固定：保留已有 production、
optional、peer 或 development 位置，缺失包默认作为开发依赖。所有项目 add 路径都禁用
lifecycle scripts。

## 真实项目发布与激活

项目安装器成功后，osdk 校验已安装包的名称与精确版本，解析其声明的 bin 条目，将每个
规范化目标限制在包目录内，并检查包管理器 launcher。原始 `node_modules/.bin` 不会加入
PATH；osdk 会改为构建不可变的筛选 generation，其中只包含项目配置所选 npm 包声明的
bin：

```text
<project>/.osdk/npm-bin/
  current
  publish.lock
  generations/<sha256>/
    manifest.json
    bin/<激活使用的筛选 launcher>

<project>/node_modules/<configured-package>/<declared target>
<project>/node_modules/.bin/<仅供校验、永不激活的源 launcher>
```

generation ID 是 schema、平台、排序后的选择与 bin 记录的 SHA-256。Unix 上的筛选条目
是指向规范声明目标的相对软链接；Windows 上则是调用受管 Node、经过约束的 `.cmd`
wrapper。所选包之间出现重复命令名时 fail closed（Windows 不区分大小写）。generation
先在 staging 目录构建再 rename；JSON `current` 指针通过临时文件原子替换。只有配置 spec
仍匹配的旧选择才会被带入新 generation。后续事务失败会恢复原指针；已完成但未被引用的
generation 可能保留，目前没有 stale-generation 垃圾回收。

发布后，osdk 原子更新 `osdk.toml` 中的精确 Node 与结构化 npm 选择，把紧凑的原生 lock
metadata 写入项目 `osdk.lock`，并信任刚生成的配置。这条分支不会创建 osdk 私有 npm
工具安装。`.osdk/npm-bin/` 是本地派生状态，通常应在 package 根使用
`/.osdk/npm-bin/`，或为嵌套 workspace package 使用 `**/.osdk/npm-bin/` 忽略，而不是
忽略将来 `.osdk` 下所有可能的文件。

每次 Shell 激活时，osdk 首先要求 npm 选择来自已信任项目配置，且配置与最近的普通
`package.json` 位于同一规范根。随后重新校验 `current` 指针、schema/平台与内容派生的
generation 身份、自有非软链接目录、精确文件集合、配置 spec、已安装包身份与版本、声明
目标及每个筛选 launcher。状态缺失或无效时会从激活增量中静默省略。有效筛选 bin 目录
会位于 osdk shim 与受管运行时之前；若其中包含 `node` 命令，则整个 generation 都会省略。

## 隔离与全局 Aube 执行路径

隔离的 `install` 与 `exec` 继续通过
[`npm_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/npm_package.rs)
和
[`aube_host.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/aube_host.rs)
调用 Aube embedded library API。该兼容路径为每个 package/version 建立一个 osdk 私有合成
项目，并关闭 Aube 的 runtime switching、self engine check 与 self-update；Node、版本选择和
生命周期编排由 osdk 管理。

全局 `use` 采用另一条路径：定位与 `osdk` 同目录安装的 `osdk-aube`，并在辅助进程中执行
Aube 真正的 `add --global --save-exact`。Aube 全局命令会占用工作目录和进程级设置，因此
需要进程隔离。辅助程序接收 osdk 自有的 home、配置、全局前缀、bin、禁用的 runtime 目录，
以及共享的 Aube cache/store：

```text
<global-install-staging>/
  aube-home/
  aube-global/global-aube/...    # Aube 原生全局输出
  bin/                           # Aube 原生 launcher

<cache>/aube/v1/cache/
<store>/aube/
```

辅助进程退出后，osdk 在 Aube 原生 `global-aube` 树中定位选中的根包，并把该安装移动到
规范的 `<global-install>/project` 布局。随后删除临时 Aube home/global/runtime 目录，清空
原生 bin 目录，只根据选中包声明的 `bin` 重建可迁移 launcher。staging 根提升和 shim 发布
之前，还会校验包身份、精确版本、目标路径边界、每个 launcher、Aube 原生 lock、inventory
与完成状态。同目录缺少 `osdk-aube` 会作为安装错误返回，不会回退到其他模式。

Aube cache/store 会跨隔离、项目和全局操作共享，但每个项目或全局安装仍保留自己的原生
lock。Aube 2.1 的全局调用不会收到 offline flag，因此在需要新建或修复安装时，
`--offline` 会在 Registry 探测或辅助进程启动前明确失败；完整、精确且选项身份匹配的安装
可以在不调用 Aube 的情况下离线复用。确实需要安装时，npm 与 pnpm 的全局委托会传递各自
的原生 offline flag。

## Source 自动选择与缓存键

动态 npm backend 复用 npm CLI backend 的 artifact sources：默认候选为 npmmirror 和
npmjs 官方 Registry。它们经过通用 [`ranked_source_list`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs)：

1. pin 优先，并保留其余来源作为失败回退；
2. `ordered` 按 priority；
3. 默认 `auto` 并发探测，吞吐优先、TTFB 为次要因子；
4. 探测结果按 `cache_ttl` 复用，`--refresh-sources` 强制刷新。

source probe cache 使用 schema 2，并保存候选集合的 BLAKE3 指纹。指纹覆盖顺序、ID、
kind、index/download URL、priority、enabled、credential-forwarding 和 header 名；header
值只写摘要。因此改变候选不会沿用旧排名，凭据也不会明文写入 cache。离线模式可用
兼容的已缓存排序；没有兼容 cache 时保留静态顺序，但不会进行 probe。

osdk 自己发起的 npm metadata 请求和 source probe 会使用显式 `Source.headers`，但只发
给配置的 index/download origin；同源 redirect 保留 header，第一次跨源 redirect 后永久
移除。Aube 2.1 无法通过这些集成路径安全接收任意 source header，因此 Aube 真正获取
package 时不转发 `Source.headers`。全局受管工具安装目前会拒绝原生认证、scope、私有、
自定义 TLS 或代理 Registry 的透传，因为在不扩大凭据边界的前提下无法把这些状态复制进
隔离前缀；该路径请使用可匿名访问的已配置 Registry。`package_registry.rs` 的 Registry
preflight 针对用户随后运行的
npm/pnpm/Yarn/Bun/Deno 命令，每次调用独立判断，不能与这里的 TTL cache 等同。

## 构建脚本策略与结构化配置

对隔离和全局安装，默认 `BuildPolicy::Deny` 都会禁用 root 与传递依赖的 lifecycle/build
scripts。embedded 路径设置 `ignore_scripts = true`。Aube 2.1 在构造内部 global-add request
时会丢弃该标志，因此全局辅助程序改传 `--deny-build=*`，覆盖 Aube 内置的可信依赖列表。
`allow_builds` 从 CLI 字符串或结构化 `[tools]` 读取：

- false 值或空值仍为 deny；
- 包名数组在进入 request 时转成逗号列表，再写入 embedded 合成项目的
  `package.json#aube.allowBuilds`，或为全局 Aube 转成重复的 `--allow-build`；
- true 值在 embedded 路径设置 `dangerously_allow_all_builds`，或在全局路径传入
  `--dangerously-allow-all-builds`，是显式危险的全图放行。

`osdk lock` 的 graph-only 阶段无论最终安装策略如何都设置 `ignore_scripts = true`、
`run_root_lifecycle = false` 和 `lockfile_only = true`。结构化工具项支持字符串、布尔值
与字符串数组；合并配置时，更高优先级文件的同名工具项整体替换较低层条目，不逐字段
继承。`use -o allow_builds=esbuild,sharp` 会规范化并持久化为字符串数组，true/false
则持久化为布尔值，使生成的项目配置继续保持结构化。

## lock schema 4、兼容 npm metadata 与原生依赖图所有权

`osdk.lock` 的当前写入格式是 lock schema 4，并保留 schema 3 的 npm metadata 模型。
每个 npm 工具记录 request、精确 version、
options 和 `npm` 元数据；当前不写通用 `artifact` 子表，也不把 graph payload 或路径写进
主 lock：

```toml
[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
installer = "aube"
scope = "project"
node_version = "24.1.0" # 可省略

[platforms.linux-x64.tools."npm:prettier".npm.native_lock]
kind = "aube"
format = "aube-v9"
sha256 = "<64 lowercase hex characters>"
```

公开的 `installer` 与 `allow_builds` 选项仍保存在 lock 的 `options` 表，并在读取时重新注入
请求；内部 `__osdk_*` metadata 不会写入该表。这与下文 `.osdk-install.json` schema 1 是两个
独立格式。本节所说的旧“schema 2 sidecar”专指 lock schema 2 的 npm graph sidecar，不是动态
安装身份格式。

写入时，CLI 会从已安装工具或声明的私有 option 中提取 npm 元数据：包名、installer、
scope、可选精确 Node 版本，以及可选 native lock 的 owner/format/SHA-256。项目感知的
`use` 对真实 `package.json` 旁的原生 lock 计算摘要。原生 lock 始终由所选包管理器操作
拥有；当 Aube 消费兼容的现有 npm 或 pnpm lock 时，其 `kind` 因此可能与具体 installer
不同。全局 Aube 和 pnpm 把原生 lock 保留在受控安装根内，并将其身份记录到用户 lock；
npm 的真实全局模式不会创建依赖 lock，因此没有该身份。原生 payload 本身不会进入
`osdk.lock`。主 lock 当前限制为 16 MiB，写入时只原子替换主 lock。

这是有意保留的限制：兼容的 npm metadata 不捕获传递依赖图，单靠它无法重建该图。真实
项目的原生 lock，或受控全局安装目录中的 Aube/pnpm 原生 lock，仍是依赖图事实来源。
npm 全局安装没有对应的 graph lock。

无参数 `osdk install` 读取 schema 3 或 4 lock 后，会把这些字段重新注入私有 option，并先校验
package/backend、一致的 installer/scope、可选的同平台 Node 精确版本，以及可选
native lock 的 format/SHA-256 是否满足 owner 的格式约束。主 lock 不再提供 graph/path，
因此这里恢复的是 metadata，而不是 sidecar 路径。

不含 npm 条目的 schema 1 lock 可正常读取，并在下一次成功写入时升级。含任意
`npm:*` 的 schema 1 lock（包括旧 inline graph）不能消费、merge 或保存；必须重新生成，
不能假装安全迁移成当前 metadata-only 格式。

旧 lock schema 2 sidecar 仍保持冻结读取兼容：读锁时如果遇到旧 sidecar 形式，osdk 会继续校验
`package`、Node 版本、`aube-v9`、64 位小写 SHA-256、规范 sidecar 路径，以及 sidecar
目录/文件非 symlink，再以 16 MiB 上限读取完整 UTF-8 字节并重算摘要。校验通过后，
graph 内容会作为兼容输入注入 backend。只有在后续成功写入主 lock 时，条目才迁移成
lock schema 4 metadata-only 形式；原有 sidecar 文件不会被自动删除。

## 隔离/全局安装身份、shim 与冲突拒绝

隔离安装会扫描合成项目完整的 `node_modules/.bin`，因此记录的 bin 可能来自根包或传递
依赖。全局安装则会在规范化时重置包管理器生成的 bin 目录，只为选中根包声明的 bin 重建
launcher。两条路径都会写入 `.osdk-install.json` schema 1。其嵌套 `identity` 包含 `tool`、
精确 `version`、`platform`、`scope`、规范 `material_options`、`dependencies`、`materials` 与
`install_id`；`install_id` 是带 domain separation 的规范 `b3-v2:` 身份，也用于物理安装根。
相对可执行文件路径会针对该根校验；graph integrity、native-lock hash 等 backend-specific
观测值写入独立 receipt，且不会成为 alias ownership。bin 名必须是单一文件名，解析后的
canonical target 必须仍在根内；缺失 bin、重复名称、路径穿越或身份被篡改都会拒绝。扫描
不跟随符号链接，并对深度、数量和文件大小设限。

动态安装根位于 backend 与精确版本之下，并带身份指纹；隔离与全局 npm 仍属于不同
namespace。因此，同一 scope 中相同 backend/version 的多个身份可以共存。复用及 lifecycle
命令会先派生精确身份，绝不会把另一个 fingerprint 当作版本兼容回退。

`.osdk-tool.json` 只作为遗留状态识别 metadata。其旧 schema 1 与 schema 2 都不能授权复用、
activation、shim 执行、`where`、uninstall 或 `reshim`；这些动态 inventory schema 之间没有
兼容契约。遗留安装必须重新安装，发布 `.osdk-install.json` schema 1。

CLI 和 shim 根据精确安装身份记录建立 `bin name -> backend owner` 映射。对活跃动态请求，
复用、activation、shim 分发、`where`、uninstall 与 `reshim` 只选择完整 `b3-v2:` 身份与请求
匹配的指纹化根。身份缺失、过旧或不匹配都会 fail closed，不暴露 bin 路径，也不会选择同版本
的其他根。
当前 schema 4 lock bridge 中，精确受管 Node 依赖属于 install ID；只有作为安装前输入存在的
旧冻结 graph digest 才参与路径。紧凑 native-lock hash 在 lock 重放与全新安装后的注入形式相同，
因此它仍是需要校验的 receipt evidence，而不是路径选择器。未锁定安装后观察到的
graph/SRI 数据采用同样规则。

bin owner 判定是另一项独立
检查：

- 一个 owner 时直接路由；
- 运行时多个 owner 但当前配置只选中一个时，路由到该 owner；
- 运行时仍有多个候选时拒绝任选一个；CLI 生成/重建 shim 则始终按多个已安装
  backend owner fail closed，移除歧义的受管 shim 并报错；
- 同一 backend 的多个版本不是 owner 冲突，由当前目录的版本选择决定；
- Node 与独立 npm backend 对 `npm`/`npx` 的协作是唯一特例。

因此 fail closed 的边界是 **shim 发布与命令路由**。包内容可能已经成功物化到安装
目录；冲突不会把这一步描述成已回滚的全局安装事务。

## 主要验证点

相关单元与契约测试覆盖 namespaced/scoped parser、选项规范化、规范 `b3-v2:` 安装身份、
schema 1 身份校验、安装器规划、依赖区段保留、原生委托只执行一次、紧凑 lock metadata、
全局前缀参数、原生 lock 身份、筛选 generation 发布与重新校验、原始项目 bin 排除、
安装扫描和 shim 冲突行为。兼容性测试继续覆盖旧 lock schema 2 sidecar 校验、旧
`.osdk-tool.json` 只识别不执行，以及 lock schema 1 npm 迁移拒绝边界。跨平台行为仍需按仓库要求运行
Linux workspace 测试与完整 Windows GNU Wine 套件。
