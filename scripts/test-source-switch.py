"""Actual CLI source-switch regressions; all requests stay on loopback fixtures."""
import contextlib
import hashlib
import http.server
import json
import os
from pathlib import Path
import shlex
import ssl
import subprocess
import sys
import tempfile
import threading

BINARY = Path(sys.argv[1]).resolve()
BASELINE = "--baseline" in sys.argv[2:]


def put(root, path, text, executable=False):
    target = root / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")
    if executable:
        target.chmod(0o755)
    return target


def run(root, args, extra=None, success=True):
    env = {key: str(root / value) for key, value in {
        "HOME": "home", "USERPROFILE": "home", "OSDK_DATA_DIR": "data",
        "OSDK_CONFIG_DIR": "config", "OSDK_CACHE_DIR": "cache", "OSDK_STORE_DIR": "store",
        "OSDK_INSTALL_DIR": "installs", "CARGO_HOME": "personal-cargo", "RUSTUP_HOME": "rustup",
        "CARGO_TARGET_DIR": "target", "TMPDIR": "tmp"}.items()}
    (root / "home").mkdir(exist_ok=True)
    (root / "tmp").mkdir(exist_ok=True)
    env.update(PATH="/usr/bin:/bin", LANG="C.UTF-8", HTTP_PROXY="http://127.0.0.1:1",
               HTTPS_PROXY="http://127.0.0.1:1", NO_PROXY="127.0.0.1,localhost")
    env.update(extra or {})
    result = subprocess.run([str(BINARY), *args], cwd=root, env=env, text=True,
                            capture_output=True, timeout=35)
    print(f"$ osdk {shlex.join(args)}\nexit={result.returncode}\n{result.stdout}{result.stderr}", flush=True)
    assert (result.returncode == 0) == success, result
    return result


@contextlib.contextmanager
def server(root, tls):
    class Handler(http.server.BaseHTTPRequestHandler):
        def request(self):
            self.server.requests.append((self.command, self.path))
            assert self.headers.get("Authorization") is None
            region = self.path.split("/")[1]
            origin = "native" if region == "native" else "selected"
            body = self.server.responses.get(self.path)
            self.send_response(200 if body is not None else 404)
            self.send_header("Content-Length", str(len(body or b"")))
            self.end_headers()
            if body and self.command != "HEAD":
                self.wfile.write(body)
        do_GET = do_HEAD = request
        def log_message(self, *_):
            pass
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as s:
        s.requests, s.responses = [], {}
        if tls:
            subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                            "-days", "1", "-subj", "/CN=source-switch-fixture",
                            "-addext", "subjectAltName=IP:127.0.0.1",
                            "-keyout", str(root / "ca-key.pem"), "-out", str(root / "cert.pem")],
                           check=True, capture_output=True)
            subprocess.run(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=127.0.0.1",
                            "-keyout", str(root / "key.pem"), "-out", str(root / "leaf.csr")], check=True, capture_output=True)
            ext = put(root, "leaf.ext", "subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\n")
            subprocess.run(["openssl", "x509", "-req", "-in", str(root / "leaf.csr"), "-CA", str(root / "cert.pem"),
                            "-CAkey", str(root / "ca-key.pem"), "-CAcreateserial", "-days", "1", "-extfile", str(ext),
                            "-out", str(root / "leaf.pem")], check=True, capture_output=True)
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.load_cert_chain(root / "leaf.pem", root / "key.pem")
            s.socket = ctx.wrap_socket(s.socket, server_side=True)
        s.base = f"{'https' if tls else 'http'}://127.0.0.1:{s.server_port}"
        thread = threading.Thread(target=s.serve_forever, daemon=True)
        thread.start()
        try:
            yield s
        finally:
            s.shutdown()
            thread.join()


def rust(root):
    tool = root / "data/rustup/toolchains/1.91.1"
    put(root, "installs/rust/1.91.1/.osdk-complete", "")
    put(tool, "bin/rustc", "#!/bin/sh\nexit 0\n", True)
    put(tool, "lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-fixture.rlib", "std")
    put(tool, "lib/librustc_driver-fixture.so", "driver")
    for component, file in [("cargo", "bin/cargo"), ("rustc", "bin/rustc"),
                            ("rust-std", "libstd-fixture.rlib")]:
        put(tool, f"lib/rustlib/manifest-{component}-fixture", f"file:{file}")
    put(tool, "bin/cargo", "#!/bin/sh\nset -eu\n"
        'test "$1" = install; test "$2" = --root\n'
        'case "$(/bin/cat "$CARGO_HOME/config.toml")" in *broken*) exit 42;; *selected*) origin=selected;; *) origin=native;; esac\n'
        '/bin/mkdir -p "$3/bin"\nprintf "#!/bin/sh\\necho %s\\n" "$origin" > "$3/bin/fixture-tool"\n'
        '/bin/chmod +x "$3/bin/fixture-tool"\n', True)


def case(family, mode):
    with tempfile.TemporaryDirectory(prefix="osdk-source-switch-") as directory:
        root = Path(directory)
        with server(root, family == "cargo") as s:
            extra = {"SSL_CERT_FILE": str(root / "cert.pem")} if family == "cargo" else {}
            tool = f"{family}:fixture-tool"
            if family == "cargo":
                project = put(root, "osdk.toml", '[tools]\nrust="1.91.1"\n')
                extra["OSDK_TRUSTED_CONFIG_PATHS"] = str(project)
                rust(root)
                put(root, "personal-cargo/config.toml", f'[source.crates-io]\nreplace-with="native"\n[source.native]\nregistry="sparse+{s.base}/native/"\n')
                for region in ["native", "selected", "broken"]:
                    s.responses[f"/{region}/config.json"] = b'{"dl":"unused"}'
                    s.responses[f"/{region}/fi/xt/fixture-tool"] = b'{"vers":"1.2.3"}'
            else:
                put(root, "osdk.toml", '[tools]\nnode="20.0.0"\n')
                put(root, "home/.npmrc", f"registry={s.base}/native/\n")
                put(root, "installs/node/20.0.0/.osdk-complete", "")
                put(root, "installs/node/20.0.0/bin/node", "#!/bin/sh\nexit 0\n", True)
                for region in ["native", "selected", "broken"]:
                    tarball = f"{s.base}/{region}/fixture-tool/-/fixture-tool-1.2.3.tgz"
                    s.responses[f"/{region}/fixture-tool"] = b'{"versions":{"1.2.3":{}}}'
                    s.responses[f"/{region}/fixture-tool/1.2.3"] = json.dumps({"dist":{"tarball":tarball,"integrity":"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}}).encode()
                put(root, "installs/node/20.0.0/bin/npm", "#!/bin/sh\nset -eu\n"
                    'registry=$(while IFS= read -r line; do case "$line" in registry=*) printf %s "${line#registry=}";; esac; done < .npmrc)\n'
                    'case "$registry" in *broken*) exit 42;; *selected*) origin=selected;; *) origin=native;; esac\n'
                    '/bin/mkdir -p node_modules/.bin node_modules/fixture-tool\n'
                    'printf %s \'{"name":"fixture-tool","version":"1.2.3","bin":{"fixture-tool":"cli.js"}}\' > node_modules/fixture-tool/package.json\n'
                    'printf "#!/bin/sh\\necho %s\\n" "$origin" > node_modules/fixture-tool/cli.js\n'
                    '/bin/chmod +x node_modules/fixture-tool/cli.js\n'
                    '/bin/ln -s ../fixture-tool/cli.js node_modules/.bin/fixture-tool\n'
                    'printf \'{"lockfileVersion":3,"packages":{"node_modules/fixture-tool":{"version":"1.2.3","resolved":"%sfixture-tool/-/fixture-tool-1.2.3.tgz","integrity":"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}}}\' "$registry" > package-lock.json\n', True)
            run(root, ["install", tool + "@1.2.3"], extra)
            filename = "fixture-tool" if family == "cargo" else "cli.js"
            old = [p for p in (root / "installs" / family / "fixture-tool").rglob(filename) if p.is_file() and "echo native" in p.read_text()]
            assert old, family
            old_bytes = {p: p.read_bytes() for p in old}
            for region in ["selected", "broken"]:
                args = ["source", "add", tool, "--id", region, "--download-url", f"{s.base}/{region}/"]
                if family == "cargo":
                    args += ["--index-url", f"sparse+{s.base}/{region}/"]
                run(root, args, extra)
            s.requests.clear()
            if mode == "failed":
                run(root, ["--source", "broken", "install", tool + "@1.2.3"], extra, success=False)
                assert all(p.read_bytes() == content for p, content in old_bytes.items())
                assert any(path.startswith("/broken/") for _, path in s.requests), s.requests
            else:
                if mode == "pin":
                    run(root, ["source", "pin", tool, "selected"], extra)
                    prefix = []
                else:
                    prefix = ["--source", "selected"]
                result = run(root, [*prefix, "install", tool + "@1.2.3"], extra)
                if BASELINE:
                    assert "already installed" in result.stdout and not s.requests
                    print(f"{family}/{mode}: reproduced stale origin", flush=True)
                    return
                assert any(path.startswith("/selected/") for _, path in s.requests), s.requests
                assert all(p.read_bytes() == content for p, content in old_bytes.items())
                new = [p for p in (root / "installs" / family / "fixture-tool").rglob(filename) if p.is_file() and "echo selected" in p.read_text()]
                assert new, (family, s.requests)
                s.requests.clear()
                reused = run(root, [*prefix, "--offline", "install", tool + "@1.2.3"], extra)
                assert "already installed" in reused.stdout and not s.requests
                assert all("echo selected" in p.read_text() for p in new)
            print(f"{family}/{mode}: requests={s.requests}; old and selected artifact bytes checked", flush=True)


for family in ["cargo", "npm"]:
    for mode in (["pin", "once"] if BASELINE else ["pin", "once", "failed"]):
        case(family, mode)
print("6 source-switch scenarios passed" if not BASELINE else "4 baseline failures reproduced")
