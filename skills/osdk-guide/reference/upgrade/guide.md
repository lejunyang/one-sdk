# `osdk upgrade`

解析符合请求约束的当前远端版本，安装它们并更新项目 `osdk.lock`。

```bash
osdk upgrade [TOOL[@VERSION]...] [-o KEY=VALUE]...
```

| 参数 | 语义 |
| --- | --- |
| `[TOOLS]...` | 为空时升级项目工具；给出时限定请求 |
| `-o, --opt KEY=VALUE` | backend 选项，可重复 |

```bash
osdk outdated
osdk upgrade
osdk upgrade node@20
```

`upgrade` 会改变已安装内容和 lock；`lock` 只解析/写 lock，`install` 负责复现现有 lock。
浮动 Rust channel 仍会随上游移动；严格复现应写明确版本或带日期 toolchain。

相关配置：`[tools]`、`[sources]`、`[settings].prerelease`。
