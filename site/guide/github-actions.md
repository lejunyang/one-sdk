# GitHub Actions

仓库根目录的 `action.yml` 提供了一个 composite `setup-osdk` Action。它直接放在
one-sdk 仓库，而不另建仓库：Action、Release 安装器、CLI 参数与缓存布局共同组成一份
需要同步演进的版本契约。只有未来需要独立发布节奏或长期维护移动的 major tag 时，拆仓才
有额外价值。

## 快速使用

必须先 checkout：Action 需要读取 `osdk.lock`、项目配置和原生依赖 lock 来计算缓存 key
并初始化环境。

```yaml
steps:
  - uses: actions/checkout@v4
  - uses: lejunyang/one-sdk@main
  - run: osdk run ci
```

生产 workflow 应把 `main` 换成 Release tag 或完整 commit SHA。未填写 `version` 时，
如果 Action 自身使用 `v0.0.4` 这类 semver tag，就安装同版本 osdk；通过分支或 commit
引用 Action 时则安装最新 Release。

默认依次执行：

1. 安装 Release 中的两个二进制并校验 checksum，不修改 shell profile；
2. 恢复 osdk data 与 cache 目录；
3. 执行 `osdk install --no-deps`；
4. 执行 `osdk deps --frozen --no-install-tools`；
5. 把项目选中的工具、runtime 变量和包管理器缓存变量导出给后续 steps。

CI 默认使用 `--no-install-tools` 是刻意的：包管理器应来自已审阅的项目工具配置或 lock。
如果希望依赖 provider 自行获取缺少的包管理器，设置
`allow-deps-tool-install: true`。

## 参数

| 参数 | 默认值 | 作用 |
| --- | --- | --- |
| `version` | Action 的 semver tag，否则 `latest` | 要安装的 osdk Release |
| `repository` | `lejunyang/one-sdk` | 发布 Release 产物的仓库 |
| `download-base-url` | `https://github.com` | GitHub 或可信下载镜像根地址 |
| `target` | 自动检测 | 覆盖 Release target triple |
| `working-directory` | `.` | 要初始化的项目目录 |
| `cache` | `true` | 恢复/保存 osdk data 与包管理器缓存 |
| `cache-key` | `default` | 额外的缓存分区，可传 GitHub expression |
| `install-tools` | `true` | 安装项目工具 |
| `install-deps` | `true` | 安装声明的应用依赖 |
| `frozen` | `true` | 要求每个依赖 provider 都有原生 lock |
| `allow-deps-tool-install` | `false` | 允许 `deps` 获取缺失的包管理器 |
| `jobs` | osdk 默认值 | 并发下载/安装数 |
| `source-mode` | `auto` | `auto` 测速排序；`env` 强制使用环境镜像配置 |
| `offline` | `false` | 缓存恢复后禁止联网 |
| `require-checksums` | `false` | 拒绝没有可验证 checksum 的项目工具制品 |
| `attestations` | `if-available` | `off`、`if-available` 或 `required` |

Action 输出 `osdk-version`，以及底层 cache Action 的 `cache-hit`。

monorepo 子项目示例：

```yaml
- uses: lejunyang/one-sdk@main
  with:
    working-directory: apps/api
    jobs: 4
    cache-key: ${{ hashFiles('tooling/company-policy.lock') }}
```

若只想安装 osdk，并导出缓存中已经存在的项目环境，可关闭两段物化：

```yaml
- uses: lejunyang/one-sdk@main
  with:
    install-tools: false
    install-deps: false
```

## 缓存了什么

缓存位于 runner 临时目录下、由 Action 独占的 `OSDK_DATA_DIR` 和 `OSDK_CACHE_DIR`。
两者覆盖工具安装、内容寻址 store、rustup/Cargo 状态、Release 下载、来源元数据，以及
osdk 为 npm、pnpm、Yarn、Bun、uv/pip、Go、Cargo、Gradle、Deno 分配的原生缓存。

缓存 key 包含 runner OS、架构、实际安装的 osdk 版本、自定义 `cache-key`，以及所有受支持
项目 manifest/lock 的摘要。key 变化时允许按前缀恢复旧快照：osdk 仍会选择请求的身份并
忽略不匹配的缓存版本，未变化的内容则可以复用。不同 OS 与架构绝不会混用。

不会缓存 `node_modules`、`.venv` 这类项目产物；缓存的是它们背后的包管理器下载缓存，
再由 frozen install 重建项目树，避免恢复一份不透明且可能过期的工作区快照。

内嵌 cache Action 固定到 `actions/cache` v4.3.0 的完整 commit。v4 使用 GitHub 当前缓存
服务，同时不像基于 Node 24 的 v5/v6 那样要求较新的 self-hosted runner。

恢复出的工具安装缓存应视作受信任 CI 状态。osdk 的普通复用会检查安装身份与完成标记，
但不会在每次调用时重新哈希一个已完成的静态工具。GitHub 会按 key 与分支隔离缓存，fork
PR 也不能写入基础仓库缓存；不要把 `pull_request_target`、不受信任的 checkout 与 secret
放在同一个 job。若这个信任边界不适用，设置 `cache: false` 或轮换 `cache-key`。

## 严格复现示例

```yaml
permissions:
  contents: read

steps:
  - uses: actions/checkout@v4
  - uses: lejunyang/one-sdk@main
    with:
      frozen: true
      allow-deps-tool-install: false
      require-checksums: true
      attestations: required
  - run: osdk run test
```

只有确认恢复的缓存完整时才适合设置 `offline: true`；它会让缺失对象直接失败，而不是回退
联网下载。
