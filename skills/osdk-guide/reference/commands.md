# osdk 命令用法参考（按功能分类）

本文覆盖 `osdk` 的**全部一级命令**，按功能分组。命令面对齐
`crates/osdk-cli/src/cli.rs` 的 `enum Command`（实测版本 `osdk 0.0.3`）。每个命令的权威
帮助仍是 `osdk <command> --help`；本文补充的是分类、意图和易错点。

## 全局参数（放在子命令之前，对所有命令生效）

| 参数 | 作用 | 环境变量 |
| --- | --- | --- |
| `-v, --verbose...` | 增加日志详细度，可重复 | — |
| `-q, --quiet` | 抑制进度输出 | — |
| `-j, --jobs <N>` | 最大并发下载 / 安装数 | `OSDK_JOBS` |
| `--model-jobs <N>` | `model sync` 同时下载的模型数（默认 2）；独立于 `--jobs`（单模型内文件并发），二者相乘 | `OSDK_MODEL_JOBS` |
| `-y, --yes` | 对提示默认「是」 | — |
| `--source <ID>` | 本次调用强制使用某个源 id | — |
| `--refresh-sources` | 忽略缓存的测速结果，重新探测源 | — |
| `--source-mode <auto\|env>` | 环境里的镜像变量是参与测速（auto）还是原样遵循（env） | `OSDK_SOURCE_MODE` |
| `--offline` | 禁用网络，只用缓存的元数据 / 产物 | `OSDK_OFFLINE` |
| `--require-checksums` | 拒绝没有可校验 checksum 的产物 | `OSDK_REQUIRE_CHECKSUMS` |
| `--attestations <off\|if-available\|required>` | GitHub 产物 attestation 策略 | `OSDK_ATTESTATIONS` |
| `--prerelease <never\|if-explicit\|allow>` | 预发布版本解析策略 | `OSDK_PRERELEASE` |
| `--lang <en\|zh>` | 覆盖单次命令的输出语言 | `OSDK_LANG`（会话级） |

用法：`osdk [全局参数] <命令> [命令参数]`，例如 `osdk --jobs 4 install node@20 python@3.12`。

## 工具标识与命名空间

许多命令接受「工具请求」，形如 `<tool>[@<version>]`：

- **内置 backend（固定名字）**：`node`、`npm`、`pnpm`、`yarn`、`go`、`python`、`java`、
  `maven`、`gradle`、`kotlin`、`rust`、`deno`、`bun`、`zig`，以及各 Android 包族
  （`android-ndk`、`android-platform-tools`、`android-build-tools`、
  `android-cmdline-tools`、`android-cmake`、`android-platforms`、`android-emulator`、
  `android-sources`、`android-system-images`）。
- **动态 backend（命名空间前缀 + subject）**，前缀取自 `DYNAMIC_NAMESPACES`：
  - `npm:<package>` — npm 发布的 CLI 包（如 `npm:prettier@3`、`npm:@antfu/ni`）
  - `cargo:<crate 或 https git URL>` — Registry crate 或 HTTPS Git 仓库
  - `go:<module 或 command path>` — Go command package（如 `go:golang.org/x/tools/gopls@0.20.0`）
  - `conda:<package>` — conda 包 / CUDA 等工具链
  - `pypi:<project>` — Python CLI（每工具一个隔离 venv）
  - `github:owner/repo` — 公开 GitHub Release
  - `http:https://...{version}...[sha256=...]` — checksum 锁定的直接 HTTPS 制品
- **版本选择子**：`@20`（数字前缀）、`@=android-36`（`=` 精确锁定不漂移）、`latest`、
  `tag:<ref>` / `branch:<ref>` / `rev:<40 位 hex>`（Git 类）等，具体因 backend 而异。

---

## 一、工具生命周期

| 命令 | 别名 | 作用与要点 |
| --- | --- | --- |
| `install [tools...]` | `i` | 安装一个或多个工具；不带参数则按解析后的项目配置安装。`-o KEY=VALUE` 传 backend 专属选项（可重复），对列出的所有工具生效；`--force` 即使已装也重装（修复 `doctor --verify` 报告的漂移）；`--no-deps` 跳过本次的 `[deps]` 兑现 |
| `use <tool>` | `u` | 装（若需要）并设为生效版本。`-g, --global` 用全局选择；`-o KEY=VALUE` 传选项。Node 项目里 `use npm:<pkg>` 会写进最近的 `package.json` |
| `uninstall <tool>` | `rm` | 卸载某个工具版本；`-g, --global` 卸载用户全局 npm 包安装（其他 backend 不支持全局卸载） |
| `list [tool]` | `ls` | 列出已安装版本，可限定单个工具 |
| `list-remote <tool> [filter]` | `lsr` | 从远端索引列出可安装版本；`filter` 按前缀过滤（如 `20`） |
| `current [tool]` | — | 显示当前目录对每个工具实际生效的版本 |
| `where <tool>` | — | 打印某工具版本的安装目录。`-g, --global` 从全局作用域解析 npm 包；`--bins` 额外列出发布的命令及被保留的命令 |
| `reshim` | — | 为所有已安装工具重新生成 shim 启动器 |
| `alias set/list/unset` | — | 管理用户自定义版本别名，如 `osdk alias set node default 20` |

常见组合：
```bash
osdk --jobs 4 install node@20 python@3.12 go@1.22   # 并发安装多个
osdk use -g node@20                                  # 用户级默认
osdk use python@3.12                                 # 当前项目固定
osdk current                                         # 看当前目录生效版本
osdk where node --bins                               # 看安装路径和发布的命令
```

## 二、项目复现（lock / install / outdated / upgrade）

| 命令 | 作用与要点 |
| --- | --- |
| `lock [tools...]` | 解析项目工具并把精确版本写入 `osdk.lock`；不带参数解析当前项目配置。`-o KEY=VALUE` 传选项。**只锁项目自身声明的工具**，全局固定版本不进 lock；`osdk lock <tool>` 可点名把某工具一起锁 |
| `install`（不带参数） | 按解析后的配置安装当前平台的锁定结果（见「一、工具生命周期」） |
| `outdated [tools...]` | 显示已装版本与当前远端解析结果不同的工具 |
| `upgrade [tools...]` | 安装符合约束的最新版本并更新 `osdk.lock`；`-o KEY=VALUE` 传选项 |

```bash
osdk use node@20 && osdk use python@3.12 && osdk lock && osdk install
osdk outdated
osdk upgrade
```
> Rust 若要不可变环境，固定明确版本或带日期的 toolchain；`stable/beta/nightly` 等浮动
> channel 写入 lock 后仍会随上游更新。

## 三、临时执行与项目任务

| 命令 | 作用与要点 |
| --- | --- |
| `exec --tool <t> [--tool <t2>] -- <cmd...>` | 按需装好工具，再用其精确环境跑命令。`-t/--tool` 必填、可重复；`--no-deps` 跳过 `[deps]`；`--` 后是命令及其参数 |
| `run <task> [-- args...]` | 跑 `[tasks]` 里定义的任务。`--dry-run` 只打印将执行什么；`--no-deps` 跳过 `[deps]`；`--` 后透传参数给任务 |
| `task list [--hidden]` | 列出可用任务（`--hidden` 含 `hide = true` 的） |
| `task info <task>` | 显示某任务解析后的定义 |
| `task deps <task>` | 打印某任务的执行顺序 |
| `task add <name> -r <cmd> [-d desc] [--depends <t>]` | 向项目配置添加任务；`-r/--run` 必填、可重复表示序列 |
| `task rm <name>` | 从项目配置删除任务 |
| `task edit <name>` | 在 `$EDITOR` 中定位到该任务打开项目配置 |

```bash
osdk run ci                 # 跑 [tasks] 里的 ci
osdk run ci --dry-run       # 只看会执行什么
osdk task list
osdk exec --tool node@20 -- node --version
```
> 只有 `osdk run <name>`，没有裸 `osdk <name>`。任务写法见 `configuration.md` 的 `[tasks]`。

## 四、应用依赖（deps）

`osdk deps [providers...]` —— 从项目自身清单（`package.json`、`pyproject.toml`、
`requirements.txt`、`go.mod` 等）把整份依赖闭包装进项目。声明在 `[deps.<provider>]`。

| 参数 | 作用 |
| --- | --- |
| `--list` | 列出探测到的 provider 与新鲜度，不安装（默认只列当前配置根） |
| `--all` | 与 `--list` 合用，包含 `[deps].roots` 展开的所有子项目 |
| `--dry-run` | 报告将执行什么命令、为何过期，不执行 |
| `--force` | 忽略新鲜度强制重跑 |
| `--explain` | 逐 provider 打印新鲜度判定依据 |
| `--skip <PROVIDER>` | 跳过这些 provider（可重复） |
| `-F, --filter <PATTERN>` | 按**路径**限定子项目（可重复），如 `--filter 'apps/*'`；匹配不到即报错 |
| `--no-install-tools` | 缺包管理器时报错而不是自动装（CI 复现用） |
| `--frozen` | 要求原生 lockfile 存在且不被修改，否则失败（CI） |
| `--verify` | 按包管理器自己的收据（`dist-info/RECORD`、`node_modules/.package-lock.json`）深度校验已装环境 |

```bash
osdk deps --list
osdk deps --dry-run
osdk deps                 # 兑现整份清单（stale 才动）
osdk deps --frozen        # 严格：无原生 lock 即失败
osdk deps --verify
osdk deps npm             # 当前工作目录最近的 npm root
osdk deps //:npm          # 配置根
osdk deps //apps/api:npm  # 指定子项目
```
> 无 provider 操作数时覆盖所有声明 root；裸 provider 名只命中最近 root，完整 `//路径:provider` 精确寻址。
>
> `pip-requirements` 使用 `uv pip install -r` 解析传递依赖；普通 `requirements.txt`
> 不是完整 lock，不能与 `--frozen` 合用，也不会按集合语义清理额外包。
>
> 声明后，裸 `osdk install` / `osdk run` / `osdk exec` 会先做哈希新鲜度快判、过期才兑现；
> 单次跳过用 `--no-deps`，永久关闭某 provider 用 `auto = false`（见 `configuration.md`）。

## 五、生态专属工作流

| 命令 | 作用 |
| --- | --- |
| `node migrate-packages --from <v> --to <v> [--apply]` | 在受管 Node 版本间迁移全局 npm 包；不带 `--apply` 只出计划 |
| `python find [request]` | 找受管 / PATH / 系统 Python 解释器，可带请求如 `pypy-3.11`、`3.14+freethreaded` |
| `rust component add/remove/list [--toolchain <t>]` | 管理 rustup 组件（默认 toolchain `stable`） |
| `rust target add/remove/list [--toolchain <t>]` | 管理 rustup 目标 |
| `rust check [--repair]` | 检查更新并从隔离 rustup 状态修复 osdk 标记 |
| `rust override import/export [path]` | 显式导入 / 导出 rustup 目录 override |
| `rust toolchain link <name> <path>` | 把本地 toolchain 路径按 osdk/rustup 名字链接 |
| `android licenses show/status/export` | 查看协议全文 / 已接受记录 / 导出接受记录给 Gradle 复用 |
| `android sdk-root show/repair` | 查看 / 重建 Google 工具读取的共享 SDK 根索引 |
| `android avd list/create/delete` | 不经 avdmanager 管理 Android 虚拟设备 |

```bash
osdk android licenses show android-ndk@29.0.14206865
osdk install android-ndk@29.0.14206865 -o accept-licenses=true
osdk android avd create pixel-35 --image "android-35;google_apis;x86_64"
osdk rust target add x86_64-pc-windows-gnu --toolchain stable
```
> osdk 不会替你接受 Android 协议：未传接受选项时安装会在下载任何内容前停止。

## 六、下载源、Registry、信任与安全

| 命令 | 作用 |
| --- | --- |
| `source list <tool>` | 列出某工具的源（含上次测速结果） |
| `source test <tool> [--model <repo>]` | 立即探测并打印速度排名；模型 provider 用 `--model` 指定仓库 |
| `source add <tool> --id <id> --download-url <url> [--index-url <url>] [--forward-credentials]` | 为某工具加自定义源 |
| `source remove <tool> <id>` | 删除自定义源 |
| `source pin <tool> <id>` / `source unpin <tool>` | 固定 / 取消固定首选源（pin 是「优先尝试」非「只用这一个」） |
| `registry test [manager]` | 探测依赖 Registry 并显示选择计划；省略 manager 测全部 |
| `trust [path] [list\|prune]` | 信任会影响执行或下载源的项目配置；`trust list` 列出已信任的；`trust prune [--dry-run]` 清理文件已不存在的记录 |
| `untrust [path]` | 取消对某项目配置的信任 |

```bash
osdk source test node
osdk source pin node tuna
osdk --source official install go@1.22
osdk --yes trust ./osdk.toml
osdk trust list
```
> 仅声明「装哪些工具/包」不需要 trust；被拒绝时 osdk 会逐条列出是哪些键、各自原因。
> 特殊源名：`self`（osdk 自身更新源）、`go-modules`（`GOPROXY`，与工具链源 `go` 独立）。

## 七、模型快照（model）

| 命令 | 作用 |
| --- | --- |
| `model pull <name> [reference] [--include/--exclude glob] [--variant v] [--endpoint u] [--forward-credentials] [--no-lock]` | 解析并下载不可变模型快照；省略 `reference` 时读取同名 `[models.<name>].source`，显式参数覆盖声明 |
| `model sync [--prune] [--dry-run]` | 无参拉取整个项目的模型：`[models]` 中 lock 未描述的声明会被拉取并写入 lock，`source`/`variant` 与 lock 不一致的声明会重新拉取并改写条目，其余按 lock 复现（已存在且校验通过的跳过）；手动新增或修改 `[models]` 无需再单独 `model pull`；`--prune` 删除 lock 不再声明的本地快照 |
| `model list` | 列出本地已物化快照 |
| `model path <name> [--stable]` | 打印当前快照路径；`--stable` 打印稳定 `current` 路径（写进 ComfyUI / llama.cpp / 脚本用这个） |
| `model verify <name>` | 校验某快照全部文件 |
| `model remove <name> [--keep-lock]` | 删除某逻辑模型的全部本地快照 |
| `model env enable/disable/list [provider]` | 管理 shell 激活导出的 provider 环境变量（provider 省略则对两者生效） |
| `model view add/list/path/rebuild/remove/export/doctor` | 渲染并管理消费者形状视图（`comfyui` / `hf-cache`），链接回快照不复制权重 |

```bash
osdk model pull qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main --include '*.safetensors'
osdk model path qwen25 --stable
osdk model view add comfyui qwen25 --map unet/=diffusion_models
osdk model sync
```

## 八、Agent Skills（skills）

`osdk skills` 安装 `SKILL.md` 包并链接进各 AI 编码 Agent 的 skills 目录。skill 是给外部
Agent 读的内容，osdk 只负责下载、内容寻址落地与链接，自己不执行 skill 里的任何脚本。

| 子命令 | 作用 |
| --- | --- |
| `add <SOURCE> [-s <NAME>...] [-a <ID>...] [-g] [--copy] [--ref <REF>] [-l\|--list] [--no-lock]` | 从来源安装 skill 到一个/多个 Agent。来源：`github:owner/repo`（可带子目录 `/skills/<名>`）、`owner/repo` 简写、github.com URL、本地路径。`-s` 选装仓库内指定 skill（`*` 全选）；`-a` 选目标 Agent（缺省用 `[skills].default_agents`）；`-g` 装到用户级；`--copy` 拷贝而非链接；`--ref` 指定 GitHub 版本（`branch:main`/`tag:`/`rev:`/commit）；`-l` 只列不装；`--no-lock` 不写 lock |
| `list [-g]` | 列出已装 skill 及其链接到的 Agent（读 `osdk.lock`） |
| `find [QUERY...] [--owner <OWNER>] [--limit <N>]`（别名 `search`） | 在 GitHub 上搜可安装的 skill 仓库（含 `SKILL.md` 的仓库）。匿名请求 GitHub 公开搜索 API，撞匿名限流才回退到 `GITHUB_TOKEN`/`GH_TOKEN`；**不接触 skills.sh**。命中以 `owner/repo` 打印，可直接交给 `add`。`--owner` 限定某 org/user，`--limit` 限结果数（1–50，默认 20） |
| `remove <NAME> [-g] [-a <ID>...]` | 从 Agent 摘除 skill；不带 `-a` 摘除全部并删 lock 条目，带 `-a` 只摘指定 Agent 并保留其余 |
| `sync [-g]` | 按 `osdk.lock` 复现全部 skill（团队 / CI）：优先用已落地的内容寻址副本，缺副本时对 GitHub 源按记录的 commit 重新下载并核对内容哈希 |
| `update [SKILL...] [-g]` | 与 `sync` 相对：`sync` 复现 lock 记录的 commit，`update` 把浮动 ref（分支/标签，取自 `[skills.<名>].ref`）重新解析到当前 commit，变了才重下并写回 lock；钉死 commit 的无可更新 |
| `use <SOURCE> [-s <NAME>] [-a <ID>] [--ref <REF>]` | 不安装、不写 lock，临时取用一个 skill：无 `-a` 时把生成的 prompt 打到 stdout（可 `\| claude` 管道），`-a <id>` 时用该 Agent 的 CLI 交互式启动并带上 prompt |
| `init [NAME]` | 生成 `SKILL.md` 模板开始写自己的 skill；`NAME/`（或当前目录），拒绝覆盖已存在的 `SKILL.md` |
| `path <NAME>` | 打印某已装 skill 的内容寻址落地路径 |
| `agents` | 列出 osdk 认识的 Agent 及其 project / global skills 目录 |

```bash
osdk skills agents
osdk skills find agent skills --limit 10                        # 搜 GitHub（匿名）
osdk skills find --owner vercel-labs                            # 浏览某 owner 的 skill
osdk skills add github:vercel-labs/agent-skills --list          # 只列，不装
osdk skills add github:vercel-labs/agent-skills/skills/web-design-guidelines -a claude-code
osdk skills add ./my-skills -s my-skill -a codex                # 本地源
osdk skills list
osdk skills sync                                                # 按 lock 复现
osdk skills remove web-design-guidelines
```

- **不可变身份**：`add` 把解析到的 commit 与内容哈希写进 `osdk.lock [skills.<名>]`，`sync` 据此
  在别的机器复现；重新下载时哈希不符会 fail-closed 拒绝（防移动的 tag / 被换的镜像）。
- **落地方式**：默认目录链接（Windows junction / Unix symlink），无链接环境或 `--copy` 时整树拷贝；
  拒绝覆盖非 osdk 放置的真实目录。
- **只读命令**（`agents` / `list` / `path` / `find`）不触发信任门槛；`add` / `remove` / `sync` / `update` 会写盘、保持
  gated。声明式配置见 `configuration.md` 的 `[skills]`。
## 九、容器运行时（container）

`osdk container` 只检查与操作**宿主原生**运行时，不把镜像搬进 osdk 存储。

| 子命令 | 作用 |
| --- | --- |
| `pull <IMAGE> [--runtime auto\|docker\|containerd] [--platform OS/ARCH] [--address <a> --namespace <n>]` | 经一个选定原生运行时拉镜像；containerd 需成对给 `--address` 与 `--namespace` |
| `doctor [--runtime <r>] [--builder <b>] [--json]` | 诊断选中的原生运行时与 Buildx builder |
| `registry test <REGISTRY> [--image <i>] [--platform <p>] [--json]` | 匿名测试 OCI Registry 及其配置 / 内置镜像 |
| `mirrors plan <REGISTRY> --runtime <docker\|containerd\|buildkit> [...]` | 规划某 Registry 的原生镜像配置，不写入 |
| `mirrors apply <REGISTRY> --runtime <r> --native-config <path> [--accept-plan <sha256>] [--dry-run] [--json]` | 测速 + 规划 + 原子写入；无人值守 `--yes` 必须带本次 `--accept-plan` |
| `cache status [--runtime <r>] [--builder <b>] [--json]` | 报告原生缓存用量 |
| `prune --runtime <docker\|buildkit\|containerd> --scope <images\|build-cache> [--execute --accept-preview <sha256>]` | 严格限定范围的原生清理；执行需同时给 `--execute` 与预览返回的 `--accept-preview` |

```bash
osdk container doctor --json
osdk container registry test docker.io --image ubuntu:24.04 --platform linux/amd64
osdk container pull ubuntu:24.04
osdk container prune --runtime docker --scope images
```

## 十、宿主包管理器（pkg，只读为主）

| 子命令 | 作用 |
| --- | --- |
| `pkg doctor [--json]` | 报告宿主有哪些系统包管理器（如 winget）及其状态；只读 |
| `pkg status [--missing] [--json]` | 按 `[syspkg.packages]` 对比宿主；`--missing` 缺包即退出非零（CI）；只读 |
| `pkg plan [--json] [--detailed-exitcode]` | 显示将安装哪些包，不安装 |
| `pkg apply [--dry-run] [--yes] [--json]` | **唯一会改系统**的包命令：装 `[syspkg.packages]` 要求且宿主缺失的包；已存在的原样保留 |
| `pkg mirrors test [--manager winget] [--json]` | 测速各已知镜像并排名；只测不改 |
| `pkg mirrors apply [--manager winget] [--dry-run] [--accept-plan <fp>] [--json]` | 把最快镜像写进包管理器配置；改机器状态、需管理员、首次 apply 需 `--accept-plan` |

```bash
osdk pkg status            # 缺什么
osdk pkg apply --yes       # 装上（唯一改系统的）
osdk pkg mirrors test
```

## 十一、存储与缓存（cache / prune）

| 命令 | 作用 |
| --- | --- |
| `cache dir` | 打印共享缓存目录 |
| `cache env` | 打印下游包管理器缓存重定向 |
| `cache clean` | 删除已下载归档和 uv/pip 缓存（保留 CAS store 与安装） |
| `cache prune` | 只删除没有任何环境再引用的缓存条目（比 clean 保守，无预览模式） |
| `prune [--dry-run]` | 回收 store 里不再被引用的对象；`--dry-run` 不删除 |

```bash
osdk cache dir
osdk --yes cache clean
osdk prune --dry-run
```

## 十二、Shell 集成、自身管理与诊断

| 命令 | 作用 |
| --- | --- |
| `activate <shell>` | 打印 shell 集成代码供 eval，如 `eval "$(osdk activate bash)"`；支持 bash / zsh / fish / PowerShell |
| `deactivate <shell>` | 打印移除 osdk 集成并还原环境的 shell 代码 |
| `completions <shell>` | 生成 shell 补全代码 |
| `config path` | 打印解析后的配置目录 / 文件路径 |
| `config list` | 打印解析后的全部设置 |
| `config get <key> [-g]` | 读一个设置；`-g` 读用户全局配置 |
| `config set <key> <value> [-g]` | 写一个设置到项目配置（`-g` 写全局）；列表型接受逗号分隔值 |
| `config unset <key> [-g]` | 删一个设置，恢复默认 |
| `self upgrade [--version <v>] [--dry-run] [--force]` | 下载最新（或指定）release 并替换当前安装；`osdk` 与 `osdk-shim` 一起替换 |
| `doctor [--verify] [--tool <t>]` | 诊断目录 / 镜像 / 同盘 / link mode / 代理；`--verify` 重新哈希每个已装文件找出与安装时不一致的（慢，可 `--tool` 限定） |

```bash
osdk config path
osdk config get jobs
osdk config set shims.exclude "apkanalyzer"
osdk doctor --verify --tool node
osdk --lang en doctor
osdk self upgrade --dry-run
```

---

## 退出码约定（部分命令）

- 一般命令：成功 0，失败非零；参数错误打印用法后退出码 2。
- `pkg status --missing`：有缺包时退出非零。
- `pkg plan --detailed-exitcode`：会改动则退 2，不会改动则退 0。
- `container` 直接 `pull`：透传子进程退出码；Unix 上被信号终止规范化为 `128 + signal`。

## 与配置的对应关系

大量命令行为等价于写进 `osdk.toml` / `config.toml` 的声明（如 `use` ≈ `[tools]`、
`source pin` ≈ `[sources.<tool>].pin`、`config set` 写 `[settings]`）。命令是「一次性动作或
即时查询」，配置是「持久声明」。要写持久声明，见 `reference/configuration.md`。
