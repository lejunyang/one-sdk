# `osdk container`

检查和操作宿主原生 Docker、containerd、BuildKit；镜像不进入 osdk store。

## 子命令

| 子命令 | 参数与语义 |
| --- | --- |
| `pull <IMAGE>` | `--runtime auto\|docker\|containerd`、`--platform OS/ARCH[/VARIANT]`；containerd 的 `--address` 与 `--namespace` 必须成对 |
| `doctor` | `--runtime`、`--builder`、`--json`、`--check`；诊断运行时与 Buildx |
| `cache status` | `--runtime auto\|docker\|containerd\|buildkit`、`--builder`、`--json`、`--check`；`auto` 选择唯一具有聚合缓存契约的 Docker |
| `registry test <REGISTRY>` | `--image`、`--platform`、`--json`、`--check`；匿名探测 registry 与镜像候选 |
| `mirrors plan <REGISTRY>` | `--runtime docker\|containerd\|buildkit` 必填；`--builder`、`--native-config`、`--containerd-main-config`、`--json` |
| `mirrors apply <REGISTRY>` | 同 plan；`--native-config` 必填，另有 `--image`、`--platform`、`--accept-plan`、`--dry-run`、`--json` |
| `prune` | `--runtime docker\|buildkit\|containerd`、`--scope images\|build-cache` 必填；预览支持 `--json`；执行需 `--execute --accept-preview <ID>` |

```bash
osdk container doctor --json
osdk container registry test docker.io --image ubuntu:24.04 --platform linux/amd64
osdk container pull ubuntu:24.04
osdk container prune --runtime docker --scope images
```

`mirrors plan` 不写入；`mirrors apply --dry-run` 会测速并给出绑定当前输入的 plan id。
Docker apply/dry-run 还会先用 `dockerd --validate` 校验生成的 daemon JSON；校验失败不会
写文件。无人值守 `--yes` 仍必须同时给 `--accept-plan`。`prune` 同理要求接受精确 preview
id，避免机器状态变化后误删。配置见 `reference/configuration/container.md`。

Linux 上遇到 Docker `permission-denied` 时优先使用 rootless Docker；已有服务通常运行
`docker context use rootless` 即可。未安装时先运行 `dockerd-rootless-setuptool.sh check` 与
`dockerd-rootless-setuptool.sh install`，再按需启用 `systemctl --user enable --now docker`。
若没有生成 context，可改用 `DOCKER_HOST=unix://$XDG_RUNTIME_DIR/docker.sock`，但不要同时让旧
`DOCKER_HOST` 覆盖已选 context。rootless daemon 配置位于
`${XDG_CONFIG_HOME:-$HOME/.config}/docker/daemon.json`；将它显式传给 `--native-config`，apply 后
运行 `systemctl --user restart docker`。

只有可信 rootful 主机才考虑 `sudo usermod -aG docker "$USER"`，并在完整退出、重新登录后生效；
`docker` 组提供近似 root 的宿主权限。禁止把 Docker socket 改成全员可写。`sudo osdk` 会让
整个进程使用 root 权限，并可能切换 osdk/Docker 的配置、信任、context 与凭据，因此不能作为
日常权限修复；rootless 用户尤其不应这样做。

当 upstream 不可达时，带 digest 的 `--image` 仍可直接对 mirror 校验相同内容；tag 不会在
mirror 上重新解析。`mirrors apply --json` 找不到合格 mirror 时会先输出包含完整诊断的
`no-verified-mirror` JSON，再以非零状态退出。

自动化时注意：`pull` 透传原生运行时退出码（Unix 信号转为 `128 + signal`）；带
`--json` 的诊断/计划输出有稳定 schema，但实时延迟值允许变化。plan/preview id 绑定
当时状态，不应保存成长期配置。

Unix 上 containerd selector 使用规范的 `unix:///path/to/containerd.sock`；osdk 会在
调用 `ctr` 时渲染成它要求的原生 `/path/to/containerd.sock` 参数。

在 one-sdk 仓库验证真实宿主契约时运行 `osdk run container-smoke`。它检查 Docker、
Buildx、Registry、plan、preview 与 pull，并在宿主提供可用 `ctr` 时检查 containerd；
不会应用 mirror 配置或执行 prune。
