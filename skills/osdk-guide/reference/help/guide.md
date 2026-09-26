# `osdk help`

Clap 自动生成的帮助入口：

```bash
osdk --help
osdk help <COMMAND>
osdk <COMMAND> --help
osdk <COMMAND> <SUBCOMMAND> --help
```

具体版本的命令面和可选值以 `--help` 为最终权威。本 skill 补充跨命令工作流、配置映射和
容易误用的语义；发现 reference 与实际 `--help` 不一致时，以 CLI 为准并修复 skill。
