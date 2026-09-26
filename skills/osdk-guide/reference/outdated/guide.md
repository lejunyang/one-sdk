# `osdk outdated`

比较已安装版本与当前远端解析结果，不执行升级。

```bash
osdk outdated [TOOL[@VERSION]...]
```

- 不带参数：检查当前项目 `[tools]`。
- 带参数：只检查指定请求。
- 受 `--offline`、`--source`、`--refresh-sources`、`--prerelease` 等全局选项影响。

```bash
osdk outdated
osdk outdated node@20 rust@stable
```

相关配置：`[tools]`、`[sources]`、`[settings].prerelease`。需要安装并更新 lock 时改用
`osdk upgrade`；只看可安装版本列表用 `osdk list-remote`。
