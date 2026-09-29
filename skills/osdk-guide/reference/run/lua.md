# `osdk run` 的内嵌 Lua

Lua 是任务的第四档逃生口：只在声明式步骤难以表达条件、循环、数据文件处理或跨平台
文件操作时使用。简单命令继续写 `run = "..."`，长脚本优先放独立脚本文件。

## 目录

- [最小写法](#最小写法)
- [返回与错误](#返回与错误)
- [任务上下文与短名](#任务上下文与短名)
- [执行命令](#执行命令)
- [路径与文件系统](#路径与文件系统)
- [JSON 与 TOML](#json-与-toml)
- [标准库、模块与边界](#标准库模块与边界)

## 最小写法

```toml
[tasks.prepare]
lua = """
local out = join(root, "dist", platform.os)
mkdir(out)
write(join(out, "version.txt"), "1.0\n")

local result = exec("git", "rev-parse", "HEAD")
if not result.success then
  print(result.stderr)
  return result.code
end
"""
```

较长脚本可保存在独立文件中，语义和可用 API 与内联 Lua 相同：

```toml
[tasks.prepare]
file = "scripts/prepare.lua"
```

osdk 按 `.lua` 扩展名选择内嵌解释器，不依赖系统 Lua、shebang 或执行位；默认任务目录中
自动发现的 `.lua` 文件也一样。

`lua` 与 `run` / `file` 互斥。它仍可配 `depends`、`wait_for`、`run_post`、`env`、
`dir`、`when`、`timeout`、参数声明和 freshness。

## 返回与错误

- 不返回、返回 `nil`、`true` 或 `0`：成功。
- 返回 `false`：失败，退出码 1。
- 返回其他数字：作为任务退出码。
- 抛错或 host API 失败：任务失败，错误包含任务名。
- 返回其他类型：硬错误，不静默转成成功。

Lua 指令、`run` / `sh` / `exec` 启动的子进程都受任务 `timeout` 约束；外部命令超时
终止整棵子进程树。`run_post` 随后仍执行。

## 任务上下文与短名

| 短名 | 完整名 | 内容 |
| --- | --- | --- |
| `root` | `osdk.project_root` | 项目配置根绝对路径 |
| `dir` | `osdk.dir` | 当前任务工作目录 |
| `task` | `osdk.task` | 当前任务名 |
| `args` | `osdk.args` | 已声明位置参数、option、flag 的名称到字符串值 |
| `argv` | `osdk.argv` | 未消费参数，Lua 1-based 数组；参数边界原样保留 |
| `platform` | `osdk.platform` | `{ os, windows, arch }` |
| `env(name)` | `osdk.env(name)` | 先读任务环境，再读父进程环境；缺失返回 nil |

`os.getenv` 已重定向到与 `env()` 相同的数据源。短名是便捷别名，`osdk.*` 完整形式
始终保留；脚本若自己需要同名全局，可使用完整形式并以 `local` 遮蔽短名。

## 执行命令

### 流式执行

```lua
local code = run("cargo", "test", "--workspace") -- 直接 argv，不经 shell
local code = sh("cargo test && cargo clippy")       -- cmd /c 或 sh -c
```

两者继承 stdin/stdout/stderr，只返回退出码。动态值优先传给 `run`，避免 shell 引用和
注入问题。`sh` 适合固定的一行 shell；Windows 使用 `cmd /c`，其他平台使用 `sh -c`。

### 捕获输出

```lua
local result = exec("git", "rev-parse", "HEAD")
if not result.success then
  print(result.stderr)
  return result.code
end
print(result.stdout)
```

结果字段：

| 字段 | 类型 | 语义 |
| --- | --- | --- |
| `code` | number | 子进程退出码 |
| `success` | boolean | `code == 0` |
| `stdout` / `stderr` | Lua string | 原始字节，可含非 UTF-8 |
| `stdout_truncated` / `stderr_truncated` | boolean | 对应流是否超过 4 MiB 保留上限 |

表形式在位置 argv、`argv={...}`、`command="..."` 中三选一；未知 option 硬报错：

```lua
local direct = exec {
  "tool", "--format", "json",
  cwd = "packages/app",           -- 相对任务 dir；也可绝对路径
  env = { MODE = "release" },     -- 覆盖任务环境
  stdin = "input\n",             -- 原始字节
  check = true,                    -- 非零退出直接抛错
}

local explicit = exec { argv = {"git", "status", "--short"} }
local piped = exec { command = "tool-a | tool-b" } -- 平台 shell
```

捕获达到 4 MiB 后停止保留但继续排空，避免管道写满导致死锁。需要不限量实时日志时用
`run` / `sh`。通常不再用 `os.execute` / `io.popen`：它们不自动套用任务的 cwd、环境、
PATH、timeout 和进程树终止语义。

## 路径与文件系统

路径短函数同时存在于 `path.*` / `osdk.path.*`：

- `join(...)`
- `exists(p)`、`is_file(p)`、`is_dir(p)`、`is_absolute(p)`
- `parent(p)`、`basename(p)`、`extension(p)`
- `absolute(p)`
- `relative(target, base?)`，base 默认任务 `dir`

文件短函数同时存在于 `fs.*` / `osdk.fs.*`：

- `mkdir(p)`：递归建目录。
- `read(p)`：读取原始字节为 Lua string。
- `write(p, bytes)`：创建父目录并覆盖写入。
- `copy(src, dst)`：复制文件或目录树，保留符号链接。
- `move(src, dst)`：创建目标父目录后原生 rename；源/目标需在同一文件系统。
- `remove(p)`：删除文件/符号链接/整棵目录；不存在返回 false。
- `glob(pattern)`：pattern 相对任务 `dir`，返回排序后的绝对路径数组；拒绝绝对 pattern。
- `which(program)` / `osdk.which(program)`：按任务 PATH 查可执行文件，缺失返回 nil。

除 `join`、`parent`、`basename`、`extension` 这类纯词法函数外，相对路径统一从任务
`dir` 解析。目录符号链接在 Windows 上受宿主权限限制。

## JSON 与 TOML

```lua
local package = json.decode(read("package.json"))
package.private = true
write("package.json", json.encode(package, true) .. "\n")

local config = toml.decode(read("tool.toml"))
config.release = { enabled = true }
write("tool.toml", toml.encode(config))
```

- `json.decode(text)` / `json.encode(value, pretty?)`。
- `json.null` 表示 JSON null。
- `json.array(table)` 标记数组，创建空数组必须用 `json.array({})`；从 JSON 解出的数组
  已带标记。
- `toml.decode(text)` / `toml.encode(value)`。
- codec 错误直接抛出，不返回半解析值。

完整形式为 `osdk.json.*` / `osdk.toml.*`。保留 `json` / `toml` 这一层是为了让
`decode` 的数据格式显式，不提供含义模糊的裸 `decode()`。

## 标准库、模块与边界

解释器是静态链接的 Lua 5.4，加载 `string`、`table`、`math`、`os`、`io`、
`coroutine`、`utf8`、`package` 等标准库，但不加载 `debug`。`require` 可加载纯 Lua
模块，不能加载 `lfs`、`socket` 等 C 扩展。

`io.open`、`os.remove`、`os.rename`、`os.execute` 和 `io.popen` 虽可调用，但它们按
osdk 进程本身的 cwd/环境工作，不会自动采用任务的 `dir` 与注入环境。跨平台任务优先
使用本页 host API。`os.getenv` 是例外：它已重定向到与 `env()` 相同的数据源。

Lua 运行时位于默认开启的 Cargo `scripts` feature 后；关闭该 feature 的 osdk 会拒绝
内联 Lua 和 `.lua` 文件任务，并提示改用其他脚本格式或启用该 feature。Lua 静态链接，
运行 osdk 无需系统 Lua；构建 osdk 需要 C 编译器。`osdk-shim` 不启用该 feature，
因此不携带 Lua 引擎。

这不是安全沙箱：同一份已信任配置本来就能通过普通 `run` 执行任意命令。host API 的
目标是提供一致的 cwd/env/argv/timeout 和跨平台语义，不是限制脚本权限。
