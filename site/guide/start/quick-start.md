# 快速上手

下面这组命令覆盖从安装工具到在另一台机器上复现的完整流程。每条命令的完整说明见[安装、切换与查询](../basics/commands)，全局参数见[命令行全局参数](../config/cli-options)。

```bash
# 安装指定版本；前缀会解析为当前可用的最高匹配稳定版
osdk install node@20 python@3.12

# 安装并把用户输入的版本请求写入当前项目
osdk use node@20

# 生成并提交当前平台的解析结果
osdk lock

# 在另一台同平台机器按 lock 安装
osdk install

# 临时运行，不修改项目 pin
osdk exec --tool node@20 -- node --version
```

工具请求通常写成 `TOOL@VERSION`。省略 `@VERSION` 等价于 `latest`；常用请求包括
精确版本 `20.11.1`、前缀 `20`/`20.11`、`latest`/`current`/`stable`、`lts`、
`lts/iron` 和用户别名。不同 backend 支持的通道与范围并不完全相同。
