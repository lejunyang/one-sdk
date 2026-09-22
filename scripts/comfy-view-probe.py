#!/usr/bin/env python3
"""Headless ComfyUI discovery probe for the model-view integration test.

Loads a *real* source-tree ComfyUI `folder_paths`, points it at the osdk-rendered
extra_model_paths.yaml, and prints one JSON line with the filename lists for the
categories under test. It runs no server and needs no torch -- only PyYAML, which
folder_paths imports.

argv: <comfy_dir> <extra_yaml_path>
Prints: RESULT=<json>
"""

import importlib
import json
import os
import sys

comfy_dir, yaml_path = sys.argv[1], sys.argv[2]
sys.path.insert(0, comfy_dir)
os.chdir(comfy_dir)

import folder_paths  # noqa: E402
from utils import extra_config  # noqa: E402

importlib.reload(folder_paths)
extra_config.load_extra_path_config(yaml_path)

categories = ["diffusion_models", "vae", "text_encoders", "checkpoints"]
result = {cat: folder_paths.get_filename_list(cat) for cat in categories}
print("RESULT=" + json.dumps(result))
