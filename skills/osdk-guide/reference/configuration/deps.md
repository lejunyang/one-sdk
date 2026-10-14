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
