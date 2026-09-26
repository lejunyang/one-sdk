# `osdk android`

管理 Android SDK 协议、共享 SDK root 索引和 AVD，不依赖 `sdkmanager` / `avdmanager`
完成这些管理动作。

## licenses

```bash
osdk android licenses show <TOOL> [--digest-only]
osdk android licenses status
osdk android licenses export --sdk-root <DIR>
```

- `show` 在下载前展示某 Android 包要求的协议；`--digest-only` 只打印 id 与摘要。
- `status` 查看已记录接受项。
- `export` 把接受记录写进目标 SDK root 的 `licenses/`，供 Gradle/Google 工具复用。
- osdk 不会代替用户接受协议；安装时显式用 `-o accept-licenses=true` 或指定 id。

## sdk-root

```bash
osdk android sdk-root show
osdk android sdk-root repair
```

`show` 显示共享 SDK root 与预期条目；`repair` 为已安装包重写 Google 工具读取的
`package.xml` 索引。

## avd

```bash
osdk android avd list
osdk android avd create <NAME> --image <API;TAG;ABI> [--force] [--data-size 8G] [--sdcard-size 512M]
osdk android avd delete <NAME>
```

`create` 要求对应 system image 已安装；设备名会校验，不能逃逸 AVD home。Android 包
版本与平台条件写在 `[tools]`，具体 backend 名见工具请求 reference。
