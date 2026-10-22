# `osdk deps`

从项目原生 manifest/lockfile 兑现完整应用依赖闭包；与安装包管理器本身的 `install`
分开。

```bash
osdk deps [PROVIDER_OR_ROOTED_ID...] [OPTIONS]
```

| 参数 | 语义 |
| --- | --- |
| `--list` | 只列当前配置根的 provider 与 freshness |
| `--all` | 与 `--list` 合用，包含 `[deps].roots` 子项目 |
| `--dry-run` | 展示命令与过期原因，不执行 |
| `--force` | 忽略 freshness |
| `--explain` | 逐 provider 展示 freshness 判据 |
| `--skip <PROVIDER>` | 可重复跳过 |
| `-F, --filter <PATTERN>` | 按子项目路径过滤；`*` 不跨 `/`，无命中报错 |
| `--no-install-tools` | 缺包管理器时失败，不自动安装；适合 CI |
| `--frozen` | 必须存在原生 lock 且执行不得改写它 |
| `--verify` | 根据包管理器 receipt 深查已安装环境漂移 |

```bash
osdk deps --list --all
osdk deps --dry-run --explain
osdk deps --frozen --no-install-tools
osdk deps npm
osdk deps //:npm
osdk deps //apps/api:uv
```

裸 provider 名选当前目录最近 root；`//:name` 指配置根；`//path:name` 精确选子项目。
无操作数会处理全部声明 root。声明、provider 字段与信任规则见
`reference/configuration/deps.md`。

成功安装后写入 `osdk.lock` 的 `run` 使用跨平台规范命令名（如 `bun`、`cargo`），
不会记录仅供 Windows 查找可执行文件使用的 `.exe` / `.cmd` 后缀。

原生启动器按宿主平台选择；Unix 不会执行安装包内的 `.exe`、`.cmd` 或 `.bat` 启动器。

`deps.<provider>.run` 现在也覆盖内置命令；推荐 argv 数组，例如 `run = ["pnpm", "install", "--offline"]`，保留参数边界。覆盖命令需要 trust，无法保证 frozen 或禁用脚本。`timeout = "5m"` 限制每条命令及其进程树；`outputs = []` 关闭输出跟踪。旧 `index` / `extra_index` 已移除。请使用 `registries`；规则见 configuration/registries.md。

### 按项目声明依赖

子项目无需 osdk 配置。根配置选择 provider，并逐字段覆盖默认值：

```toml
[deps]
roots = ["apps/*"]
[deps.npm]
[deps.pnpm]
[deps.projects."services/api".pip-requirements]
auto = false
[deps.projects."apps/web"]
disable = ["pnpm"]
[deps.projects."apps/web".npm]
outputs = []
```

`auto = false` 仍可显式执行 `osdk deps`；`disable` 从执行计划移除 provider。列表替换默认值，空列表有效；env 按键合并。先根据清单/锁文件判断归属，再应用筛选；冲突锁或多个候选安装器报错。重叠 roots 按规范项目路径/provider 去重，从子目录调用也不会重复。重叠项目配置不一致时报错。`--filter .` 可选择根，其余过滤会排除根；`--list --all --explain` 展示选择来源、有效选项及命令。`[task].roots` 单独负责任务聚合。

pnpm、uv 原生 workspace 仅从显式声明的根展开，原生成员模式与排除规则决定成员范围，不自动合并无关嵌套项目。选择成员会明确说明扩大为一次共享根安装。uv workspace 使用 `uv sync --all-packages`，普通根 sync 不代表覆盖所有成员。共享选项（含 auto/disable、命令、索引）必须一致；冲突及成员独立锁文件报错。freshness 包含 workspace 定义、成员路径与清单、共享锁、有效配置和已安装工具版本。成功的原生安装覆盖范围记录在缓存 deps 状态中；自定义覆盖命令的范围无法验证。同一 osdk 缓存内按项目串行安装和记录状态。原生递归模式需要目录前缀（`packages/**`），拒绝裸 `**`。

可用 `osdk run deps-monorepo-live` 或 `python scripts/test-deps-monorepo.py --bin target/debug/osdk` 运行真实项目验证；使用临时状态与小 npm/Python 包，检查实际产物、共享覆盖、成员变化、继承、auto/禁用及锁/离线失败。子配置可以用显式 `disable = []` 替换继承的禁用列表。
