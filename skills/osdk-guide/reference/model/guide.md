# `osdk model`

下载、验证、复现不可变模型快照，并渲染 ComfyUI / Hugging Face cache 形状的只读视图。

## 快照子命令

| 子命令 | 参数与语义 |
| --- | --- |
| `pull <NAME> [REFERENCE]` | `--endpoint`、`--forward-credentials`、可重复 `--include/--exclude`、`--variant`、`--no-lock`；省略 reference 时读 `[models.<name>]` |
| `sync` | `--dry-run` 预览；`--prune` 删除 lock 不再声明的本地快照 |
| `list` | 列本地快照 |
| `path <NAME>` | `--stable` 输出稳定 current 路径，否则输出内容寻址路径 |
| `verify <NAME>` | 校验快照内全部文件 |
| `remove <NAME>` | 默认同时移除 lock；`--keep-lock` 让以后 sync 可恢复 |

```bash
osdk model pull qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main --include '*.safetensors'
osdk model sync --dry-run
osdk model path qwen25 --stable
osdk model verify qwen25
```

`install` 不会隐式下载模型；用无参 `model sync` 兑现整个项目的声明与 lock。

## provider 环境

```bash
osdk model env enable [huggingface|modelscope] [--force]
osdk model env disable [huggingface|modelscope]
osdk model env list
```

`--force` 允许覆盖用户已有同名变量；否则用户值优先。

## 视图

```bash
osdk model view add <comfyui|hf-cache> <MODEL> [--profile NAME] [--map PREFIX=CATEGORY]...
osdk model view list
osdk model view path <KIND> [--profile NAME]
osdk model view rebuild [KIND]
osdk model view remove <KIND> [--profile NAME] [--model NAME]
osdk model view export <KIND> [--profile NAME] [--to FILE]
osdk model view doctor <KIND> [--profile NAME]
```

视图链接回快照，不复制权重。ComfyUI Desktop 不由 osdk 修改 settings；无 `--to` 的
`export` 打印片段供手工配置。声明式格式见 `reference/configuration/models.md`。
