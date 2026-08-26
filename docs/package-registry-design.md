# 项目依赖 Registry 选择：实现与边界 / Project Dependency Registry Selection

> 状态：本文记录当前实现的启动前 Registry 选择、兼容性边界，以及尚未实现的共享 tarball CAS 研究方向。
>
> Status: this document describes the implemented pre-start registry selection and compatibility boundaries, plus the future shared-tarball CAS research that is not implemented.

## 1. 问题与非目标

one-sdk 需要区分两个名字相近、但信任边界完全不同的概念：

| 概念 | 负责什么 | 示例入口 | 不负责什么 |
| --- | --- | --- | --- |
| 工具自身的 download source | `osdk` 获取 Node、npm、pnpm、Yarn、Bun、Deno 等工具二进制或归档时所用的上游和镜像 | `[sources.<tool>]`、`osdk source ...`、`osdk --source ... install ...` | 不控制项目的 `package.json` / `deno.json` 依赖从哪里下载 |
| 项目依赖 registry | 已安装的 npm、pnpm、Yarn、Bun 或 Deno 在解析、下载项目依赖时使用的 npm-compatible registry | `[registries.npm]`、`osdk registry test [manager]`、向单次子进程注入 manager-native 环境变量 | 不改变 `osdk` 自己下载 manager 可执行文件的 source |

例如，`osdk install pnpm@11` 可以通过 `sources.pnpm` 从一个镜像下载 pnpm 本身；之后执行 `pnpm install` 时，项目包的 registry 选择应由 `registries.npm` 和 pnpm 的原生配置共同决定。这两条链路必须分离，避免 `--source` 意外改写项目依赖、私有 scope 或认证流量。

当前实现只做**启动前选择**：在可能访问 npm registry 的顶层命令启动前，以匿名请求检查候选端点，选择一个健康端点，然后原样启动包管理器一次。它不是完整 registry proxy，也不会：

- 解析、修改或生成依赖 lockfile；
- 接管依赖解析、生命周期脚本或 `node_modules` 布局；
- 把五个 manager 的原生缓存目录合并成一个目录；
- 在 manager 已经运行后切换 registry 或重放命令；
- 代理、复制或推断用户的 registry 凭据。

## 2. 配置

在受信任的项目 `osdk.toml` 中使用独立顶层配置：

```toml
[registries.npm]
urls = [
  "https://registry.npmmirror.com/",
  "https://registry.npmjs.org/",
]
probe_timeout_ms = 1500
```

`urls` 是**策略顺序**，不是测速后可重新排序的集合。若多个已配置端点都健康，选择数组中最靠前的一个；后面的端点仅作为启动前健康回退。项目层的整段配置应覆盖用户全局层，避免把组织或项目策略与个人候选列表隐式拼接。显式 `urls` 存在时，它就是完整候选集合：只探测该列表，不加入识别出的原生公共默认源，也不追加未声明的内置端点；但原生配置中的私有、scope、认证、TLS 或代理信号仍会优先触发保守透传。未配置 `urls` 时，识别出的匿名公共默认 registry 保持第一优先级，随后保留内置公共回退。只有纯内置候选集才按本次探测延迟选择最快健康项。

URL 应满足以下约束：

- 仅允许带 host 的 `https://`（兼容测试或显式内网场景时可以允许 `http://`）；
- 规范化尾部 `/` 并去重，但保持首次出现的顺序；
- 不在 URL 中保存用户名、密码、token、query 或 fragment；
- 只配置允许 osdk **匿名探测**的端点。需要身份认证的端点放在 manager 原生配置中。

因为 `[registries]` 能改变子进程的网络目的地，它属于 execution-affecting project configuration，应沿用 one-sdk 的项目配置 trust 流程。

## 3. 五类 manager 的命令与环境变量

下表是 MVP 的识别和注入契约。Yarn 是一个 manager family，但 Classic 与 Berry 的原生变量不同，因此拆成两行。命令集合应采用明确 allow-list；普通的 `--version`、`config get`、`help` 等命令不应为了 registry 产生网络探测。

| Manager | 可能触发依赖解析/下载的顶层命令 | 选择后仅向该子进程注入 | 说明 |
| --- | --- | --- | --- |
| npm | `npm install/i/ci/add/update/up/exec`，以及 `npx` | `npm_config_registry` | 同时识别用户显式的大小写环境变量写法；不覆盖已有值 |
| pnpm | `pnpm install/i/add/update/up/fetch/dlx/deploy`，以及 `pnpx` | `pnpm_config_registry` | 已在 pnpm 11.21 实机验证；`npm_config_registry` 在该版本不生效。旧版本兼容行为可能变化，不能把 npm 变量描述为 pnpm 的通用接口 |
| Yarn Classic 1.x | `yarn/yarnpkg install/add/upgrade/create` 等 allow-list 命令 | `YARN_REGISTRY` | 已在 Yarn 1.22.22 实机验证；`npm_config_registry` 在该版本不生效 |
| Yarn Berry 2+ | `yarn/yarnpkg install/add/up/dlx` 等 allow-list 命令 | `YARN_NPM_REGISTRY_SERVER` | 必须先可靠确定 Yarn major；无法确定时保守透传 |
| Bun | `bun install/i/ci/add/update/x`，以及 `bunx` | `BUN_CONFIG_REGISTRY` | `ci` 等价于 frozen-lockfile 安装，但仍会补齐缺失缓存；Bun 也读取 `.npmrc`，官方 CLI 的 `--registry` 优先级更高 |
| Deno | 可能解析 npm 包的 `deno add/bench/cache/check/ci/compile/doc/eval/info/install/outdated/run/serve/task/test/update` | `NPM_CONFIG_REGISTRY` | 只影响 npm registry；不替换 JSR 或普通 `http(s):` module URL |

`osdk registry test [manager]` 是诊断入口：指定 manager 时显示该策略的匿名探测与最终选择；省略时检查所有支持的 manager。对于没有可判定 major 的 `yarn`，诊断可以分别显示 Classic 和 Berry 策略，但实际执行路径不得猜测。该命令只做 HTTP 探测，不运行 `install`。严格离线只按 manager 真正支持的参数识别：npm/pnpm/Yarn/Bun 的 `--offline`，以及 Deno 支持该参数的命令上的 `--cached-only`；`--prefer-offline`、`--immutable-cache`、`--frozen-lockfile` 和 `deno ci` 的 `--frozen` 都仍可能联网，不会跳过预检。Deno 没有通用的 `--offline` 参数，且当前 `ci/outdated/update` 也不支持 `--cached-only`。

## 4. 启动前选择算法

一次顶层调用的目标流程如下：

1. 识别实际将运行的 manager、版本族和子命令。非 allow-list 命令直接透传。
2. 检查显式 CLI、环境变量和原生配置。只要命中下面的透传条件，就不探测、不注入。
3. 对有效候选执行全新的、有超时和响应大小上限的**匿名**探测。可并发探测以控制总等待时间；显式配置或识别出的原生公共候选严格保持策略顺序，不按延迟重新排名。只有纯内置候选集按本次延迟排名。
4. 按上述规则选择健康端点，把上表中的一个原生环境变量只注入即将启动的子进程。
5. 启动原始 manager 命令**恰好一次**，完整继承 stdin/stdout/stderr，并透传其退出状态。
6. 如果所有候选都不健康，则在启动 manager 前失败，并展示每个候选的有界、脱敏诊断。

实现会探测 npm-compatible registry 的标准轻量 endpoint `<base>/-/ping`，要求成功 HTTP 状态、非空且不超过 64 KiB 的 JSON object。探测不得调用 manager 的 `install` 命令，不得携带 registry token、cookie 或从原生配置提取的 Authorization header，也不得执行响应中的任何内容。普通 HTTP(S) proxy 环境变量会沿用，以免在必须通过企业代理联网时把可用 manager 阻断；manager 原生配置中无法安全复现的代理或 TLS 策略则触发保守透传。redirect 最多三次，并且每一跳必须保持原始 HTTPS origin，禁止降级、跨 origin、URL credential 与循环。

### 为什么运行中不重试

manager 启动以后绝不因为失败而换源重跑。一次安装可能已经：

- 写入或部分写入 lockfile、`node_modules`、virtual store 或全局 cache；
- 执行任意第三方 lifecycle script；
- 运行 workspace hook、生成代码或修改项目文件；
- 发布包、改变 tag，或执行其它不能安全重放的操作。

因此“失败后用下一个 registry 再执行一遍”既不满足 exactly-once，也无法证明幂等。MVP 的故障转移只能发生在 manager 启动之前。运行中的网络错误由 manager 自己处理；osdk 原样返回失败。

## 5. 保守透传：用户和原生配置优先

以下任一条件成立时，osdk 必须保持 pass-through：不执行自动探测，不覆盖 registry 环境变量，也不改写参数或配置文件。

- 命令行已经带有该 manager 支持的显式 registry 参数，例如 `--registry URL` 或 `--registry=URL`；
- 进程环境已经显式设置对应 manager 的 registry 变量；兼容别名只能用于识别“用户已有选择”，不能据此声称不同版本一定会消费该别名；
- `.npmrc`、`.yarnrc`、`.yarnrc.yml`、`bunfig.toml` 等原生配置声明了私有或未知 registry；
- 原生配置声明了任何 scoped registry；
- 原生配置或相关环境变量包含 token、basic auth、username/password、`always-auth`、mTLS 或其它认证信号。

这条规则的目的不是判断原生配置是否“正确”，而是避免 osdk 把默认 registry 注入置于用户的 scope/auth 规则之上，或把私有端点当作匿名公共镜像来探测。原生 manager 仍然按照自己的优先级、证书、代理、scope 和 credential 规则运行。osdk 不读取 token 值，不在日志中输出 token，也不把凭据转发到候选镜像。

已知且匿名的公共默认 registry 可以参与启动前选择；一旦不能可靠证明“匿名 + 公共 + 无 scope/auth”，就退回原生行为。安全上宁可不优化，也不能误送凭据。

## 6. Lockfile 与绝对 tarball URL 边界

默认 registry 的环境变量只能影响 manager 愿意通过默认 registry 解析的请求，不能保证接管所有依赖流量。项目中可能存在：

- 直接写在 manifest 中的 `https://host/package.tgz` 依赖；
- lockfile 中的绝对 `resolved` / `tarball` URL；
- registry metadata 返回的指向另一 host 的绝对 `dist.tarball`；
- Git、GitHub、JSR、workspace、file 和任意 HTTP module 依赖。

这些 URL 可能被 manager 按原值请求，从而绕过选中的默认 registry。不同 manager、lockfile 版本和诸如 npm `replace-registry-host` 的设置会产生不同结果，不能作跨 manager 保证。MVP 不解析或重写 lockfile，也不做透明网络拦截，因此：

- “registry 健康”只表示匿名 metadata probe 成功，不表示 lockfile 中每个绝对 artifact URL 都健康；
- 选择 fallback 不保证所有 tarball 都来自 fallback host；
- 已固定到失效绝对 URL 的安装仍可能失败，而且 manager 只运行一次；
- 绝不能为了提高命中率静默改写并提交 lockfile。

各 manager 的已知差异进一步说明这里不存在通用改写规则：

- npm 的 `package-lock.json` 用 `resolved` 记录 tarball 位置、用 `integrity` 记录 SRI；其中 `registry.npmjs.org` 是表示“当前配置 registry”的特殊 host。默认 `replace-registry-host=npmjs` 只替换默认 npm registry host，`never` 保留原 host，`always` 才替换所有 registry host；`omit-lockfile-registry-resolved=true` 可以省略 registry dependency 的 `resolved`。因此自定义或非规范绝对 URL，尤其随机 `127.0.0.1:<port>`，仍可能粘在 lockfile 中。
- pnpm 默认 `lockfileIncludeTarballUrl=false`。规范 registry tarball URL 可以省略并由当前 registry、包名和版本重建；非规范 URL 仍可能保留，显式开启该选项也会保留 tarball URL。
- Yarn Classic 的 `yarn.lock` 记录并使用绝对 `resolved` URL，本地 loopback 地址会成为不可移植的持久内容。Modern Yarn 通常以 npm locator 表示规范 npm archive，但非规范 `dist.tarball` 可能成为带 `__archiveUrl` 的绑定引用并被持久化；必须用固定版本 fixture 证明实际行为。
- Bun 与 Deno 的 canonical/noncanonical URL lock serialization 不在本设计中作保证；在固定版本 smoke fixture 或源码审计完成前，它们都是 proxy 上线的显式阻塞项。

## 7. 五种 native cache 格式

下表描述 manager 自己拥有的缓存格式。`<cache>/pkg/<manager>` 只是建议的隔离根目录；表内映射是否已在某个 release 提供，应以该 release 的 `osdk cache env` 和测试为准。

| Manager family | 原生设置（建议隔离路径） | 代表性磁盘格式与 key | 为什么不能直接给其它 manager 使用 |
| --- | --- | --- | --- |
| npm | `npm_config_cache=<cache>/pkg/npm` | npm 当前的 `cacache` 实现中，`_cacache/content-v2` 按 SRI digest 寻址响应 bytes，`index-v5` 则把逻辑请求 key（HTTP fetch 通常来自规范化 URL）映射到 integrity、大小、时间和 HTTP metadata | npm 明确把 `_cacache` 定义为 opaque、可丢弃的 cache；路径、index schema、GC 和校验流程都不是公共 API，更不是通用 registry 文件树 |
| pnpm | 版本相关的 `npm_config_store_dir` 或 `pnpm_config_store_dir=<cache>/pkg/pnpm-store`；`PNPM_HOME` 是另一类全局可执行文件/状态目录 | 版本化 store 把**解包后的单个文件**按内容 hash 保存，再用 package index metadata 组装包。pnpm 11 当前使用 SHA-512 派生的 `files/...` 路径及 SQLite `index.db`（值为 MessagePack），这些都是版本相关实现细节 | 粒度是 package file 而非原始 `.tgz`，并依赖 store version、package index 和链接语义；store 与安装目录跨文件系统时不能依赖 hardlink，可能退化为 copy |
| Yarn | Classic：`YARN_CACHE_FOLDER=<cache>/pkg/yarn-classic`；Berry：`YARN_GLOBAL_FOLDER=<cache>/pkg/yarn` | Classic 的版本化 native cache 主要是展开后的 package tree；其 offline mirror 才另外保存可复用原始 `.tgz`。Modern Yarn 把 registry tarball 规范化为 `.zip`；逻辑 `cacheFolder` 默认是项目 `.yarn/cache`，当前启用 global cache 时会指向 `<globalFolder>/cache`，所以 archive 不直接位于 `YARN_GLOBAL_FOLDER` 根目录 | Classic、offline mirror 和 Modern Yarn 不是同一格式；Modern Yarn 的 cache version、压缩、checksum、filename、locator 和 Zero-Install 语义也不能当作稳定 registry tarball API |
| Bun | `BUN_INSTALL_CACHE_DIR=<cache>/pkg/bun` | 官方保证的边界是 cache root 和类似 `${name}@${version}` 的展开目录（prerelease/build suffix 会被编码），再以 hardlink、clonefile 或 copy 等方式物化到 `node_modules`；其它 binary metadata 布局只应视为当前实现细节 | 这是 Bun 管理的展开目录和 metadata，不是 npm `cacache`、pnpm file store 或 Yarn zip |
| Deno | `DENO_DIR=<cache>/pkg/deno` | `DENO_DIR` 是混合 runtime cache/state root，供 npm package、远程 module、编译/分析产物及部分运行时状态使用；精确子目录和数据库布局属于版本相关实现 | 不只是 npm tarball cache；本地 `node_modules` 模式也会改变 npm 物化位置，且 Deno 的 JSR/任意 URL import 不属于 npm registry proxy |

表中的具体目录和 schema 只用于解释“不兼容”的原因，不构成 one-sdk 或上游 manager 的稳定 API。实现和测试应使用各 manager 的受支持配置入口，而不是直接依赖内部文件名。

### 为什么当前只能“各自共享”，不能“跨 manager 共享”

将同一 manager 的原生 cache 放在稳定路径，可以让不同项目和该 manager 的多个受管版本复用下载。但五类格式在以下维度不兼容：

1. **存储单位不同**：原始响应 blob、解包后的单文件、规范化 zip、展开目录和混合 module cache 都存在。
2. **身份 key 不同**：URL/cache key、SRI digest、文件 hash、package locator、registry host + name + version 不能互换。
3. **metadata 不同**：每个 manager 有自己的索引、lock、校验、side-effects 和 schema version。
4. **物化语义不同**：copy、hardlink、reflink、symlink、PnP 与 virtual store 对目录结构和可变性有不同假设。
5. **并发与 GC 不同**：manager 只理解自己的锁、临时文件、引用关系和 prune 规则。

因此不能把 npm、pnpm、Yarn、Bun、Deno 指向同一个物理目录，也不能因为两个缓存都使用 SHA-512 就认为 blob 可互换。这会带来 false hit、损坏、权限问题或缓存投毒。正确的当前模型是每个 manager 在自己的 namespace 内复用。

## 8. 未来：SRI 原始 tarball CAS + loopback proxy

跨 manager 去重的可行方向，是在原生 cache **下方**增加一个只保存 registry 原始 tarball bytes 的共享层，而不是让 manager 直接读取彼此的 cache：

1. 仅从可信 metadata 或 lockfile 取得 `dist.integrity`；
2. 下载原始 `.tgz`，边流式计算 SRI，校验成功后以 `algorithm/digest` 原子写入 CAS；
3. 在 `127.0.0.1` 随机端口提供短生命周期 registry proxy；
4. 仅对已经用固定版本 fixture 证明“不会把临时 loopback origin 写入 lockfile”的 manager，采用该 manager 可支持的 transport indirection；适配 packument 中的 `dist.tarball` 只是可能方案，不是可移植的跨 manager 规则；
5. manager 从受控 endpoint 收到与上游 artifact entity bytes 完全相同且 SRI 一致的 tarball，随后仍生成自己的 native cache。

这只能去重 byte-identical tarball。两个 registry 即使提供语义相同、解包后相同的 package，只要 tar/gzip bytes 或 SRI 不同，就必须保留两个对象；不能通过重新打包来伪造原 integrity。SRI 证明“收到的 bytes 与给定 digest 一致”，不证明发布者身份，也不能在 metadata 和 tarball 同时被攻击者替换时提供 provenance。

在实现前至少需要满足以下安全门槛：

- **仅监听 loopback**，分别按需绑定 `127.0.0.1` 和 `::1`，使用随机端口、精确 Host 校验和每进程不可预测的访问能力；禁止绑定 `0.0.0.0`、`::` 或局域网接口；
- **不做 TLS MITM**，不要求用户安装本地根证书；明确 manager 对 loopback HTTP 的兼容性；
- **上游限制与 SSRF 防护**：只允许 `http(s)` 中的受信 registry，重新校验每次 redirect，处理 DNS rebinding，并限制访问 loopback、link-local、metadata service 和私网地址；
- **凭据与 metadata 隔离**：MVP 先限匿名公共 registry。若未来支持认证，metadata/index 必须按 registry 和 auth identity 分 namespace；Authorization/cookie 绝不落盘、跨 origin 转发或进入日志。强 digest 对应的 bytes 可以全局去重，但 cache hit 不得泄露私有包是否存在，也不能让 registry A 的 metadata 授权 registry B 的对象；
- **完整性先于可见性**：CAS 保存上游 artifact entity 的原始 bytes，绝不规范化或重新打包。定义多 digest 选择和算法降级规则，只接受强 SRI（优先 SHA-512/SHA-256）；完整下载、流式校验、落盘和原子 rename 完成前，不能把未验证 bytes 流给 manager。适配 metadata 时保留 `dist.integrity`，校验 package/version、上游 tarball URL 与 digest 的映射，并定义 HTTP content-decoding 语义，保证 hash 的正是 manager 收到的 artifact bytes；
- **本地 CAS 防护**：目录和文件 owner-only，安全创建临时文件，阻止 symlink/reparse traversal，按 digest 加锁；读取时重新校验，或明确只在受信本地文件系统假设下运行。corrupt hit 必须隔离并修复；若承诺断电耐久性，还要定义 fsync/rename 顺序；
- **资源边界**：限制 metadata/body/tarball 大小、连接数、redirect 数和总时间，处理并发同 digest、崩溃恢复、临时文件清理、磁盘耗尽与有界 GC；
- **协议正确性**：正确处理 scoped package URL encoding、dist-tag、ETag/cache-control、range/HEAD、状态码和 content headers，不能把一个 registry 的 metadata 与另一个 registry 的 tarball 静默拼接；
- **lockfile 可移植性**：不能把随机 `127.0.0.1:<port>` 持久化到 lockfile。需要证明 manager 会保存规范上游 identity，或提供不改写 lockfile 的映射方案；
- **绝对 URL 边界**：已有绝对 tarball URL 不会自然经过 proxy。除非增加经过审计的显式迁移，否则不得改写 lockfile或劫持系统代理/DNS；
- **exactly-once**：proxy 可以在单次 manager 运行中服务多个请求，但 osdk 仍不得以换源为由重新启动 manager；
- **供应链边界**：共享 CAS 不执行 package 内容，也不替代 manager 的签名、provenance、lifecycle-script policy 或审计能力。

在这些条件完成前，loopback proxy 和跨 manager tarball CAS 都只能作为未来研究项，不能在用户文档中描述为现有功能。

## 9. 验收清单

- 五个 manager family 的 registry-fetching 命令在每次顶层调用前做一次有界匿名 preflight。
- 所有候选可并发探测；显式/原生候选选择策略顺序中第一个健康项，只有纯内置候选按延迟选择。
- manager 最多启动一次；全部候选失败时启动次数为零；manager 自身失败时也不重试。
- 显式 CLI/env 与任何 private/scoped/auth native 配置均完整 pass-through。
- 探测请求不含 registry 凭据，日志和错误信息经过脱敏。
- Yarn major 未知时不猜测 Classic/Berry 的变量。
- `osdk registry test` 与执行前选择复用同一 planner，且诊断中英文本完整。
- 有绝对 tarball URL 的 fixture 证明其边界被保留，且 lockfile 不被修改。
- 固定版本 fixture 分别覆盖 npm、pnpm、Yarn Classic/Modern 的规范与非规范 tarball URL；Bun 和 Deno 的 lock serialization 在证明不会持久化临时 loopback origin 前阻塞 proxy 上线。
- npm、pnpm、Yarn Classic/Berry、Bun、Deno 的变量名分别做真实版本 smoke test，尤其覆盖 pnpm 11.21 和 Yarn 1.22.22 的已知差异。
- direct shim、`osdk exec` 和 shell activation 入口不产生递归 shim，且 stdin/stdout/stderr、signal/exit code 行为保持不变。

## 10. English design summary

### Scope and configuration

SDK download **sources** and project dependency **registries** are separate control planes. `[sources.<tool>]` chooses where osdk itself obtains a runtime or package-manager artifact. `[registries.npm]` is intended to choose an npm-compatible registry for a delegated project dependency command; it must never rewrite SDK sources, native scope rules, credentials, or lockfiles.

```toml
[registries.npm]
urls = [
  "https://registry.npmmirror.com/",
  "https://registry.npmjs.org/",
]
probe_timeout_ms = 1500
```

The array order is policy order. Probe candidates anonymously with strict time and response-size bounds, optionally in parallel, then select the first healthy configured or recognized native URL in policy order. Do not latency-sort those policy candidates. An explicit list never gains undeclared built-ins; without one, a recognized public native default keeps the built-ins as pre-start fallback. Only the pure built-in set is latency-ranked. Inject exactly one manager-native environment variable and execute the original manager command once. If all candidates are unhealthy, start the manager zero times. Never retry or replay a running/failed install because it may already have changed a lockfile, cache, `node_modules`, or arbitrary lifecycle-script state.

### Manager contract

| Family | Fetching commands (allow-list examples) | One-process override |
| --- | --- | --- |
| npm | `install/i/ci/add/update/up/exec`, `npx` | `npm_config_registry` |
| pnpm | `install/i/add/update/up/fetch/dlx/deploy`, `pnpx` | `pnpm_config_registry` |
| Yarn Classic 1.x | `install/add/upgrade/create` | `YARN_REGISTRY` |
| Yarn Berry 2+ | `install/add/up/dlx` | `YARN_NPM_REGISTRY_SERVER` |
| Bun | `install/i/ci/add/update/x`, `bunx` | `BUN_CONFIG_REGISTRY` |
| Deno | npm-capable `add/bench/cache/check/ci/compile/doc/eval/info/install/outdated/run/serve/task/test/update` | `NPM_CONFIG_REGISTRY` |

Strict offline bypass is manager- and command-aware: npm/pnpm/Yarn/Bun use
their supported `--offline` mode, while Deno uses `--cached-only` only on
commands that accept it. `--prefer-offline`, immutable/frozen lockfile modes,
and Deno `ci`/`outdated`/`update` can still access the network and therefore
retain preflight. Deno does not expose a general `--offline` option.

`pnpm_config_registry` was verified with pnpm 11.21, where `npm_config_registry` did not take effect. `YARN_REGISTRY` was verified with Yarn 1.22.22, where `npm_config_registry` did not take effect. Compatibility aliases may be version-dependent and must not be documented as universal. Yarn must pass through when its major version is unknown.

Explicit registry CLI options or environment variables always pass through. Native private, unknown, scoped, or authenticated configuration also passes through unchanged. osdk must not probe those endpoints, extract credentials, override scope routing, or forward secrets to mirrors. A known anonymous public native default may participate in selection; uncertainty means pass-through.

### Lockfile and cache boundary

An injected default registry cannot portably redirect absolute tarball URLs in manifests, lockfiles, or registry metadata. Such URLs may bypass the selected registry. npm has configurable host-replacement and `resolved` omission semantics; pnpm may reconstruct canonical registry URLs but retain noncanonical ones; Yarn Classic persists absolute `resolved` URLs, while Modern Yarn may bind nonconventional archives. Bun and Deno serialization remains an explicit pinned-version test gate. The MVP does not parse or rewrite lockfiles and does not intercept network traffic, so health of the metadata endpoint is not a guarantee that every artifact URL is healthy or served by the selected host.

The five native cache families also remain isolated: npm uses opaque `cacache` response blobs and an index; pnpm uses a versioned content-addressed store of unpacked files and package indexes; Yarn Classic and Modern Yarn use version-specific expanded trees, raw offline-mirror tarballs, or normalized zip archives; Bun uses manager-owned expanded `${name}@${version}` directories and metadata; Deno uses a mixed module/package/analysis state root under `DENO_DIR`. Exact paths and schemas are version-sensitive implementation details. Their units, keys, metadata, materialization, locking, and garbage collection are incompatible. Stable per-manager roots enable reuse within one manager, not cross-manager sharing.

### Future shared tarball CAS

A future design may place an SRI-keyed CAS of exact upstream artifact `.tgz` bytes beneath the native caches and expose it through a short-lived loopback registry proxy. It must verify strong SRI before atomic publication, never stream an unverified miss, and let each manager populate its own cache afterward. Metadata and authorization remain registry/identity-scoped even if verified byte objects are globally deduplicated. This does not exist merely because native caches share an osdk parent directory.

The proxy is not safe to ship until it has loopback-only binding and per-process access control, redirect/DNS/SSRF defenses, credential and metadata isolation, owner-only/symlink-safe local storage, strict resource limits, atomic concurrent writes and corrupt-hit repair, correct npm registry and scoped-package semantics, a manager-specific solution that never persists random loopback URLs into lockfiles, and a tested policy for existing absolute tarball URLs. SRI authenticates exact bytes against a digest; it does not establish publisher provenance when metadata itself is compromised. osdk must still execute the manager exactly once.

## 11. 官方参考 / Official references

- npm registry and configuration: <https://docs.npmjs.com/cli/v11/using-npm/registry>, <https://docs.npmjs.com/cli/v11/using-npm/config#registry>, <https://docs.npmjs.com/cli/v11/configuring-npm/npmrc>
- npm cache and lockfile: <https://docs.npmjs.com/cli/v11/commands/npm-cache>, <https://docs.npmjs.com/cli/v11/configuring-npm/package-lock-json>, <https://docs.npmjs.com/cli/v11/using-npm/config#replace-registry-host>, <https://docs.npmjs.com/cli/v11/using-npm/config#omit-lockfile-registry-resolved>
- pnpm registry, lockfile, and store layout: <https://pnpm.io/settings#registry>, <https://pnpm.io/settings#lockfileincludetarballurl>, <https://pnpm.io/symlinked-node-modules-structure>, <https://pnpm.io/cli/store>
- Yarn configuration and caching: <https://yarnpkg.com/configuration/yarnrc#npmRegistryServer>, <https://yarnpkg.com/configuration/yarnrc#cacheFolder>, <https://yarnpkg.com/configuration/yarnrc#enableGlobalCache>, <https://yarnpkg.com/features/caching>, <https://classic.yarnpkg.com/lang/en/docs/cli/cache/>, <https://classic.yarnpkg.com/blog/2016/11/24/offline-mirror/>
- Bun registries, `.npmrc`, install flags, and cache: <https://bun.com/docs/pm/scopes-registries>, <https://bun.com/docs/pm/npmrc>, <https://bun.com/docs/pm/cli/install>, <https://bun.com/docs/pm/global-cache>
- Deno npm registry, cache, and supply-chain boundaries: <https://docs.deno.com/runtime/fundamentals/node/#private-registries>, <https://docs.deno.com/runtime/reference/cli/cache/>, <https://docs.deno.com/runtime/reference/env_variables/>, <https://docs.deno.com/runtime/packages/supply_chain/>
- Subresource Integrity: <https://www.w3.org/TR/SRI/>
