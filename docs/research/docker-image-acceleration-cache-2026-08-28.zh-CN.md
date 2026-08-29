# Docker 镜像加速与 OCI 缓存实施报告

日期：2026-08-28

项目：`github.com/lejunyang/one-sdk`

状态：设计建议；本报告不代表任何实现承诺

## 核心决策

Docker 和 OCI 加速应有意拆分为两个彼此独立的
层来交付。

1. **先交付原生运行时集成。** `osdk` 应分别发现 Docker
   Engine、containerd 和 BuildKit；解释它们实际生效的
   镜像仓库配置；测试候选路径；生成安全的配置
   方案；并将拉取、构建、缓存报告和清理委托给原生
   所有者。Docker Engine、containerd 和 BuildKit 必须继续拥有各自的
   内容存储、凭据、已解包快照、租约和垃圾
   回收。
2. **仅将由 `osdk` 管理的 OCI 存储作为第二个实验性产品加入。**
   它最初的实用形态应是以匿名方式从公共源按摘要锁定地获取内容并存入
   OCI 镜像布局，随后再加入固定/取消固定、导出/导入、可恢复传输
   和图感知型 GC。只有在这些
   基础能力通过本报告中的安全与并发门禁后，才可以添加本地只读代理。

通用的带身份验证的拉取直通代理是一种镜像仓库产品。它必须解决
仓库范围授权、Bearer 令牌质询、凭据辅助程序
集成、可变标签新鲜度、内容协商、多平台
选择、引用者、配额、租约、GC、SSRF 防护以及调用方之间的
隐私隔离。因此，它明确不在初始实现范围内。

最重要的产品约束是存在多个彼此独立的
控制平面：

- Docker Engine 运行时拉取；
- containerd 拉取，包括 Kubernetes 节点所使用的 CRI 命名空间和
  配置；
- 每个 BuildKit 或 Buildx 构建器的基础镜像拉取；
- BuildKit 构建缓存导入/导出；
- 以及未来的 `osdk` OCI 缓存。

一个平面的测试成功，并不能对其他平面得出任何确定结论。尤其是
Docker Engine 的 `registry-mirrors` 是一种 Docker Hub 机制；它
不是通用的 `ghcr.io` 主机重写机制。

## 目标

建议的能力应：

- 让缓慢且不可靠的公共镜像拉取可诊断；
- 配置正确的原生控制平面，同时不隐藏所做的更改；
- 区分 Docker Hub 镜像源与 GHCR 及其他镜像仓库代理；
- 保留原生凭据，绝不将其泄漏给加速端点；
- 解析并报告不可变摘要和目标平台；
- 在大小、摘要、媒体类型、签名或元数据损坏时采用失败关闭策略；
- 使原生清理操作显式可见，并使托管缓存的 GC 感知租约和图结构；
- 保留签名、证明、SBOM 以及其他 OCI 引用者；
- 在离线和网络退化模式下保持可预测的行为；以及
- 提供一条从诊断逐步演进到本地只读 OCI 缓存的路径，而无须
  让 `osdk` 成为容器运行时。

## 非目标

实现初期不应：

- 替代 Docker Engine、containerd、CRI、BuildKit 或其快照器；
- 解包层、构造根文件系统或运行容器；
- 向上游镜像仓库推送内容，或在其中删除、重新打标签或修改内容；
- 实现透明 TLS 拦截代理；
- 代理私有镜像仓库或转发上游凭据；
- 静默重写 Dockerfile、Compose 文件、Kubernetes 清单或
  锁文件；
- 将 BuildKit 构建缓存导出/导入视为等同于镜像拉取缓存；
- 扫描或删除原生运行时私有存储目录内的文件；
- 自动运行 `docker system prune`、`docker image prune`、
  `docker buildx prune` 或 containerd 的同类命令；
- 提供高可用的团队镜像仓库、复制服务或跨站点
  缓存集群；或
- 为让损坏的镜像源看似可用而削弱摘要或签名检查。

对于持久的局域网或组织级缓存，`osdk` 应与
Harbor、Distribution 或 zot 等持续维护的镜像仓库产品集成并对其进行
诊断，而不是把这套运维能力嵌入 CLI。Harbor 明确
支持将 GitHub Container Registry 作为代理缓存端点类型。

## 术语与信任边界

本报告对以下术语作精确定义：

- **源镜像仓库**：由镜像引用指定的权威镜像仓库，
  例如 `docker.io` 或 `ghcr.io`。
- **镜像源**：原生客户端为源命名空间选择的备用端点。它是否
  可以解析标签取决于具体运行时。
- **代理缓存**：一种呈现为镜像仓库的服务，它从源站获取内容并
  存储响应以供后续请求使用。其向客户端展示的仓库命名空间
  可能与源命名空间不同。
- **构建缓存**：用于复用构建步骤的 BuildKit 记录。它与
  基础镜像中缓存的清单和层并非同一事物。
- **解析**：将可变标签映射到清单或索引摘要。这是一个
  安全敏感操作。
- **内容获取**：检索已由摘要命名的字节。摘要校验的是
  字节完整性，而不是发布者身份。
- **引用者**：其 `subject` 指向另一个清单的清单，通常
  用于签名、证明、SBOM 和来源信息。
- **租约**：保护使用中内容不被 GC 的临时或持久根。

源身份、标签解析、内容传输、发布者信任以及
保留决策是相互独立的控制项。镜像源即使不被信任去决定标签
所代表的摘要，仍可安全地提供按确切摘要寻址的字节。

## 运行时与构建器能力矩阵

| 控制平面 | 镜像源/代理覆盖范围 | 配置与生命周期 | 存储及 GC 所有者 | 建议的 `osdk` 角色 |
| --- | --- | --- | --- | --- |
| Docker Engine | `registry-mirrors` 文档用于 Docker Hub 拉取 | 全守护进程范围的 `daemon.json` 或守护进程参数；通常需要特权，并可能需要重新加载/重启 | Docker Engine | 检查、测试、生成合并方案，并可在确认后应用 |
| containerd | 通过 `hosts.toml` 为每个镜像仓库配置命名空间，包括 `ghcr.io` | `config_path` 选择 hosts 目录；更改 `hosts.toml` 通常不需要重启守护进程 | containerd 内容存储、镜像元数据、快照、租约、GC | 任意镜像仓库专属的镜像策略应优先使用它；保留能力和命名空间行为 |
| BuildKit/Buildx | 在 `buildkitd.toml` 中按镜像仓库配置镜像源，包括非 Hub 镜像仓库 | 构建器专属；不同 Buildx 构建器可能使用不同的驱动和配置 | 选定的 BuildKit 工作节点 | 发现选定的构建器，并与运行时拉取分开配置和测试 |
| Harbor/Distribution/zot | 取决于产品的上游代理支持；Harbor 包括 GHCR | 具有 TLS、身份验证、存储、监控和升级能力的独立服务 | 代理镜像仓库 | 将其视为外部端点并验证其行为 |
| 未来的 `osdk` OCI 存储 | 初期支持匿名公共、按摘要锁定的获取；后续提供实验性只读代理 | 用户范围的 `osdk` 存储及可选的回环服务 | `osdk`，使用显式根和租约 | 仅在原生集成完成后作为实验性功能提供 |

### Docker Engine：Docker Hub 是特殊情况

Docker 文档将 `registry-mirrors` 描述为镜像 Docker Hub 库的一种方式。如下
守护进程配置可以加速 `ubuntu:24.04` 或
`docker.io/library/ubuntu:24.04`：

```json
{
  "registry-mirrors": ["https://mirror.example"]
}
```

它并**不会**透明地将 `ghcr.io/org/image` 映射到 GHCR 缓存。因此
必须将以下情况报告为不同的结果：

- `docker.io`：支持 Docker Engine 镜像源方案；
- `ghcr.io`：Docker Engine 没有等效的按主机镜像源映射；应使用
  显式改写的代理引用、镜像仓库产品的代理命名空间、
  containerd 配置或构建器专属的 BuildKit 配置；
- 守护进程 HTTP(S) 代理：这会更改出站网络传输，并非
  OCI 镜像源映射；它是一项由外部管理的高级选项。

引用重写，例如从 `ghcr.io/org/image@sha256:...` 改写为
`harbor.example/ghcr/org/image@sha256:...`，可能会保留清单和层
摘要，同时改变镜像仓库和仓库身份。仓库范围的
授权、准入规则、允许列表和签名身份策略可能
因此表现不同。`osdk` 必须展示重写后的引用，而不能
将其呈现为透明过程。

Docker Engine 默认并发下载三个层。降低
`max-concurrent-downloads` 可以改善有丢包或低带宽的链路，但它是一个
守护进程全局性能设置。`osdk container doctor` 可以基于证据建议
进行更改；但绝不能静默更改。更高的并发度
并不总是更快，因为镜像仓库限流、广域网丢包、解压缩和磁盘
写入压力可能成为主导因素。

### containerd：将 `pull` 与 `resolve` 分离

当前的 containerd 镜像仓库配置使用包含
各命名空间 `hosts.toml` 文件的 hosts 目录。CRI 通过 `config_path` 指向该目录；containerd 1.x 和 2.x 的具体
插件表有所不同。`osdk` 必须检查
运行中的版本和实际生效的配置，而不能假定某一路径。它应当
诊断已弃用的内联 CRI `registry.mirrors` 和 `registry.configs`
形式，但不应为新安装生成该形式。

Containerd 能力既是协议功能，也是信任声明：

- `pull` 允许获取清单和 blob；
- `resolve` 允许将标签映射到摘要；
- `push` 允许上传。

不受信任、不能决定标签身份的加速端点应仅具有
`["pull"]`；应由权威上游解析标签，之后
镜像源可以提供按摘要寻址的字节。完全受信任的代理缓存可以
获得 `["pull", "resolve"]`。`push` 应保留在权威
镜像仓库上，除非用户明确配置了不同的发布模型。

适配器必须保留主机顺序、`server`、能力、CA 和客户端
证书设置、`override_path` 以及命名空间语义。代理主机可能
通过 `ns` 查询参数接收原始命名空间；`ns` 是路由
上下文，而不是授权。`override_path` 用于 API 根路径非标准的
镜像仓库，不得为普通的 `/v2` 端点启用。

### BuildKit：选定的构建器是其自身的控制平面

BuildKit 接受 `buildkitd.toml` 中按镜像仓库配置的镜像源：

```toml
[registry."docker.io"]
mirrors = ["mirror.example"]

[registry."ghcr.io"]
mirrors = ["ghcr-cache.example"]
```

该配置属于某个 BuildKit 守护进程。Buildx 可以在
`docker`、`docker-container`、`kubernetes`、`remote` 和云端支持的构建器之间选择，
每种构建器的所有权和配置边界各不相同。编辑主机上的
Docker 守护进程并不能证明 `docker-container` 或远程构建器使用
相同的镜像源。诊断路径至少必须记录：

- 当前 Docker 上下文；
- 选定的 Buildx 构建器和节点；
- 驱动和端点；
- 报告的平台；
- 实际生效或附加的 `buildkitd.toml`；
- 镜像仓库的镜像源和 TLS 设置；以及
- BuildKit 工作节点磁盘用量和 GC 策略。

BuildKit 的 `reservedSpace`、`maxUsedSpace`、`minFreeSpace`、期限和记录
过滤器属于原生 GC 策略。`inline`、`local`、
`registry` 和 `gha` 等构建缓存后端用于导入或导出求解器缓存记录。它们可以改善
重复构建，即使每个基础镜像层仍来自源站；
而镜像仓库镜像源可以改善基础镜像传输，却不保留
任何构建步骤。CLI 和文档绝不能将二者称为等同事物。

## 与当前 `osdk` 架构的契合度

仓库已经包含有用的可靠性基础能力，但容器
镜像需要一个新领域，而不是另一个 SDK 后端。

| 现有区域 | 可复用的理念或代码 | 容器支持的边界 |
| --- | --- | --- |
| `config/mod.rs` | 分层的 CLI/环境/项目/用户配置、类型化设置和项目信任输入 | 添加单独的 `[containers]` 部分；不要将 OCI 镜像仓库放在 SDK `[sources]` 或 npm `[registries]` 下 |
| `source/mod.rs` 和 `source/select.rs` | 有界探测、缓存排名、源指纹、固定和离线行为 | OCI 镜像源具有镜像仓库命名空间、身份验证和 `resolve` 信任语义；不得静默按延迟重新排序已配置的策略顺序 |
| `pipeline/download.rs` | `.partial` 状态、`Range` 加 `If-Range`、精确的 `206` 检查、`416` 重启、重试、流式传输和原子重命名 | 部分下载的身份必须包含镜像仓库/仓库/摘要，且最终发布还必须验证描述符大小和 OCI 摘要 |
| `store/mod.rs` | 近似原子的内容发布、去重、清单根，以及在清单损坏时采用失败关闭的 GC | 当前存储使用 BLAKE3 对解压出的文件做哈希；OCI 要求按描述符算法存储精确的原始描述符字节，并需要图根和租约 |
| `cache/mod.rs` | 尊重原生管理器所拥有的缓存格式，而不假装每个生态系统共用一个 CAS | Docker、containerd 和 BuildKit 存储同样应保持由原生系统拥有 |
| `http/mod.rs` | 有界重定向以及在来源变化后移除显式标头 | OCI 身份验证增加了质询 realm、仓库/操作范围、令牌生命周期和更严格的跨来源策略 |
| `verification/mod.rs` | Fulcio/SCT、DSSE、Rekor SET/检查点/Merkle 证明、签名时间检查、缓存的 bundle，以及 `off`/`if-available`/`required` 策略先例 | 当前证据专用于 GitHub 发布制品；OCI 发现以及 Cosign/Notation 载荷绑定需要专用验证器 |
| `cli.rs` 和 `commands.rs` | 现有的 `doctor`、`source test`、`cache`、`prune --dry-run`、确认和子进程模式 | 添加一个 `container` 命令族，以免将运行时状态与 SDK CAS 混淆 |
| `docs/package-registry-design.md` | 匿名有界探测、信任边界、不转发凭据、失败关闭的预检和仅执行一次的委托操作 | OCI 令牌质询以及镜像仓库/仓库身份是额外要求 |

### 为什么 SDK CAS 不能直接存储 OCI 对象

当前的 `Cas` 使用 BLAKE3 对每个**解压出的普通文件**进行哈希，并
通过硬链接、reflink 或复制来具现化安装。OCI 描述符
则以**精确的编码字节**命名索引、清单、配置、
压缩层、签名或证明，通常使用 `sha256`。
解压缩某个层、更改 JSON 字节或重新打包归档文件都会改变其
镜像仓库摘要。

这两个存储可以共享底层文件系统辅助函数、锁定工具和
原子发布模式。它们绝不能共用对象命名空间，也不能声称
某个 BLAKE3 解压文件对象就是其来源的 OCI 描述符。OCI
存储还需要根、引用遍历、租约、标签解析元数据
以及当前安装清单 GC 未建模的引用者关系。

当前 CAS 在被引用的安装清单损坏时会正确拒绝执行 GC。
这一失败关闭规则应继续沿用。然而，其当前的静态根模型
以及缺少 OCI 摄取租约，使其不足以支持并发的
获取/代理/GC 操作。

### 为什么它不是 SDK `Source`

SDK 源决定 `osdk` 从何处下载一个已知的工具制品。OCI
镜像仓库将仓库范围的引用解析成图，可能要求进行
质询令牌交换，协商媒体类型，并公开相关制品。
同样，`[registries.npm]` 控制委托的项目包操作，且
必须保持独立。新的 `[containers]` 部分是第三个网络平面。

## 推荐架构

```text
                         osdk container ...
                                |
             +------------------+------------------+
             |                                     |
       inspect / plan / test                  experimental OCI client
             |                                     |
    +--------+---------+---------+          reference + platform resolver
    |                  |         |                    |
 Docker Engine     containerd  BuildKit         registry protocol
 native store      native CAS  worker cache       /         \
 credentials       snapshots   native GC     OCI blob store  referrers/verify
 leases + GC       leases + GC                  roots + leases + GC
```

原生适配器和托管 OCI 客户端共享引用解析、
平台类型、脱敏、诊断和策略。它们不共享运行时存储的
所有权。

## 原生集成设计

### 探测

探测是只读且感知版本的。绝不能仅凭某个二进制文件
存在，就推断某个运行时存在。实现在条件允许时应同时收集客户端
和服务端信息。

对于 Docker Engine：

- 定位 `docker` 并查询所选上下文；
- 区分本地、SSH、TCP 和 Docker Desktop 端点；
- 记录服务端 OS/架构、Engine 版本、rootless/Desktop 模式，以及
  守护进程报告的 registry mirror 信息；
- 仅当守护进程配置文件位于本地且目标明确时才定位该文件；并且
- 说明远程上下文必须在远程主机上配置。

对于 containerd：

- 定位守护进程和客户端，记录版本、地址和命名空间；
- 区分直接使用 `ctr` 的行为与 CRI 行为；
- 读取生效的插件版本和 `config_path`；
- 识别命名空间专属的 `hosts.toml` 与 `_default` 回退；并且
- 标记旧式内联 registry 配置，但不重写它。

对于 BuildKit/Buildx：

- 除非提供了 `--builder`，否则查询所选 builder；
- 枚举节点、驱动、端点、状态和平台；
- 判断配置是否位于本地且可编辑；并且
- 在没有证据时，绝不将主机守护进程的 mirror 归因于独立 builder。

探测应将 `not-installed`、`client-only`、`unreachable`、
`permission-denied` 和 `unsupported-version` 作为不同状态返回。守护进程
不可达并不能证明其配置不存在。

### Registry 路径测试

`osdk container registry test REGISTRY` 默认应使用匿名方式，并且有明确边界。
可选的 `--image` 可以启用更深入的检查，而无需任意选择一个大型
仓库。测试报告应包括：

1. DNS 地址、连接目标、TLS 对等端名称、CA 结果和耗时。
2. `/v2/` 响应：成功响应或语法有效的 `401` bearer challenge
   表明 registry 可达；登录页面则不能。
3. 脱敏后的 challenge realm、service 和请求的仓库 scope。
4. Manifest `HEAD`/有界 `GET`、协商得到的 media type、返回的
   `Docker-Content-Digest`、字节大小和摘要验证结果。
5. tag 是由源站还是 mirror 解析。
6. 所选小型 blob 的 `HEAD`、有界 `Range`、`206`/`Content-Range`，以及
   从头重新下载的回退行为。
7. 请求的平台与选中的平台，以及 index 和子 manifest 的摘要。
8. mirror 回退情况，且不转发凭据。
9. 按需检查 Referrers API 支持、分页和回退可用性。
10. 明确区分需要认证、访问被拒、达到速率限制、未找到、内容损坏和协议
    不兼容。

该命令必须限制响应体、重定向、墙钟时间和下载
字节数。它必须脱敏 URL 用户信息、查询参数值、authorization header、bearer
token、cookie、签名 URL 和凭据助手输出。诊断 JSON 必须
遵循与人类可读输出相同的脱敏规则。

### 规划原生配置变更

`mirrors plan` 应生成语义差异、影响范围、权限
要求、验证命令、重启/重建要求和回滚
路径。它不应执行写入。计划必须保留未知键和现有
注释，只要相应格式允许。

Docker Engine 规划必须：

- 将针对 `ghcr.io` 的请求判定为 `registry-mirrors` 不支持，而不是
  生成具有误导性的守护进程补丁；
- 仅合并请求的 `registry-mirrors`，以及可选但明确请求的
  下载设置；
- 拒绝 `dockerd` 报告的命令行选项与文件选项冲突；并且
- 说明目标是 Docker Desktop、rootless Docker、本地服务，
  还是远程上下文。

Containerd 规划必须：

- 在缺少 `config_path` 时，渲染正确的 1.x 或 2.x 版本变更；
- 渲染一个命名空间目录和 `hosts.toml`，且不触及无关的
  registry；
- 在视觉上明确区分 `pull` 与 `resolve`；
- 保留 TLS 和 `override_path` 设置；并且
- 说明 hosts 目录的编辑通常不需要重启，而新增
  `config_path` 可能需要。

BuildKit 规划必须：

- 指明确切的 builder 和节点；
- 拒绝假装远程或云端 builder 的本地文件可编辑；
- 渲染各 registry 的 mirror/TLS 变更，并保持 GC 策略不变，除非
  明确要求修改；并且
- 说明是否必须使用该配置重建或重启 builder。

### 应用变更

应用属于后续阶段，必须获得明确同意，即使自动化环境中存在全局
`--yes` 也不例外。写入前，`osdk` 应重新读取文件，
并将其与生成计划时使用的版本比较。它应写入同目录
临时文件、验证完整结果、创建带时间戳且
权限受限的备份、在平台允许时进行原子替换，并显示仍需执行的
确切服务操作。它不得自行提升权限。

回滚必须指明备份，并且无需 `osdk` 即可完成。secret、内联
auth 和私钥绝不能复制到诊断输出或任何用户均可读取的
备份中。当无法安全保留注释时，命令
应停止，并提供渲染好的片段供手动应用。

### 委托拉取和清理操作

原生 `pull` 是一个精确一次的进程操作。在 `docker pull`、
`ctr`/CRI pull 或 BuildKit 操作启动后，`osdk` 必须保留 stdin、
stdout、stderr、信号和退出状态，且不得针对
另一个 mirror 重放命令。失败的拉取可能已经填充了内容或改变了
运行时元数据。mirror 故障转移应由原生客户端在该次操作之前或期间
完成。

原生缓存状态应使用受支持的 API 或 CLI。原生 prune 应是一个
独立的显式命令；在运行时能够提供时，应带 dry-run/预览。
`osdk` 绝不能通过遍历 `/var/lib/docker`、
containerd 根目录或 BuildKit 状态目录来实现原生清理。预览必须说明
原生 prune 可能移除在 `osdk` 之外创建的状态。

## 托管 OCI 获取与缓存设计

### 范围门控

首个托管实现仅支持：

- 明确允许列表中的公共 registry；
- 匿名请求；
- 不可变的摘要引用，或在线解析并立即记录
  为摘要的 tag；
- 只读的 `GET`/`HEAD` 镜像内容；
- 获取所选平台，只有明确请求时才获取所有平台；以及
- 输出 OCI image layout，并通过原生运行时受支持的
  接口导入其中。

私有 registry、任意 challenge realm、凭据转发、push、
delete、跨用户服务模式和透明守护进程拦截仍保持
禁用。此边界应在类型和路由中强制执行，而不仅仅写在
文档中。

### 引用解析与规范身份

创建一个带类型的 `ImageReference`：保留用户的原始拼写以供输出，
但解析出规范的 registry、repository，以及 tag 或 digest。它必须安全地
处理默认 Docker Hub 展开、端口、IPv6 字面量、tag 和 digest
算法，同时拒绝 URL 语法、凭据、查询字符串、片段、
空组件/路径遍历组件和不受支持的 digest 算法。

示例：

- `ubuntu:24.04` -> `docker.io/library/ubuntu:24.04`；
- `ghcr.io/org/app@sha256:...` 保持为 registry/repository/digest；
- `registry.example:5000/team/app:v1` 将端口保留为 registry 身份的一部分。

缓存键不得由未经清理的原始引用字符串派生。Blob
身份是 OCI digest。tag 解析身份包含规范 registry、
repository、tag、接受的 media type 和认证身份。MVP 只有
`anonymous` 身份。在设计出保护隐私的身份分区之前，
不得发布已认证缓存功能。

### OCI 图与媒体协商

一次镜像拉取是一张图，而不是单个归档：

```text
tag
 `-- resolves to index or manifest digest
      |-- image index / Docker manifest list
      |    `-- selected platform manifest
      `-- image manifest
           |-- config descriptor
           `-- ordered compressed layer descriptors

subject digest
 `-- referrer index
      `-- signatures / attestations / SBOM manifests and their blobs
```

每个 descriptor 都包含 media type、字节大小和 digest。获取器必须发送
明确的 `Accept` 集合，并且初始至少支持：

- `application/vnd.oci.image.index.v1+json`；
- `application/vnd.oci.image.manifest.v1+json`；
- `application/vnd.docker.distribution.manifest.list.v2+json`；以及
- `application/vnd.docker.distribution.manifest.v2+json`。

Schema-1 镜像、语义未知的 manifest、外部/不可分发 layer，
以及遍历规则不受支持的 artifact 类型必须以精确错误
失败；只有 OCI layout 允许安全传输时，才可不透明地保留它们。
在进行哈希或存储之前，不得规范化任何 manifest JSON。

对于每个响应，应在分配内存前验证公布的边界，读取量不得超过
配置的限制，对确切字节进行哈希，比较 descriptor 的大小和
digest，然后才可发布。`Docker-Content-Digest` 是有用的证据，但
不能替代对响应体进行哈希。mirror 返回稳定但错误的 digest 即为
损坏；重试不得压制或覆盖该结果。

### 平台选择

目标平台属于目标运行时或 builder，不一定属于
运行 `osdk` 客户端的机器。远程 Docker 上下文可能是 Linux/ARM，
而 CLI 运行在 Linux/AMD64 上。解析应按以下顺序：

1. 显式指定的 `--platform OS/ARCH[/VARIANT]`；
2. 所选 builder 或目标运行时的平台；
3. 仅在没有远程目标时使用本地主机。

当顶层 descriptor 是 index 或 manifest list 时：

1. 验证并记录顶层字节及 digest；
2. 在有界深度内递归处理嵌套 index；
3. 匹配 `os`、`architecture`、可选的 `variant`，以及相关的 `os.version` 和
   `os.features`；
4. 没有兼容条目时失败；
5. 当未知或不受支持的平台约束使选择存在歧义时
   失败，而不是静默选择一个看似相近的镜像；
6. 只有在评估完所有受支持的兼容性
   约束后，才使用确定性的 index 顺序；
7. 获取并验证所选 manifest、config 和有序 layer；以及
8. 持久化顶层和所选子项的 digest。

`--all-platforms` 是单独的选择加入项，因为它会成倍增加网络用量、磁盘
用量、referrer 遍历和验证工作。测试必须包含 `amd64`、
`arm64/v8`、variant 不匹配、嵌套 index 和 Windows `os.version` 场景。

### 存储布局

使用新的版本化存储，而不是 `<data>/store`：

```text
<data>/oci/v1/
  blobs/<algorithm>/<encoded>
  descriptors/<algorithm>/<encoded>.json
  roots/pins/<id>.json
  roots/layouts/<id>.json
  resolutions/<registry>/<repository-hash>/<tag>/<scope-hash>.json
  referrers/<registry>/<repository-hash>/<subject-digest>.json
  leases/<lease-id>.json
  locks/blobs/<algorithm>/<encoded>.lock
  locks/gc.lock

<cache>/oci/v1/
  partial/<algorithm>/<encoded>.partial
  partial/<algorithm>/<encoded>.json
  quarantine/<timestamp>-<digest>/...
```

Blob 路径包含经过精确验证的字节。Descriptor 记录不可变，
并包含 media type、size、digest 和验证时间戳。可变的操作
字段（如最后访问时间）应位于不可变的 blob 和 descriptor 文件之外。
repository 名称只能通过安全编码或哈希后的键出现，
规范值则存放在经过验证的元数据中。

在成功完成授权和验证后，digest 完全相同的 blob 可以跨
registry 和 repository 去重。解析记录、可见性
为私有的 manifest、referrer 探测、错误、命中/未命中报告和访问
元数据仍按 registry/repository/auth-identity 隔离。物理去重
绝不能让一个调用方推断另一个私有仓库是否包含某个 blob。
仅支持匿名身份的 MVP 避免了这类跨身份泄漏。

### 可恢复的 blob 摄取

当前下载器具备正确的失败处理模式，但需要一个
OCI 专用 API。partial 以预期 digest 和限定范围的请求
元数据为键，而不只是以 URL 为键。

1. 获取每个 digest 的摄取锁，并重新检查是否已有经过验证的最终 blob。
2. 写入内容前创建或续期临时 lease。
3. 在 partial 元数据中记录规范 origin、repository、预期 descriptor、ETag 或
   Last-Modified validator、字节数和创建时间。
4. 如果存在兼容的 partial，则请求 `Range: bytes=N-` 并使用 `If-Range`。
5. 只有在收到 `206` 且 `Content-Range` 精确、内部一致时才追加；
   否则截断并重新开始。
6. 遇到 `200`、validator 变化、不可能的总大小或 `416` 时，按照
   有界重试策略安全地从头重新下载一次。
7. 在最大大小、截止时间、重定向和并发
   限制下流式读取响应。
8. 对完整重建出的字节进行哈希。MVP 可以在恢复前
   重新读取现有 partial；持久化哈希状态的可移植性不足，
   不能盲目信任。
9. 要求大小和 digest 与 descriptor 完全一致。将不一致的数据移至
   有界 quarantine 记录供诊断，而不是放入活动存储。
10. 通过相邻临时文件和原子重命名发布，并在支持时对数据
    和父目录元数据执行 fsync。
11. 提交 descriptor/reference 元数据，然后释放 lease。

OCI Distribution 将 blob range 支持规定为 `SHOULD`，而不是普遍
保证。因此必须提供从头重新下载的回退路径。manifest 和较小的 JSON 响应
通常应以原子方式获取，而不是断点续传。

### Tag、新鲜度和离线行为

digest 是不可变缓存身份。tag 是可变的查找结果。存储一条
tag 记录，其中包含：

- registry 和 repository；
- tag 和 auth-scope 身份；
- 接受的 media-type 集合；
- 解析出的 digest 和 media type；
- 解析时间、validator 和配置的 TTL；以及
- 源端点，以及由 origin 还是可信 mirror 执行了解析。

在线使用时会重新验证过期 tag。`--offline` 仅在所选图的每个对象
都存在时才可使用已记录的 tag；输出必须说明已记录的
digest、解析时间戳和陈旧程度。不得声称该 tag
是当前最新的。只要所需的图和验证证据存在，digest 引用
就可以离线使用。在离线模式中不存在隐藏的网络回退。

### 根、租约和垃圾回收

托管 GC 是图遍历，而不是按目录年龄扫描。根包括：

- 显式的 `cache pin` 记录；
- 由 `osdk` 管理的已导出/离线 OCI layout；
- 策略保留期内的 tag 解析结果；
- 活跃 pin 所需的验证证据；以及
- 活跃的 fetch、export、import、verification 或 proxy-request lease。

遍历会跟随 index、manifest、config、有序 layer 和保留的
referrer。在持久根存在之前，活跃 lease 会保护 partial 和新发布的对象。
lease 包含 owner、创建/续期时间、过期时间和
所引用的 descriptor。崩溃客户端的 lease 最终会过期；活跃
操作会在此之前续期。

GC 策略应支持 `max_bytes`、`min_free_space`、高/低水位线和
最小对象年龄。它应驱逐未 pin 的最近最少使用根，直到
达到低水位线，然后清扫不可达 blob。全局 GC/变更
协调机制必须防止在内容发布与
根创建之间发生回收。每个 digest 的锁可防止重复发布，但不能取代
该屏障。

如果可达性所需的根、descriptor、manifest、referrer index 或 lease 记录
损坏，GC 必须在删除任何内容前以关闭方式失败。dry-run 和
实际 GC 必须使用相同的快照和遍历引擎。删除错误应
准确报告；回收的字节总数只计算成功删除的内容。
清扫后可以清理空的扇出目录。

在代理阶段之前，元数据后端必须证明具备崩溃一致的
多进程事务。对于仅获取阶段，版本化文件加 repository 锁可能足够；
对于并发服务，嵌入式事务索引可能更合适，
但该依赖及其 Windows 行为需要单独的 ADR 和基准测试。

### OCI image-layout 导出与导入

`fetch --output oci-layout:PATH` 应写入标准 OCI image layout，其中包含
`oci-layout`、`index.json` 和按确切 digest 寻址的 blob。发布应
使用同级临时目录并执行重命名。如果现有目标非空，
除非使用显式替换选项，否则绝不覆盖。

导出会保留解析出的顶层 descriptor 和所选平台信息。
使用 `--include-referrers` 时，还会导出可达的 signature、attestation
和 SBOM 图，并记录任何无法表示为
直接 OCI-layout 关系的兼容性回退。导入会验证每个 descriptor，然后才将其
加入托管存储或交给运行时支持的 importer。

运行时适配器应优先使用受支持的原生导入机制。它
不得直接写入 Docker/containerd/BuildKit 存储。

## 身份验证与安全模型

### 默认策略

- 只有匿名公共镜像源才是自动加速候选。
- 私有注册表和经过身份验证的镜像源使用原生直通方式。
- 上游 `Authorization` 标头、cookie、刷新令牌、Docker 配置
  条目、凭据助手结果或 GHCR PAT 绝不会被转发到另一个
  源站。
- 需要客户端身份验证的代理是独立的注册表身份；
  用户使用原生工具登录该代理。
- 拒绝 URL 用户信息，并对查询参数值做脱敏。

Docker 凭据可能由 `credsStore` 或各注册表的
`credHelpers` 管理，而不是作为可复用的 JSON 存在于 `config.json` 中。除非 `osdk` 日后实现
凭据助手协议以及显式、严格限定范围的授权流程，否则不应解析和
复制 Docker 身份验证材料。原生委托使 Docker 或其他运行时能够继续负责
该交互。

### 未来对认证客户端的要求

如果未来提出经过身份验证的托管获取方案，不记名令牌必须仅驻留于
内存中，并至少按注册表主机、质询域、服务、
仓库、操作和已认证身份区分键。客户端必须验证每个
质询域和重定向，使用 HTTPS，限制允许的域来源，遵守
令牌过期时间，请求最小权限的 `pull` 范围，并且绝不持久化令牌
响应。不同的认证身份不得共享标签或可见性
元数据。

OCI Distribution 有意不定义一套完整的通用身份验证
系统。注册表可能通过独立的令牌服务发起质询。支持一种
Docker 风格的不记名令牌流程，并不意味着与所有云
注册表、身份提供商或凭据助手都兼容。

### 拉取式缓存的凭据风险

Distribution 官方镜像文档警告，如果上游账户
拥有 Docker Hub 私有内容访问权限，那么该账户可见的所有内容都可能
通过镜像提供，除非镜像具有与之匹配的访问控制。
Harbor 同样指出，代理缓存端点的凭据可以拉取该
凭据有权访问的所有镜像。`osdk` 在诊断
经过身份验证的外部缓存时必须显式展示这项警告，并且绝不能使用
权限宽泛的个人凭据初始化此类缓存。

### 代理 SSRF 与路由控制

实验性代理默认绑定到环回地址和临时端口。它
仅服务已配置的上游注册表身份。它必须：

- 根据精确配置的 Host/路径或经过验证的 `ns` 映射进行路由，绝不接受
  调用方提供的任意 URL；
- 拒绝用户信息、片段、未经批准的端口和方案，以及路径遍历；
- 在防 DNS 重绑定策略下解析 DNS，并在 `public-only` 模式中阻止环回、链路本地、
  元数据服务、多播和私有目标；
- 独立验证重定向目标和质询域；
- 限制标头、正文、清单图深度、引用项页面数、并发数和
  时长；
- 仅支持只读的 `/v2/`、清单、Blob 和引用项路由；并且
- 不得在日志、指标、错误或缓存
  键中输出任何上游机密或敏感 URL。

监听局域网地址、允许私有目标、添加凭据，
或服务多个信任域，都会使该功能超出此嵌入式
公共缓存设计的范围，此时应要求使用外部注册表产品。

## 签名、证明和引用项

摘要验证可以证明接收到的字节与选定摘要一致，却
无法证明是谁选择或发布了该摘要。因此，完整的策略需要
将完整性与来源真实性分开。

建议的验证策略为 `off`、`if-available` 或 `required`，
沿用当前 CLI 的惯例。对于具有明确签名约定的
发布者，`required` 是适合生产环境的安全模式。
当不受信任的镜像源阻止发现引用项时，`if-available` 容易遭到
降级攻击；因此应优先采用权威源站发现，或先前固定且已经验证的
证据。

对于 Cosign 无密钥验证，需要绑定以下全部内容：

- 解析得到的主体摘要；
- 预期的证书身份，或有锚点且经过审查的身份表达式；
- 预期的 OIDC 颁发者；
- Fulcio 证书链和 SCT；
- Rekor 签名条目时间戳、检查点和 Merkle 包含证明，或携带
  等效证据的有效离线包；以及
- 签名时间和策略有效性。

`verify-attestation` 还必须验证 DSSE 主体摘要、谓词
类型以及策略特定的声明。仅仅存在证明并不代表
策略决策成功。缓存的验证结果以主体
摘要、策略指纹、信任根修订版本、验证器版本和证据
摘要作为键。离线验证会对缓存的
证据重新执行密码学验证，而不会信任缓存的布尔值。

OCI 1.1 使用 `subject`、`artifactType` 和 Referrers API 来发现
签名、证明和 SBOM。供应链完备的缓存必须保留
并提供该图，包括分页和按制品类型筛选。当
注册表对 Referrers API 返回 `404` 时，应支持标准化的引用项
标签回退，并在 Cosign 兼容性需要时支持其旧式摘要标签
约定。应记录所使用的发现路径，因为基于标签的回退存在
并发更新竞态。

对于多平台标签，解析得到的顶层索引是主要主体。
无关子项上的签名并不能授权该索引。策略也可以
要求选定的子清单具有签名，但结果必须分别报告
顶层和子项决策。只镜像层而省略
引用项时，绝不能将其报告为经过完整验证的镜像。

现有的 GitHub 制品验证器包含有价值的 Sigstore 原语，但
不能直接复用为 OCI 验证器：它的获取 API、仓库
身份和 DSSE 声明专用于 GitHub 发布证明。只有在测试证明
两个调用方都保留各自不同的策略绑定之后，才能提取
通用的信任根、包、Rekor 和签名时间辅助组件。Notation 支持属于
后续的策略提供方；OCI 引用项提供的是通用传输机制，而非 Notation 与 Cosign 之间通用的信任
语义。

## 建议的配置

容器设置应位于独立的顶层配置节中：

```toml
[containers]
runtime = "auto"             # auto | docker | containerd
builder = "auto"             # auto or an explicit Buildx builder name
platform = "runtime"         # runtime or OS/ARCH[/VARIANT]
probe_timeout_ms = 1500
tag_ttl = "15m"

[containers.registries."docker.io"]
mirrors = ["https://mirror.example"]
anonymous_only = true
resolve = "upstream"         # upstream | mirror

[containers.registries."ghcr.io"]
mirrors = ["https://ghcr-cache.example"]
anonymous_only = true
resolve = "upstream"

[containers.cache]
mode = "native"              # native | managed-experimental
max_bytes = 53687091200
min_free_space = 10737418240
high_watermark_percent = 90
low_watermark_percent = 75
min_age = "1h"

[containers.verification]
policy = "if-available"       # off | if-available | required
include_referrers = true
```

设计规则：

- `[containers]` 与 `[sources]` 和 `[registries.npm]` 保持分离。
- 项目级容器配置会改变执行行为和网络
  目标，因此必须通过现有的项目信任门禁。
- `runtime`、构建器选择、注册表映射、`resolve`、TLS 路径、代理
  允许列表和验证策略都参与信任指纹计算。
- 凭据和不记名令牌绝不存储在这里。原生凭据
  仍由原生工具管理。
- URL 必须使用 HTTPS，包含主机，并且不含用户信息、查询或片段。只有显式启动的本地代理
  才可以允许环回 HTTP。
- `resolve = "upstream"` 表示镜像源仅提供内容。无法
  表达独立解析的适配器，尤其是 Docker Engine 的 Hub 镜像路径，
  必须拒绝该计划或明确将其降级，而不能声称已实施该策略。
- 配置的镜像源顺序即为策略顺序。探测结果默认仅用于
  诊断。未来的 `selection = "fastest"` 只能在一组
  具有相同源站和信任策略、匿名、等价且仅提供内容的端点之间重新排序；
  它必须使用候选集指纹，并且绝不能将不受信任的
  解析器移到源站之前。
- CA 和客户端证书的**路径**可以作为未来的用户全局选项。不得
  从不受信任的项目配置中接受客户端密钥材料。
- 未知的原生运行时键必须在计划和应用过程中保留下来。架构稳定后，未知的
  `osdk` 容器键应产生清晰的版本/兼容性警告，
  而不是被静默忽略。

环境变量覆盖最初应仅限于非敏感选择器，例如
`OSDK_CONTAINER_RUNTIME`、`OSDK_CONTAINER_BUILDER` 和
`OSDK_CONTAINER_PLATFORM`。避免为镜像策略、TLS 或凭据提供范围宽泛的
环境变量接口。

## 建议的 CLI

### 阶段 1：检查、测试和计划

```text
osdk container doctor [--runtime auto|docker|containerd] [--builder NAME]
osdk container registry test REGISTRY [--image IMAGE] [--platform PLATFORM]
osdk container mirrors plan --runtime docker
osdk container mirrors plan --runtime containerd
osdk container mirrors plan --runtime buildkit --builder NAME
osdk container cache status [--runtime auto|docker|containerd|buildkit]
```

`doctor` 报告运行时上下文、守护进程端点/版本/平台、rootless 或
Desktop 模式、containerd 命名空间和 `config_path`、Buildx 构建器/驱动、
生效的镜像源、原生存储所有权、磁盘用量、GC 设置、检测到的
旧版配置，以及任何重启或权限边界。

`registry test` 分别检测 `/v2` 可达性、身份验证质询、标签解析、
摘要拉取、Range 行为、媒体协商、平台选择、TLS 和
引用项支持。它绝不打印机密，也绝不使用原生私有
凭据，除非未来设计了显式的认证模式。

`mirrors plan` 输出语义差异和机器可读计划，但不执行任何
写入。退出状态会区分健康、降级、不支持、配置无效
和目标不可访问。

### 阶段 2：显式原生操作

```text
osdk container pull IMAGE [--runtime ...] [--platform PLATFORM]
osdk container verify IMAGE[@DIGEST] --policy off|if-available|required
osdk container prune --runtime ... --dry-run
osdk container mirrors apply --runtime ... [--builder NAME]
```

当原生客户端提供相应信息时，`pull` 输出解析得到的摘要。`verify`
只解析一次，验证不可变主体，并报告顶层摘要和选定的
平台摘要。`prune` 要求提供特定于运行时的预览并进行确认。
`mirrors apply` 使用刚生成的计划，拒绝过时输入，验证
结果，并创建回滚备份。

### 阶段 3：托管 OCI 获取和缓存

```text
osdk container fetch IMAGE --platform PLATFORM --output oci-layout:PATH
osdk container fetch IMAGE --all-platforms --output oci-layout:PATH
osdk container cache pin IMAGE@DIGEST
osdk container cache unpin IMAGE@DIGEST
osdk container cache gc --dry-run
osdk container cache gc
osdk container export IMAGE@DIGEST --output oci-layout:PATH
osdk container import oci-layout:PATH --runtime docker|containerd
```

在线时允许输入标签，但输出始终记录解析得到的摘要。缓存
固定必须使用摘要。离线复用标签时会明确报告其陈旧性。GC 试运行
和实际执行共用同一种图算法。

### 阶段 4：实验性本地代理

```text
osdk container proxy serve \
  --listen 127.0.0.1:0 \
  --upstream docker.io \
  --public-only
```

该命令仍会明确标注为实验性。它是只读的，默认只绑定环回地址，
并仅限于显式的上游允许列表。它不能接受或
转发凭据。在开始监听端口之前，它会检查 GC 租约、配额、
请求路由、脱敏、崩溃恢复、引用项和平台/媒体
夹具是否均已通过测试。对于持久化或共享部署，它会建议使用
外部注册表；除非未来存在明确支持的服务模式，否则会
退出。

所有命令都需要提供中英文帮助、错误、状态标签、修复建议
和示例。人类可读输出应首先给出结论；稳定的 JSON
输出应包含模式版本和结构化的脱敏证据。

## 模块与所有权映射

容器领域应作为后端、模型和包
注册表的同级模块：

```text
crates/osdk-core/src/container/
  mod.rs                 公共领域 API 与功能门控
  reference.rs           镜像引用解析与规范化
  platform.rs            目标平台解析与索引选择
  runtime.rs             发现类型与原生适配器 trait
  docker.rs              Docker 上下文/守护进程检查与配置计划
  containerd.rs          版本化 CRI/hosts.toml 检查与计划
  buildkit.rs            Buildx 构建器发现与 buildkitd 计划
  registry.rs            有界 OCI Distribution 客户端
  mirror.rs              注册表测试矩阵与信任/能力策略
  auth.rs                质询模型与脱敏；初期仅支持匿名
  verify.rs              摘要、Cosign、证明与 referrer 策略
  oci_layout.rs          OCI image-layout 导入/导出
  cache/
    mod.rs               存储门面与路径
    metadata.rs          版本化描述符、根、解析记录与访问数据
    lease.rs             获取、续租、过期与释放租约
    gc.rs                图标记/清扫与配额策略

crates/osdk-cli/src/
  container.rs           命令编排与输出模型
  cli.rs                 `container` 命令树
  commands.rs            仅负责顶层分派

crates/osdk-core/tests/fixtures/oci/
  ...                    固定的清单、索引、blob、referrer 与 bundle
```

配套变更应位于：

- `config/mod.rs`：用于类型化、分层的 `[containers]` 配置；
- `dirs.rs`：用于显式 OCI 数据/缓存路径；
- `i18n/catalog.rs`：用于所有英文和中文字符串；
- `trust.rs`：用于影响执行/网络的项目设置；以及
- 仅在实现面向用户的阶段时，才更新成对的 README 与 VitePress
  文档。

初期应通过 shell 调用原生 CLI 来执行检查与委派，并使用
现有的进程控制约定。不要仅仅为了避免子进程而添加守护进程 SDK 依赖。在
实现托管注册表客户端之前，应从身份认证、重定向控制、精确
字节访问、referrer、维护活跃度、MSRV 和 Windows GNU 支持等方面评估一个
持续维护的 OCI 协议 crate。将其封装在 `registry.rs` 之后，使协议库的选择不会泄漏到
领域 API。

## 测试策略

所有测试都必须使用临时的 `HOME`、`DOCKER_CONFIG`、`OSDK_*`、运行时配置、
存储、缓存和构建目录。单元测试和集成测试绝不能修改
`/etc`、用户的 Docker 上下文、Docker Desktop 设置、containerd 状态或
Buildx 构建器。窄范围测试应通过确定性 fixture
或伪可执行文件来运行运行时 CLI；选择性启用的实时测试可使用一次性守护进程
和注册表。

### 引用与平台测试

- Docker 简写、显式 Docker Hub、GHCR、端口、IPv6、标签、摘要，以及
  规范化往返转换。
- 拒绝凭据、URL 查询/片段、路径遍历、格式错误的摘要，以及
  不安全的文件系统组件。
- OCI 和 Docker 媒体类型、嵌套索引，以及有界图深度。
- `linux/amd64`、`linux/arm64/v8`、缺失/错误的变体、不受支持的歧义、
  Windows `os.version`/`os.features`，以及无匹配错误。
- 远程运行时平台优先于客户端主机；显式 `--platform` 优先于
  前两者。

### 原生适配器测试

- Docker Hub 镜像源计划成功，而 GHCR Docker Engine 镜像源计划应被拒绝，
  并给出正确的替代方案。
- 能够区分 Docker 本地、远程上下文、rootless 与 Desktop 的所有权。
- containerd 1.x 和 2.x 插件路径、命名空间查找、`_default`、`server`、
  `override_path`、TLS，以及无需重启的 hosts 变更。
- containerd 不受信任镜像源获得 `pull` 而非 `resolve`；受信任镜像源
  获得显式请求的能力。
- 诊断已弃用的内联 CRI 配置，但不生成该配置。
- Buildx `docker`、`docker-container`、Kubernetes 和远程构建器保持
  相互独立；计划应标明选中的节点与配置边界。
- 按承诺，应用后保留现有未知 JSON/TOML 键和注释。
- 计划/应用应检测并发编辑，在替换前验证，限制
  备份权限，并报告重启/重建要求。
- 委派的拉取仅启动一次，并保留 stdout、stderr、stdin、信号
  和退出码。
- 清理默认采用 dry-run/预览，且不能直接触碰原生存储。

### 注册表协议与损坏测试

使用本地 fixture 注册表覆盖：

- `/v2/` `200`、有效的 `401` 质询、`403`、`404`、`429`，以及有界 `5xx`；
- 清单 `HEAD` 和 `GET`、内容协商、缺失/错误的
  `Docker-Content-Digest`，以及过大的 JSON；
- blob 完整拉取与 `Range` 情形：有效的 `206`、以 `200` 忽略 Range、`416`、
  格式错误或起点错误的 `Content-Range`、验证器发生变化、响应体截断，
  以及总大小不一致；
- 描述符摘要、大小和媒体类型不匹配；损坏数据绝不会
  发布；
- 公共仓库间相同摘要去重为一个 blob；
- 标签变更在 TTL 后刷新，离线使用会报告已记录的陈旧摘要；
- 重定向保持在策略范围内；凭据和敏感标头绝不跨
  源传递；
- 身份认证质询 realm 不能重定向到未经批准或私有的地址；
- Docker Hub 和 GHCR 路径语义；以及
- 有界并发、重试、退避、响应大小和图深度。

### GC 与租约测试

- 活跃租约保护部分内容以及已发布但尚未作为根的内容；
- 已释放或过期的租约变为可回收状态；
- 固定项、布局、保留的标签记录、索引、子项、配置、层和
  referrer 构成正确的可达性图；
- 共享 blob 保留到最后一个根消失为止；
- 确定性地执行高/低水位、最小年龄、最大字节数和最小可用空间
  限制；
- 对一个摘要的并发获取只发布一次；
- GC 不能在发布与根提交之间发生竞态；
- 崩溃恢复使废弃租约过期，并清理有界的部分内容；
- 损坏的根、清单、referrer 或租约元数据会让 GC 在任何
  删除之前失败；以及
- dry-run 与实际回收从同一快照中选择相同对象。

### 签名与 referrer 测试

- 有效的 Cosign 密钥签名和无密钥签名；
- 错误的证书身份、错误的 OIDC 签发者、错误的仓库、错误的签名对象
  摘要、过期/无效的签名时间，以及不受信任的根；
- 缺失、格式错误或被篡改的 Rekor SET、检查点与 Merkle 证明；
- 有效和无效的 DSSE 谓词类型与声明策略；
- 缓存 bundle 的离线重新验证，以及策略/信任根缓存失效；
- `off`、`if-available` 和 `required`，包括被镜像源抑制的 referrer；
- 原生 Referrers API、分页、制品类型过滤、标准化标签
  回退，以及旧版 Cosign 摘要标签；
- 分别报告顶层索引与所选子项的验证结果；以及
- 无关平台子项的签名不能授权该索引。

### 安全与服务测试

- Docker 配置、辅助程序输出、PAT、bearer token、cookie 或签名 URL 均不得出现
  在日志、JSON、诊断、错误、缓存键或备份中；
- 镜像源与质询重定向不能接收源站凭据；
- 代理路由拒绝任意上游、编码路径遍历、DNS 重绑定、
  元数据服务目标、不允许的端口，以及 Host/`ns` 混淆；
- 默认监听器位于回环地址，并使用随机可用端口；
- 只接受读取操作；上传、挂载、删除和 catalog 路由
  均失败；以及
- 仓库范围的元数据不能泄露私有缓存状态。

### 实现阶段所需的项目验证

每个阶段都应在其专属提交之前运行最窄范围的相关测试。
在宣布任何 Rust 实现阶段完成之前，运行：

- 格式化；
- 聚焦的单元测试与 CLI 集成测试；
- 完整工作区测试与 Clippy；
- MSRV 检查；
- 在受到影响时运行安装程序和文档检查；
- 对任何文档/导航变更运行 VitePress 生产构建；以及
- `./scripts/windows-wine-tests.sh`，这是本仓库对 Linux 下
  验证 Rust 代码的要求。

面向用户的工作包括在同一个功能提交中同步更新 `README.md`、`README.zh-CN.md`、中文
和英文 VitePress 指南/实现页面、导航、CLI 帮助，以及 i18n
catalog 更新。

## 分阶段实现与验收门槛

### 阶段 0：契约与 fixture

交付类型化镜像引用、平台匹配、运行时发现接口、
脱敏、配置 schema、输出 schema，以及确定性的本地 OCI fixture
注册表。不写入原生配置，也不提供托管缓存。

验收标准：

- 引用与平台 fixture 覆盖 Docker Hub、GHCR、多平台和
  Windows 情形；
- 配置合并与项目信任均为显式行为；
- 所有诊断结构在构造层面即确保机密安全；
- 无需真实守护进程即可测试运行时适配器；以及
- 本报告中来自官方来源的假设固定在测试注释或
  实现文档中。

### 阶段 1：原生诊断与计划

交付 `container doctor`、`registry test`、`mirrors plan` 和原生缓存
状态。所有操作保持只读。

验收标准：

- Docker Engine 为 `docker.io` 规划镜像源，但拒绝透明映射 GHCR；
- containerd 按命名空间报告并规划 `pull` 与 `resolve`；
- BuildKit 输出能够证明检查了哪个构建器和驱动；
- 匿名 GHCR 测试成功，且不读取或转发 PAT；
- 注册表测试报告摘要、平台、Range、TLS 和身份认证质询结果；
- 远程/不可达/权限错误彼此可区分；以及
- 双语 CLI/文档与 Windows 运行时测试通过。

### 阶段 2：安全的原生应用与委派

交付具备陈旧计划保护的配置应用、仅执行一次的原生拉取、
原生验证编排，以及显式原生清理。

验收标准：

- 应用时保留无关配置，验证完整结果，创建
  受保护的备份，并报告重启/重建步骤；
- 发生并发编辑时中止，而不是覆盖；
- 原生凭据仍由原生客户端管理；
- 委派命令保留流、信号和退出码，且仅运行一次；
- 没有预览和显式确认就不能运行原生清理；以及
- 任何测试或实现都不扫描原生私有存储。

### 阶段 3：摘要固定的 OCI 获取、布局与托管 GC

交付匿名公共获取、OCI 布局导出/导入、可断点续传的 blob 摄取、
固定项、租约、配额、dry-run GC 与离线使用。此阶段仍无 HTTP 代理。

验收标准：

- Docker Hub 和 GHCR fixture 镜像按摘要拉取；
- `amd64`、`arm64/v8` 和 Windows 选择会生成记录的索引与子项
  摘要；
- 精确字节按 OCI 摘要存储，并与 BLAKE3 SDK CAS 分离；
- 有效的 `206` 会恢复传输，而 `200`/`416`/错误范围会安全地重新开始；
- 大小、摘要、媒体类型或图损坏绝不会进入正式存储；
- 跨仓库公共 blob 在不混淆元数据作用域的情况下去重；
- 可变标签会重新验证，并显式标示离线使用陈旧数据；
- 活跃租约在 GC 后仍保留，损坏的可达性元数据会触发封闭式失败；以及
- 导出的 OCI 布局通过独立 OCI 工具验证。

### 阶段 4：验证与供应链完备缓存

交付 Cosign 验证、受策略约束的离线证据、Referrers API 与
回退发现，以及可选的 referrer 导出。

验收标准：

- 正确的签名和证明通过；错误的签发者、身份、仓库、
  摘要、谓词或 Rekor 证据失败；
- 镜像源省略 referrer 时，不得将 `required` 验证策略降级；
- 离线验证使用缓存证据重新运行密码学检查；
- Referrers API 与旧版回退 fixture 均可工作；以及
- 多平台索引与子项决策不会混淆。

### 阶段 5：实验性回环代理

交付仅限公共内容、只读、带允许列表的回环服务。将其置于
实验性标志之后，并针对持久服务推荐外部注册表。

验收标准：

- 精确的 Host/路径/命名空间路由适用于 Docker Hub 和 GHCR fixture；
- 不存在 push/delete/catalog 端点；
- 凭据不能进入或离开代理；
- SSRF、重定向、DNS 重绑定、标头/响应体、并发、配额和崩溃测试
  通过；
- 标签、清单、blob、平台、Range 请求、referrer 和签名
  通过代理时行为一致；
- 强制崩溃后，并发获取、租约续期和 GC 仍保持正确；
  以及
- 该功能拒绝非回环或需身份认证的操作。

需身份认证的代理、多用户隔离、LAN 绑定和持久服务
安装需要新的安全审查与产品决策；它们不会自动成为
阶段 6。

## 风险与缓解措施

| 风险 | 后果 | 必需的缓解措施 |
| --- | --- | --- |
| 混淆 Docker Hub 与 GHCR | 配置看似成功，但并未加速镜像 | 针对特定运行时验证能力，并明确返回不支持结果 |
| 配置错误的 Buildx builder | 构建继续使用旧路径 | 将每个方案绑定到 builder 名称、节点、驱动、端点和配置指纹 |
| 覆盖原生配置 | 守护进程停机或自定义设置丢失 | 语义合并、完整验证、陈旧输入检查、受限备份，且不进行隐式提权 |
| 凭据转发 | 注册表账户失陷或私有镜像暴露 | 自动路径仅允许匿名访问；标头绑定源站；私有内容走原生直通路径 |
| 可变标签投毒或过期 | 即使 blob 完整无损，仍选择了错误镜像 | 由源站控制解析、TTL 重新验证、固定 digest，并明确标示离线状态下的过期情况 |
| 镜像源返回损坏字节 | 拉取失败或内容不安全 | 发布前精确验证大小和 digest；隔离；绝不绕过校验和 |
| 平台错误 | 运行时失败或 Windows 基础镜像不兼容 | 针对目标运行时解析，并评估变体/操作系统约束 |
| 签名降级 | 字节有效，但来自不可信发布者 | 绑定 digest 的签名策略；权威的 referrer 发现；`required` 模式 |
| Referrers 被遗漏 | 签名/SBOM 经缓存后消失 | 保留并测试 Referrers API 和回退图 |
| GC 与摄取发生竞态 | 使用中的镜像变得不完整 | 活跃租约，加上全局 GC/变更协调，以及遇到损坏或不确定状态时拒绝删除的遍历机制 |
| 运行时存储重复 | 磁盘占用不降反升 | 默认优先使用原生路径；托管存储需显式启用，并设置配额和导入/导出意图 |
| 注册表/协议漂移 | 注册表升级后的行为发生变化 | 版本化夹具、有界兼容层、结构化诊断，且不静默回退 |
| 代理 SSRF | 内部服务或元数据服务遭到访问 | 精确允许列表、公共地址策略、DNS 重绑定防御、重定向/realm 验证 |
| 错误的性能承诺 | 镜像源增加延迟或触发限流 | 报告实测阶段和字节数；保持策略顺序；不将可达性等同于速度 |
| 跨平台回归 | Windows CLI/配置路径或子进程失败 | 隔离的 Windows GNU Wine 测试套件，加上原生 CI 和文件系统安全键值 |

## 决策摘要

### 已实现的原生 mirror 范围（2026-08-30）

原生优先部分已实现，且没有引入 osdk 自有 OCI store。Docker Hub 内置两个由运营方公开
说明的候选（`mirror.gcr.io` 和 `docker.m.daocloud.io`）；显式用户/项目 policy 会完整覆盖
它们。Registry report schema 2 只在 upstream 解析一次，再按不可变 Manifest digest 检查每个
mirror（index 还会检查选中的 child），对同一 layer 做有界 Range 采样，并且只对内容一致且
确实返回字节的结果排序。

`container mirrors apply` 串联测速与原生规划。交互使用会展示并确认同一进程内的 plan；
无人值守使用通过 `--dry-run --json` 获取新 ID，再要求 `--yes --accept-plan` 精确接受。应用
范围仍限制为 ready 且只有一个 candidate 的本地计划，并采用重复目标发现、跨进程锁、
no-follow 陈旧输入检查、候选解析、权限保留与同目录原子替换。它刻意不执行 sudo、daemon
重启、builder 重建、Desktop/远程修改、镜像存储或 OCI GC。

实施应从原生检查和配置开始，因为
这能在保留成熟运行时
语义的同时立即带来加速价值。containerd 是支持任意按注册表
镜像策略的最强原生选项，因为它可以将 `pull` 与 `resolve` 分离。BuildKit 必须
按 builder 分别配置。Docker Engine 的镜像功能应仅被表述为
Docker Hub 加速；GHCR 需要另一条路径。

只有在 `osdk` 能将固定到 digest 的 OCI 图拉取并验证到
独立存储中、正确选择平台、续传且不发布损坏
数据、保留 referrers，并用租约保护活跃数据之后，托管路径才足够可信。
代理是最后的实验层，而不是起点。

## 官方参考资料

本报告审阅了以下链接。应优先采用带版本的 OCI 规范（如有）；
containerd 操作示例仍必须与已部署的
containerd 版本匹配。

### Docker Engine 与凭据

- [镜像 Docker Hub library](https://docs.docker.com/docker-hub/image-library/mirror/)
- [Google Cloud 托管基础镜像缓存（`mirror.gcr.io`）](https://cloud.google.com/artifact-management/docs/managed-base-images)
- [DaoCloud 公共镜像加速](https://github.com/DaoCloud/public-image-mirror)
- [`docker image pull`](https://docs.docker.com/reference/cli/docker/image/pull/)
- [`dockerd` 参考资料](https://docs.docker.com/reference/cli/dockerd/)
- [`docker login` 与凭据存储/辅助程序](https://docs.docker.com/reference/cli/docker/login/)
- [Docker CLI `config.json` 属性](https://docs.docker.com/reference/cli/docker/#docker-cli-configuration-file-configjson-properties)
- [清理未使用的 Docker 对象](https://docs.docker.com/engine/manage-resources/pruning/)
- [Docker credential-helper 实现与协议](https://github.com/docker/docker-credential-helpers)

### containerd

- [注册表主机配置（`hosts.toml`）](https://github.com/containerd/containerd/blob/main/docs/hosts.md)
- [带版本的 containerd 1.7 hosts 文档](https://containerd.io/docs/1.7/hosts/)
- [containerd CRI 注册表配置](https://containerd.io/docs/2.3/cri/registry/)
- [containerd 内容流](https://containerd.io/docs/2.2/content-flow/)
- [containerd 垃圾回收与租约](https://github.com/containerd/containerd/blob/main/docs/garbage-collection.md)

### BuildKit 与 Buildx

- [配置 BuildKit 注册表镜像源](https://docs.docker.com/build/buildkit/configure/#registry-mirror)
- [`buildkitd.toml` 配置](https://docs.docker.com/build/buildkit/toml-configuration/)
- [上游 BuildKit 守护进程配置](https://github.com/moby/buildkit/blob/master/docs/buildkitd.toml.md)
- [Buildx builders](https://docs.docker.com/build/builders/)
- [Buildx 驱动](https://docs.docker.com/build/builders/drivers/)
- [构建垃圾回收](https://docs.docker.com/build/cache/garbage-collection/)
- [构建缓存存储后端](https://docs.docker.com/build/cache/backends/)
- [`buildctl` 缓存导入/导出](https://github.com/moby/buildkit/blob/master/docs/reference/buildctl.md)

### OCI 镜像与分发规范

- [OCI Image Spec 1.1.1 descriptor](https://github.com/opencontainers/image-spec/blob/v1.1.1/descriptor.md)
- [OCI Image Spec 1.1.1 image index 与 platform](https://github.com/opencontainers/image-spec/blob/v1.1.1/image-index.md)
- [OCI Image Spec 1.1.1 manifest](https://github.com/opencontainers/image-spec/blob/v1.1.1/manifest.md)
- [OCI image-layout 规范](https://github.com/opencontainers/image-spec/blob/v1.1.1/image-layout.md)
- [OCI Distribution Spec 1.1.1](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md)
- [OCI Distribution pull](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#pull)
- [OCI 可续传拉取](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#resumable-pull)
- [OCI Referrers API](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#listing-referrers)
- [OCI referrers 回退](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#unavailable-referrers-api)
- [OCI 注册表代理与凭据边界](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#registry-proxying)
- [OCI 1.1 镜像与分发版本概览](https://opencontainers.org/posts/blog/2024-03-13-image-and-distribution-1-1/)

### 注册表服务与身份验证

- [Docker Registry HTTP API V2](https://distribution.github.io/distribution/spec/api/)
- [注册表令牌身份验证](https://distribution.github.io/distribution/spec/auth/token/)
- [Distribution 拉取式缓存](https://distribution.github.io/distribution/recipes/mirror/)
- [Distribution 注册表配置](https://distribution.github.io/distribution/about/configuration/)
- [Distribution 垃圾回收](https://distribution.github.io/distribution/about/garbage-collection/)
- [Harbor 支持的注册表端点](https://goharbor.io/docs/main/administration/configuring-replication/create-replication-endpoints/)
- [Harbor 代理缓存](https://goharbor.io/docs/2.10.0/administration/configure-proxy-cache/)
- [GitHub Container Registry 身份验证](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry)

### 签名与证明

- [Cosign 验证](https://docs.sigstore.dev/cosign/verifying/verify/)
- [Cosign 签名规范](https://github.com/sigstore/cosign/blob/main/specs/SIGNATURE_SPEC.md)
- [`notation verify`](https://notaryproject.dev/docs/user-guides/cli-reference/notation_verify/)
- [Notary Project 信任存储与信任策略](https://github.com/notaryproject/specifications/blob/main/specs/trust-store-trust-policy.md)
- [Notary Project 签名规范](https://github.com/notaryproject/specifications/blob/main/specs/signature-specification.md)

