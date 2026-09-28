# `osdk model`

声明、同步、验证和移除不可变模型快照，并渲染 ComfyUI / Hugging Face cache 视图。

| 子命令 | 参数与语义 |
| --- | --- |
| `use <NAME> <REFERENCE>` | 受管写入项目声明；支持 `--endpoint`、`--include/--exclude`、`--variant`、`--kind/--family/--derived-from`、`--view/--profile/--map`；`--sync` 立即物化 |
| `import <NAME> <PATH>` | 导入本地文件/目录为 CAS 不可变快照；支持语义元数据、`--target-path`、`--view comfyui` 与 `--json`，不写声明或 lock |
| `unuse <NAME>` | 移除声明、lock 和视图，默认删除快照；`--keep-snapshot` 保留字节 |
| `sync [NAME]` | 同步单个或全部模型；`--dry-run` 预览；全项目模式可用 `--prune`；`--jsonl` 输出逐行 schema 1 事件 |
| `list/show/path/verify` | 检查本地快照；查询命令支持 `--json`；`path --stable` 输出稳定 current 路径 |
| `remove <NAME>` | 仅删除本地快照和视图，保留声明与 lock |

```bash
osdk model use qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main \
  --include '*.safetensors' --view comfyui --sync
osdk model use character-lora civitai:456@123 --view comfyui --sync
osdk model import local-style C:\models\style.safetensors --kind lora --view comfyui --json
osdk model sync qwen25 --dry-run --jsonl
osdk model show qwen25 --json
osdk model view list --json
osdk model unuse qwen25
```

不要手写 OSDK 受管模型声明；使用 `model use/unuse`。`model import` 只创建当前机器的本地快照，不写声明或 lock，并拒绝链接/reparse point 与特殊文件。`install` 不会隐式下载模型。Civitai 引用必须是精确的 `civitai:<model-id>@<model-version-id>`；OSDK 不做搜索与排序。Civitai 内置 `.com`（`official`）和 `.red`（`official-red`）两个官方入口，auto 模式按精确版本探测，亦可用 `source pin civitai <id>` 固定。
机器模式 stdout 只输出 schema 1 JSON/JSONL，诊断走 stderr，失败保持非零退出码；`model view list/path/doctor` 也支持 `--json`。
provider 环境和 `model view` 的其余子命令以 `osdk model --help` 为准；声明字段见
`reference/configuration/models.md`。