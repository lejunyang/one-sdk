# `[models]`

声明由 `osdk model use` 受管写入、并由 `osdk model sync [name]` 物化的模型快照。

```toml
[models.flux]
source = "hf:black-forest-labs/FLUX.1-dev@main"
include = ["*.safetensors", "*.json"]
exclude = ["*.md"]
variant = "fp16"
kind = "diffusion-model"
family = "flux"
derived_from = "hf:black-forest-labs/FLUX.1-dev@main"
when = { os = "linux" }
# endpoint = "https://..."             # 自定义来源，需要 trust

[models.flux.views.comfyui]
profile = "default"

[models.flux.views.comfyui.map]
"unet/" = "diffusion_models"
"vae/" = "vae"
```

| 字段 | 语义 |
| --- | --- |
| `source` | 必填，`hf:owner/repo@revision`、`modelscope:owner/repo@revision` 或精确 Civitai LoRA `civitai:model-id@model-version-id` |
| `include` / `exclude` | 文件选择 glob |
| `variant` | 格式/量化标签，参与快照身份 |
| `kind` | 语义类型枚举，Civitai 默认 `lora` |
| `family` | 架构/生态家族，参与快照身份 |
| `derived_from` | 基础模型/上游血缘，参与快照身份 |
| `when` | 平台过滤 |
| `endpoint` | provider endpoint 覆盖；改变来源，需 trust |
| `views.<kind>.profile` | 视图 profile，默认 `default` |
| `views.<kind>.map` | 仓库相对前缀到消费者分类的映射 |

`model use` 的 reference/include/exclude/variant/kind/family/derived_from/endpoint 会写入同名声明；`sync` 会识别这些字段的变化。模型配置不触发
任何信任要求：模型字节是内容，osdk 不执行它们，下载仍按锁定摘要校验。`model import` 不属于 `[models]`：本地路径不可跨机器恢复，因此只创建本机快照，不写配置或 lock。
