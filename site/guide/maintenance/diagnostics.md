# 诊断与安装校验

`osdk doctor` 报告环境状态；`osdk doctor --verify` 重新哈希已安装文件，找出安装之后被改动过的内容。

普通 `doctor` 还会只读列出 Node 版本目录中的旧 npm 全局包（包名、版本、路径），
并指出共享 npm prefix 中的重复安装。使用输出中的 `node migrate-packages --from VERSION
--to-global` 先预览；不同版本用 `--package NAME` 明确选择。本地链接与别名包会标为
需从原始来源单独处理，不会建议按 registry 包迁移。诊断本身不修改包或配置。

## 诊断

```text
osdk doctor
osdk doctor --verify
osdk doctor --verify --tool TOOL
osdk config path
osdk config list
osdk config get KEY [-g]
osdk source list TOOL
osdk registry test [MANAGER]
```

| 命令 | 输出 |
| --- | --- |
| `doctor` | 平台、data/store/install 目录、store 与 install 是否同文件系统、shim 路径及是否在 PATH、backend ID、代理状态 |
| `doctor --verify` | 以上全部，并重新哈希每个已安装文件，指出不再匹配的部分 |
| `doctor --verify --tool` | 同样的检查但只针对单个工具；完整校验会读取每个字节，耗时数分钟 |
| `config path` | 配置目录、用户配置文件、当前项目配置 |
| `config list` | 部分最终设置与目录、registry、模型环境、tools、alias.tools |
| `source list` | 某 backend/provider 的来源与 pin；`doctor` 不列镜像 |
| `registry test` | npm-compatible Registry 的匿名探测与选择计划 |
| `container doctor` | Docker/containerd 只读选择，以及独立的 Buildx 报告 |

顶层 `doctor` 当前不直接打印 `link_mode`；使用 `config list` 查看。它与诊断原生容器
控制面的 `container doctor` 不同。

`doctor` 的代理行需要解释一下。osdk 的 HTTP 客户端只从环境变量读取代理
（`HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`，大小写均可），**不读** Windows 设置里
的「系统代理」（那是浏览器等 WinINET 程序读的注册表项）。所以当你在 Windows 设置里
开了代理、却没有设置上述环境变量时，浏览器能上网而 osdk 超时——`doctor` 会明确报
「Windows 系统代理已开启，但 osdk 不会使用它」，并给出要设置的变量；设置了环境变量
则显示正在使用。osdk 只做诊断、不会擅自改用系统代理，因为那会改变流量去向。代理值中
若含账号密码会在输出前脱敏。

## 校验已安装文件

osdk 在下载时校验字节，但在重新检查之前，安装完成之后的变化没有任何环节会发现。会造成这种变化的
有三类：工具自己原地更新、手工改动、备份只恢复了一半或磁盘位翻转。这几种情况下 osdk 仍然报告
它当初安装的版本，实际运行的却是另一个二进制。

`osdk doctor --verify` 会按各安装目录下 `.osdk-manifest.json` 的记录重新哈希每个文件，
并报告不再匹配的部分：

```text
osdk doctor --verify
```

```text
  正在校验已安装文件
  node@20.11.1 : 与 osdk 安装时的内容已不一致
    E:\osdk-data\data\installs\node\20.11.1
      node.exe: contents changed
  已检查 9 个安装，1 个发生变化
  重装即可恢复（普通 install 会跳过已存在的安装）：
    osdk install --force node@20.11.1
```

它区分五类漂移：内容变化、文件缺失、文件类型变化（文件被换成链接，或反之）、链接指向已改变，
以及 zip 里的符号链接被解压成了普通文件（`symlink extracted as a plain file`）。最后一类是
osdk 0.0.9 及更早版本的解压缺陷：例如 Linux 上 Android NDK 的 `clang` 成了内容为 `clang-21` 的
文本文件。清单当时记录的就是这个坏文件，所以只能拿下载缓存里的原归档比对；归档已被清理时无法判断，
不会报告。同样的检查也在 `osdk install` 判断「已安装」时进行：发现这种安装会提示并自动重装，
不需要 `--force`。
早于清单机制的安装会被报告为无法校验，而不是默认通过。

这会读取每个文件，因此需要显式开启。普通 `osdk doctor` 仍是快速的环境检查，执行路径上也不做任何
哈希——运行工具的速度不受影响。

开销来自磁盘读取速度，而不是哈希计算。对 11.6 GB、32,463 个文件的完整校验实测约 4.5 分钟，
主要耗在两个 NDK 和一个系统镜像上。指定你实际怀疑的工具即可保持可用：

```text
osdk doctor --verify --tool node
```

`doctor --verify` 分别校验每个动态安装身份。新 PyPI 安装会原地记录文件哈希，不移动虚拟环境。旧安装需通过 `--force` 重装建立基线；字节码缓存不参与校验。

同样的检查用时不到 1 秒。`--tool` 必须与 `--verify` 一起使用，未知的名字会被拒绝，而不是
静默地什么都不检查。

### 修复发生漂移的安装

普通 `osdk install` 把已存在的安装视为已完成并立即返回，因此不会修复文件已被改动的安装。
`--force` 会覆盖重装：

```text
osdk install --force node@20.11.1
```

只影响你显式指定的版本。osdk 代为安装的依赖不会跟着被强制重装：把依赖装到位，不构成重建它的理由。

由于安装目录是硬链接到 CAS 的，改动已安装文件同时也改动了其背后的 store 对象。因此 osdk 在复用
对象前会确认它仍然哈希到自己所在的文件名，不符则丢弃，这样重装才是真正的修复，而不是把损坏的字节
重新链接回来。完好的对象仍会复用，去重不受影响。

### 它不做什么

校验是一次快照，不是一项策略。它只能告诉你某个安装与 osdk 当初放进去的内容不再一致，无法判断这次
改动是正当的自更新还是恶意替换——因为在文件系统层面两者完全相同。会自更新的工具在每次更新后都会持续
报告漂移；对这类工具，`--force` 会重装 osdk 所锁定的版本，而这正是锁定版本的意义。

osdk 更新托管 npm 的默认 prefix 时会维护对应配置文件的完整性记录，不会把合法刷新
误报成漂移；已有 npmrc 修改和其他安装文件损坏仍会被报告，不会重建缺失的旧基线。
