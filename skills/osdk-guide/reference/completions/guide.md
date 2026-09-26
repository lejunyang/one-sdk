# `osdk completions`

向 stdout 生成 shell 补全脚本。

```bash
osdk completions <bash|elvish|fish|powershell|zsh>
```

命令不直接修改 shell 配置；把输出保存到对应 shell 的补全目录，或按 shell 规则 source。

```bash
osdk completions bash > ~/.local/share/bash-completion/completions/osdk
osdk completions fish > ~/.config/fish/completions/osdk.fish
```

安装位置由用户和 shell 决定，没有对应 `osdk.toml` 配置。
