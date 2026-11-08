# `osdk upgrade`

解析符合请求约束的当前远端版本，安装它们并更新项目 `osdk.lock`。

```bash
osdk upgrade [TOOL[@VERSION]...] [-o KEY=VALUE]...
osdk upgrade -g|--global [TOOL...]
```

| 参数 | 语义 |
| --- | --- |
| `[TOOLS]...` | 为空时升级项目工具；给出时限定请求 |
| `-g, --global` | 只升级用户全局选择并 upsert 用户 lock，忽略项目；与 `-o` 互斥 |
| `-o, --opt KEY=VALUE` | backend 选项，可重复 |

```bash
osdk outdated
osdk upgrade
osdk upgrade node@20
```

浮动请求先通过 backend 重新解析，再按精确版本与动态安装身份复用本地产物；
不会因为本地已有旧匹配版本而跳过解析，也不会强制重装已满足的精确目标。
离线只能使用已有 metadata 或本地 catalog，不能确认上游最新。

`upgrade` 会改变已安装内容和 lock；`lock` 只解析/写 lock，`install` 负责复现现有 lock。
浮动 Rust channel 仍会随上游移动；严格复现应写明确版本或带日期 toolchain。

相关配置：`[tools]`、`[sources]`、`[settings].prerelease`。

## 全局升级

```bash
osdk upgrade -g github:lejunyang/ai-auto-desktop
osdk upgrade -g aad  # 唯一归属于已安装全局选择的命令
osdk upgrade -g npm:prettier
osdk upgrade --global
```

工具名和已配置别名优先匹配；否则接受已安装全局选择实际导出、且归属唯一的命令名。
例如 `aad` 可定位 `github:lejunyang/ai-auto-desktop`，不会按其他包的名字或后缀猜测。
多个选择导出同一命令时拒绝操作，列出候选，要求完整 ID 或配置别名。遵守 shim 的
包含/排除设置；不匹配非全局选择、过期安装版本或 PATH 上的外部程序，也不会执行探针。
命令名同样不能带 `@VERSION` 或选项；仅 `-g/--global` 模式支持命令名回查。
规则同样适用于 Cargo、Go、PyPI、Conda、HTTP 和静态/声明式 backend。HTTP 的精确
版本、必填 `sha256` 和布局选项从用户配置继承（含配置别名），无需在升级参数中重复。
HTTP 不发现新版本，也不自动替换摘要；选择新制品时须一起更新版本与 SHA-256。

无参数升级全部适用选择（包括 lazy），排除不匹配
平台的 `when` 条目。保留原约束、选项、`when`、`lazy`，精确 pin 不会越界升级；修改
约束/选项用 `osdk use -g TOOL@VERSION -o KEY=VALUE`。不读取项目配置、版本文件或 lock，
不要求信任当前项目。只更新 `config.toml` 旁的用户 `osdk.lock`，保留其他工具和平台。
Go/Cargo/独立 JS manager 的受管 runtime 需有对应全局选择。`npm:` 继续使用受控
全局 prefix、原安装器和原生 lock，不进入项目安装路径。批量操作允许部分成功。
