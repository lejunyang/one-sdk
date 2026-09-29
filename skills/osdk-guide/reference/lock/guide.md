# `osdk lock`

把项目工具请求解析为当前平台的精确版本并写入 `osdk.lock`。

```bash
osdk lock [TOOL[@VERSION]...] [-o KEY=VALUE]...
```

| 参数 | 语义 |
| --- | --- |
| `[TOOLS]...` | 为空时锁定项目 `[tools]`；给出时只处理点名请求 |
| `-o, --opt KEY=VALUE` | backend 选项，可重复 |

```bash
osdk lock
osdk lock node@20 python@3.12
osdk lock cargo:ripgrep@14 -o features=pcre2
```

只把项目声明或显式点名的工具写入 lock；用户全局选择不会混入项目 lock。各平台条目
分开保存。不要手写 lock 内的下载 URL、checksum、安装器身份或私有 replay metadata。

项目级 `use` 会在安装成功后自动更新对应工具及其受管 runtime 的 lock 条目；裸项目
`install` 在没有可用当前平台 lock、必须从配置解析时也会自动写入精确结果。显式
`install TOOL...` 仍是一次性安装。手动编辑 `[tools]` 后，或只想解析而不安装时，再显式
运行 `osdk lock`。

相关配置：`[tools]` 见 `reference/configuration/tools.md`。`model` 与 `skills` 有自己的
锁定/同步命令，不由此命令处理。
