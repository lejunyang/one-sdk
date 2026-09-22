#!/usr/bin/env python3
"""Loopback Hugging Face fixture server for the ComfyUI model-view integration test.

Serves three tiny repos over plain HTTP on 127.0.0.1:<port>:
  owner/a : unet/vae/text_encoder/config.json  (maps to three ComfyUI categories)
  owner/b : vae + loras (rendered into a second profile)
  owner/c : unet only (collides with a on diffusion_models/model.safetensors)

The metadata endpoint shape mirrors what the osdk huggingface provider queries;
file endpoints serve fixed bytes keyed by repo-relative path. Kept deliberately
dependency-free (stdlib only).
"""

import hashlib
import http.server
import json
import sys

REPOS = {
    "owner/a": {
        "sha": "aaaaaa1",
        "files": {
            "unet/model.safetensors": b"AAA-UNET",
            "vae/model.safetensors": b"AAA-VAE",
            "text_encoder/model.safetensors": b"AAA-TE",
            "config.json": b'{"a":1}',
        },
    },
    "owner/b": {
        "sha": "bbbbbb1",
        "files": {
            "vae/model.safetensors": b"BBB-VAE",
            "loras/x.safetensors": b"BBB-LORA",
        },
    },
    "owner/c": {
        "sha": "cccccc1",
        "files": {"unet/model.safetensors": b"CCC-UNET"},
    },
}


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):  # silence request logging
        pass

    def _repo(self):
        for repo_id in REPOS:
            if "/" + repo_id + "/" in self.path:
                return repo_id
        return None

    def do_GET(self):
        repo_id = self._repo()
        if repo_id is None:
            self.send_response(404)
            self.end_headers()
            return
        repo = REPOS[repo_id]
        if "/api/models" in self.path:
            siblings = [
                {
                    "rfilename": path,
                    "lfs": {
                        "sha256": hashlib.sha256(payload).hexdigest(),
                        "size": len(payload),
                    },
                }
                for path, payload in sorted(repo["files"].items())
            ]
            body = json.dumps({"sha": repo["sha"], "siblings": siblings}).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        # File endpoint: .../resolve/<sha>/<repo-relative-path>
        name = self.path.split("/resolve/", 1)[1].split("/", 1)[1]
        payload = repo["files"].get(name)
        if payload is None:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("ETag", '"fix"')
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    port = int(sys.argv[1])
    http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
