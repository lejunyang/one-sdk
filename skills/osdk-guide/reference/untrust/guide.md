# `osdk untrust`

移除一个项目配置的信任记录。

```bash
osdk untrust [PATH]
```

`PATH` 可为配置文件或目录，省略时使用最近项目配置。命令不删除 `osdk.toml`，也不卸载
工具；以后命令到达需要信任的作用域时会再次拒绝并列出原因，只读命令不受影响。

查看全部记录用 `osdk trust list`，清理已经不存在的配置路径用 `osdk trust prune`。
