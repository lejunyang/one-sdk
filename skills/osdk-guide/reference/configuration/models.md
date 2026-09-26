# `[models]`

声明可由 `osdk model pull <name>` 和 `osdk model sync` 物化的模型快照。

```toml
[models.flux]
source = "hf:black-forest-labs/FLUX.1-dev@main"
include = ["*.safetensors", "*.json"]
exclude = ["*.md"]
variant = "fp16"
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
| `source` | 必填，`hf:owner/repo@revision` 或 `modelscope:owner/repo@revision` |
| `include` / `exclude` | 文件选择 glob |
| `variant` | 格式/量化标签，参与快照身份 |
| `when` | 平台过滤 |
| `endpoint` | provider endpoint 覆盖；改变来源，需 trust |
| `views.<kind>.profile` | 视图 profile，默认 `default` |
| `views.<kind>.map` | 仓库相对前缀到消费者分类的映射 |

显式 CLI reference/include/exclude/variant/endpoint 覆盖声明。普通模型声明不阻断工具命令；
只有自定义 endpoint 触发信任要求。
