# npm 开发工具实现

本页描述 `npm:<package>` 动态 backend 的内部边界。用户命令和配置示例见
[npm 开发工具](../npm-tools)。它与固定 npm CLI 的 `npm` backend、项目依赖命令的
Registry 预检是三个相邻但不同的概念。

## 身份、解析与生命周期编排

[`ToolRequest::parse`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs)
会先识别 `npm:`。普通包在包名后的 `@` 分隔版本；scoped 包先解析
`@scope/name`，再把其后的第二个 `@` 作为版本分隔符。请求解析阶段保留输入的包名
大小写，inventory 身份会规范为小写；为避免二者不一致，当前应使用 npm 常规的小写
包名。空包名、只有 scope、多余路径层级、反斜杠、冒号或空白会被拒绝。裸 `npm`
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

## Embedded Aube 安装

[`npm_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/npm_package.rs)
为每个 package/version 建立 osdk 私有的合成项目；
[`aube_host.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/aube_host.rs)
以 library API 嵌入 Aube，而不是生成 `npm install -g` 子进程。host 关闭 Aube 的
runtime switching、self engine check 和 self-update，由 osdk 管理 Node、版本和生命周期。

安装目录和缓存逻辑上按 canonical backend 与版本隔离：

```text
<installs>/npm/<package>/<version>/
  project/package.json
  project/aube-lock.yaml
  project/node_modules/.bin/...
  .osdk-tool.json
  .osdk-complete

<cache>/aube/npm/<package>/<version>/cache/
<cache>/aube/npm/<package>/<version>/store/
```

实际路径会经过平台安全的 tool-id 转换，scoped 包因此继续形成嵌套目录。只有包目录、
`.bin` 目录、可解析的导出命令和 inventory 都写好后，安装才发布
`.osdk-complete`。失败路径会尽力清理未完成的安装根。根包 metadata 必须提供可解析的
SHA-256 或 SHA-512 SRI，否则拒绝安装；完整传递依赖的 integrity 由 Aube 图管理。

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
移除。Aube 2.1 的 embedded API 无法安全接收任意 source header，因此 Aube 真正获取
package 时不转发 `Source.headers`；需要认证的 Registry 应使用 Aube/npm 原生且可信的
配置或环境变量路径。`package_registry.rs` 的 Registry preflight 针对用户随后运行的
npm/pnpm/Yarn/Bun/Deno 命令，每次调用独立判断，不能与这里的 TTL cache 等同。

## 构建脚本策略与结构化配置

默认 `BuildPolicy::Deny` 向 Aube 传入 `ignore_scripts = true`，因此 root 与传递依赖的
lifecycle/build scripts 都不会运行。`allow_builds` 从 CLI 字符串或结构化 `[tools]`
读取：

- false 值或空值仍为 deny；
- 包名数组在进入 request 时转成逗号列表，再写入合成
  `package.json#aube.allowBuilds`；
- true 值设置 Aube 的 `dangerously_allow_all_builds`，是显式危险的全图放行。

`osdk lock` 的 graph-only 阶段无论最终安装策略如何都设置 `ignore_scripts = true`、
`run_root_lifecycle = false` 和 `lockfile_only = true`。结构化工具项支持字符串、布尔值
与字符串数组；合并配置时，更高优先级文件的同名工具项整体替换较低层条目，不逐字段
继承。`use -o allow_builds=esbuild,sharp` 会规范化并持久化为字符串数组，true/false
则持久化为布尔值，使生成的项目配置继续保持结构化。

## schema 2、Graph Sidecar 与冻结恢复

`osdk.lock` 的当前写入 schema 是 2。每个 npm 工具记录 request、精确 version、
options 和 `npm` sidecar 元数据；当前不写通用 `artifact` 子表：

```toml
[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
node_version = "24.1.0"
lock_format = "aube-v9"
sha256 = "<64 lowercase hex characters>"
graph = "osdk.lock.d/npm/<sha256>.yaml"
```

`osdk lock` 对 npm 工具不是纯 metadata 解析：CLI 先确保受管 Node 已安装，再调用
`prepare_lock_graph`，由 Aube 的 lockfile-only 模式解析完整依赖图并使用隔离的
cache/store。安装目录中 `aube-lock.yaml` 的原始 UTF-8 字节按 SHA-256 寻址，原子写入
`osdk.lock.d/npm/<sha256>.yaml`；主 lock 只保存上面的五个字段。单个 graph sidecar 和
主 lock 当前都限制为 16 MiB。sidecar 写完后会先重新读取并校验，最后才原子替换主
lock。`osdk.lock` 与 `osdk.lock.d/` 必须一起提交到仓库。

无参数 `osdk install` 读取当前平台 lock 后，先检查 package/backend、同平台 Node 精确
版本、`aube-v9`、64 位小写 SHA-256 和由摘要唯一决定的路径；再拒绝 sidecar 目录或
文件中的 symlink，以 16 MiB 上限读取完整 UTF-8 字节并重算摘要。校验通过后，graph
内容才作为私有 option 交给 backend，恢复成 `aube-lock.yaml` 并执行 Aube frozen install。

不含 npm 条目的 schema 1 lock 可正常读取，并在下一次成功写入时升级。含任意
`npm:*` 的 schema 1 lock（包括旧 inline graph）不能消费、merge 或保存；必须重新生成，
不能假装安全迁移成 sidecar 格式。

offline 需要三部分同时存在：有效的 schema 2 主 lock 元数据、已提交且校验通过的 graph
sidecar，以及包含 graph 所引用 package 的 Aube cache/store；同平台锁定的受管 Node 也
必须可用。缺任何一项都会失败且不回退网络。已有安装只有通过完整 identity/layout
校验才会复用，而不是只检查 `.osdk-complete`。

## Inventory、shim 与冲突拒绝

安装完成前，backend 扫描合成项目的整个 `node_modules/.bin`，将 tool id、精确版本、
相对 bin 路径和稳定 metadata 写入 `.osdk-tool.json`；这些 bin 可能来自根包或传递依赖。
bin 名必须是单一文件名，解析后的 canonical target 必须仍在安装根内；没有任何 bin、
重复名称、路径穿越或损坏 inventory 都会拒绝。inventory 扫描不跟随符号链接，并对
深度、数量和文件大小设限。

CLI 和 shim 根据 inventory 建立 `bin name -> backend owner` 映射：

- 一个 owner 时直接路由；
- 运行时多个 owner 但当前配置只选中一个时，路由到该 owner；
- 运行时仍有多个候选时拒绝任选一个；CLI 生成/重建 shim 则始终按多个已安装
  backend owner fail closed，移除歧义的受管 shim 并报错；
- 同一 backend 的多个版本不是 owner 冲突，由当前目录的版本选择决定；
- Node 与独立 npm backend 对 `npm`/`npx` 的协作是唯一特例。

因此 fail closed 的边界是 **shim 发布与命令路由**。包内容可能已经成功物化到安装
目录；冲突不会把这一步描述成已回滚的全局安装事务。

## 主要验证点

相关单元与契约测试分别覆盖 namespaced/scoped parser、构建策略、sidecar 身份/格式/
摘要与精确字节 round trip，以及 missing、tampered、oversized、symlink sidecar 的拒绝；
还覆盖 schema 1 npm 迁移拒绝、无 graph 的离线拒绝、bin 路径约束、inventory 扫描、
shim 运行时 Node 注入和同一动态 backend 的多版本 reshim。跨平台行为仍需按仓库要求
运行 Linux workspace 测试与完整 Windows GNU Wine 套件。
