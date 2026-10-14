# `[deps]`

声明项目应用依赖 provider；它不同于 `[tools]`：后者安装包管理器，前者让包管理器读取
`package.json`、`pyproject.toml`、`requirements.txt`、`go.mod` 等并兑现项目依赖。

```toml
[deps]
disable = ["yarn"]
roots = ["apps/*", "packages/*"]

[deps.pnpm]
auto = true
sources = ["package.json", "pnpm-lock.yaml"]
outputs = ["node_modules/.package-lock.json"]
run = "pnpm install --frozen-lockfile"
env = { NODE_ENV = "development" }
dir = "."
depends = ["proto"]
timeout = "5m"
index = "https://mirror.example/simple/"
allow_build_from_source = false
```

- 空 `[deps.<provider>]` 即启用内置 provider，`auto` 默认 true。
- `disable` 可覆盖更低层配置，但不卸载已有依赖。
- `roots` 显式列出 monorepo 子项目；不做无界递归发现。
- `sources` 替换默认输入；`outputs = []` 显式关闭输出跟踪。
- `index` 改变来源，需要 trust；`extra_index` 不支持并报迁移错误；`allow_build_from_source = true`
  允许运行构建或生命周期脚本，需要更强的 ExecutesCode trust。
- provider 对象拒绝未知字段，拼写错误不会静默忽略。

支持的 provider 包括 `npm`、`pnpm`、`yarn`、`bun`、`deno`、`go`、`cargo`、`uv`、
`pip-requirements`。普通 requirements 文件不是完整 lock，不能与 `--frozen` 组合。

`deps.<provider>.run` 现在也覆盖内置命令；推荐 argv 数组，例如 `run = ["pnpm", "install", "--offline"]`，保留参数边界。覆盖命令需要 trust，无法保证 frozen 或禁用脚本。`timeout = "5m"` 限制每条命令及其进程树；`outputs = []` 关闭输出跟踪。`installer` 已废弃且必须与 provider 名一致，请删除。`extra_index` 现在明确报错，请迁移到原生 registry 配置；uv 优先使用 `pyproject.toml` 的命名索引与包绑定，不保证通用低优先级索引。

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
