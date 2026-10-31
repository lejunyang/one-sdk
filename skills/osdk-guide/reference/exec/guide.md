# `osdk exec`

按需物化一个或多个工具，并用这些工具的精确环境运行一条命令。

```bash
osdk exec [-t <TOOL>...] [--no-deps] -- <COMMAND> [ARGS...]
```

| 参数 | 语义 |
| --- | --- |
| `-t, --tool <TOOL>` | 可重复；指定要暴露的工具请求。省略则使用当前目录自己的选择 |
| `--no-deps` | 跳过 `[deps]` 自动兑现 |
| `-- <COMMAND>...` | 原样作为程序与 argv 执行，不经 shell |

```bash
osdk exec --tool node@20 -- node --version
osdk exec -t rust@1.98.0 -t cargo:ripgrep@14 -- rg --version
osdk exec --no-deps -t python@3.12 -- python -c "print('ok')"
osdk exec -- ./gradlew build
```

**不带 `--tool`** 时，命令以激活的 shell 在当前目录下会有的环境运行（与 `hook-env`
同一份计算：backend 环境、下游缓存、模型 adapter、PATH），**不安装任何东西**：选中但
未安装的工具会像激活的 shell 一样报错，先 `osdk install`。PATH 以外层激活之前的那份
为基础；外层激活设置而当前目录不设置的变量恢复为激活前的值。该形式不需要 install
信任范围，`--no-deps` 时不需要任何信任。只需要导出环境而不运行命令时用
[`env`](../env/guide.md)。

这条一级命令与 Lua 任务里的 `exec(...)` 不是同一个入口：前者构造临时工具环境并继承
stdio，后者在任务脚本内捕获子进程输出。相关配置是 `[tools]` 和自动执行的 `[deps]`。

动态工具的裸命令名按所选安装清单映射到实际文件；例如 `prettier` 可以指向
`prettier.cjs`。显式路径保持不变；多个所选工具公开同名命令时应只选择其中一个。


显式配置 `tool_order=["system"]` 可使用受支持的已有 PATH/home 工具；默认不启用。显式版本及托管锁/frozen 仍保持权威。参见[system 配置、适配器和限制](../configuration/settings.md#system-执行来源)。

执行计划已提供宿主时，已知原生全局命令直接使用该宿主，不再次进入 shim 改用项目版本：

```bash
osdk exec --tool node@22 -- tsc --version
```

这适用于已装在受控全局目录中的应用，不会把原生包转换成 `npm:` receipt/lock 安装。
内嵌绝对解释器的原生 wrapper 保持原生语义；该命令不重写 launcher，也不保证跨 Node
版本的 native addon ABI。普通 shim 缺少运行时选择时给出提示，不任取已安装版本。
