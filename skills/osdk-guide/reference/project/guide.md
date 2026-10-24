# `osdk project`

把一个目录绑定为**外置项目**：不在项目目录里写 `osdk.toml` / `osdk.lock`，
而是把配置与锁存到 `$OSDK_CONFIG_DIR/projects/<root-id>/`。适用于只读检出的
第三方仓库、不能改动的代码库，或希望配置集中托管的多项目机器。

| 子命令 | 参数 | 语义 |
| --- | --- | --- |
| `add [PATH]` | `--dry-run`、`--json` | 绑定给定目录（默认 cwd）；经 OS canonicalize 计入真实路径；不猜 Git 根、不安装、不改目录；重复 add 不覆盖；目录已有本地配置时允许绑定但运行时本地胜出（stderr shadowed 警告） |
| `show [PATH]` | `--json`（仅） | 显示该目录命中的来源（`local`/`external`）、`config_path`、`lock_path`、`binding_dir`、`shadowed_local_config`、`missing` |
| `list` | `--json`（数组） | 无位置参。纯文本逐行打印根目录、缩进 `config:`/`lock:`；状态字段 `root_exists`、`config_exists`、`shadowed_by_local` 仅 JSON 可见 |
| `relocate OLD NEW` | `--dry-run`、`--json` | OLD 与 NEW 必填；外置 `osdk.toml` 与 `osdk.lock` 保留；旧根 missing 不自动删 |
| `remove PATH` | `--dry-run`、`--json` | PATH 必填；只删外置绑定目录；不删项目文件、不删共享安装池工具 |

```bash
osdk project add .                  # 绑定当前目录
osdk project add C:\repos\frontend  # 精确绑定，不向上猜 Git 根
osdk project show .                 # 这个目录实际走哪份配置/锁
osdk project show . --json          # 含全部状态字段
osdk project list                   # 纯文本：根 + config:/lock:
osdk project list --json            # 数组，含 root_exists/config_exists/shadowed_by_local
osdk project relocate ~/old ~/new --dry-run
osdk project remove ~/tmp/checkout --dry-run
osdk project remove ~/tmp/checkout
```

## 选择规则（对所有读写入口一致）

- 从 cwd 逐祖先向上：最近项目胜出；同一项目根本地 `osdk.toml` /
  `.osdk.toml` 优先于外置配置；近的外置子项目可胜远的本地父项目。
- 配置与锁成套选择：本地缺锁不借外置锁，外置缺锁在外置目录内生成；
  选中配置损坏直接报错，不回退；shadowed 的外置正文不读。
- `current` / `where` / `exec` / shim / shell hook / `use` / `config` /
  `source` 共用这套解析；`--global` 仍是用户全局默认，与绑定无关。

## 写边界（安全相关，先读）

- 外置项目的 `osdk use npm:xxx` 走隔离安装：不改 `package.json`、不碰原生
  lock；但 `osdk run` / `osdk deps` 仍按逻辑项目根写项目输出，**不承诺项目
  目录零写入**，不要绑定到真只读卷后期望 run/deps 不写盘。
- 逻辑项目根是被绑定的目录：任务、deps、相对资源按它解析；`extends` 保留
  逻辑祖先与仓库边界。
- 信任按外置目录里真实 `osdk.toml` 审批；`relocate` 后物理路径变了，需要
  重新 `osdk trust`，不会自动跟随。
- 没有 Git remote / 分支 / multi-profile 概念：绑定就是路径绑定。

完整用法见用户指南 `site/guide/project/external-config.md`，实现取舍见
`site/internals/project-config.md`。以 `osdk project --help` 为最终参数权威。
