# `osdk container`

检查和操作宿主原生 Docker、containerd、BuildKit；镜像不进入 osdk store。

## 子命令

| 子命令 | 参数与语义 |
| --- | --- |
| `pull <IMAGE>` | `--runtime auto\|docker\|containerd`、`--platform OS/ARCH[/VARIANT]`；containerd 的 `--address` 与 `--namespace` 必须成对 |
| `doctor` | `--runtime`、`--builder`、`--json`；诊断运行时与 Buildx |
| `cache status` | `--runtime auto\|docker\|containerd\|buildkit`、`--builder`、`--json` |
| `registry test <REGISTRY>` | `--image`、`--platform`、`--json`；匿名探测 registry 与镜像候选 |
| `mirrors plan <REGISTRY>` | `--runtime docker\|containerd\|buildkit` 必填；`--builder`、`--native-config`、`--containerd-main-config`、`--json` |
| `mirrors apply <REGISTRY>` | 同 plan；`--native-config` 必填，另有 `--image`、`--platform`、`--accept-plan`、`--dry-run`、`--json` |
| `prune` | `--runtime docker\|buildkit\|containerd`、`--scope images\|build-cache` 必填；可选 `--context` / `--builder`；执行需 `--execute --accept-preview <ID>` |

```bash
osdk container doctor --json
osdk container registry test docker.io --image ubuntu:24.04 --platform linux/amd64
osdk container pull ubuntu:24.04
osdk container prune --runtime docker --scope images
```

`mirrors plan` 不写入；`mirrors apply --dry-run` 会测速并给出绑定当前输入的 plan id。
无人值守 `--yes` 仍必须同时给 `--accept-plan`。`prune` 同理要求接受精确 preview id，
避免机器状态变化后误删。配置见 `reference/configuration/containers.md`。

自动化时注意：`pull` 透传原生运行时退出码（Unix 信号转为 `128 + signal`）；带
`--json` 的诊断/计划输出有稳定 schema，但实时延迟值允许变化。plan/preview id 绑定
当时状态，不应保存成长期配置。

Unix 上 containerd selector 使用规范的 `unix:///path/to/containerd.sock`；osdk 会在
调用 `ctr` 时渲染成它要求的原生 `/path/to/containerd.sock` 参数。
