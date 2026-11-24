#!/usr/bin/env python3
"""Run with real Go on PATH and an optional path to the built osdk binary."""

import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest


PARENT = "corp.example/secret"
MODULE = f"{PARENT}/cli"
VERSION_LIST = f"{MODULE}/@v/list"
VANITY_URL = f"http://{MODULE}?go-get=1"


class Proxy(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_CONNECT(self):
        # Refuse HTTPS so GOINSECURE sends the vanity lookup over HTTP. Both
        # requests terminate here, without resolving corp.example.
        self.server.requests.append(self.path)
        self.send_error(403)

    def do_GET(self):
        self.server.requests.append(self.path)
        if self.path == VANITY_URL:
            # Go's direct discovery supports a module proxy as the private
            # origin. This makes a successful list with zero tags reproducible.
            origin = f"http://127.0.0.1:{self.server.server_port}/private"
            body = f'<meta name="go-import" content="{MODULE} mod {origin}">'
        elif self.path == f"/private/{VERSION_LIST}":
            if self.server.refuse_private:
                self.send_error(403, "private module unavailable")
                return
            body = ""
        else:
            module, _, resource = self.path.removeprefix("/public/").partition("/@v/")
            if not self.path.startswith("/public/") or module not in (MODULE, PARENT):
                self.send_error(404)
                return
            version = "v1.2.3" if module == MODULE else "v9.0.0"
            if resource == "list":
                body = f"{version}\n"
            elif resource == f"{version}.info":
                body = json.dumps({"Version": version, "Time": "2026-01-01T00:00:00Z"})
            elif resource == f"{version}.mod":
                body = f"module {module}\n\ngo 1.24.0\n"
            else:
                self.send_error(404)
                return
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body.encode("ascii"))


class PrivateGoProxyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        temporary = tempfile.TemporaryDirectory(prefix="osdk-go-private-")
        cls.addClassCleanup(temporary.cleanup)
        cls.root = Path(temporary.name)
        home = cls.root / "home"
        home.mkdir()
        cls.env = {
            key: os.environ[key]
            for key in ("SYSTEMROOT", "WINDIR")
            if key in os.environ
        }
        cls.env.update(HOME=str(home), USERPROFILE=str(home), GOENV="off", GOTOOLCHAIN="local")

        go = shutil.which("go")
        if go is None:
            raise RuntimeError("A real Go runtime must be available on PATH")
        goroot = Path(subprocess.check_output(
            [go, "env", "GOROOT"], cwd=cls.root, env=cls.env, text=True, timeout=15
        ).strip())
        version = (goroot / "VERSION").read_text().split()[0].removeprefix("go")
        install = cls.root / "installs"
        runtime = install / "go" / version
        shutil.copytree(goroot, runtime)
        (runtime / ".osdk-complete").touch()
        (cls.root / "osdk.toml").write_text(f'[tools]\ngo = "{version}"\n')
        cls.env["OSDK_INSTALL_DIR"] = str(install)

    def lookup(self, *, refuse_private=False, **overrides):
        with (
            tempfile.TemporaryDirectory(dir=self.root) as temporary,
            http.server.ThreadingHTTPServer(("127.0.0.1", 0), Proxy) as server,
        ):
            server.requests = []
            server.refuse_private = refuse_private
            state = Path(temporary)
            worker = threading.Thread(target=server.serve_forever)
            worker.start()
            try:
                base = f"http://127.0.0.1:{server.server_port}"
                env = self.env | {
                    f"OSDK_{name}_DIR": str(state / name.lower())
                    for name in ("CACHE", "CONFIG", "DATA", "STORE")
                }
                env.update(
                    GOPROXY=f"{base}/public,off",
                    GOPRIVATE=f"{PARENT}/*",
                    GOINSECURE="corp.example",
                    HTTP_PROXY=base,
                    HTTPS_PROXY=base,
                    TMPDIR=temporary,
                    TEMP=temporary,
                    TMP=temporary,
                )
                result = subprocess.run(
                    [self.osdk, "list-remote", f"go:{MODULE}"],
                    cwd=self.root, env=env | overrides,
                    capture_output=True, text=True, timeout=30,
                )
            finally:
                server.shutdown()
                worker.join()
        return result, server.requests

    def assert_private_lookup(self, reason, result, requests):
        self.assertIn(VANITY_URL, requests)
        self.assertIn(f"/private/{VERSION_LIST}", requests)
        public_requests = [path for path in requests if path.startswith("/public/")]
        self.assertEqual(public_requests, [], f"unexpected public proxy requests: {public_requests}")
        self.assertNotEqual(result.returncode, 0, result)
        self.assertIn(reason, result.stderr)
        self.assertIn(f"search limited to private module paths under `{MODULE}`", result.stderr)

    def test_private_error_does_not_send_parent_to_public_proxy(self):
        result, requests = self.lookup(refuse_private=True)
        self.assert_private_lookup("403", result, requests)

    def test_private_empty_list_does_not_send_parent_to_public_proxy(self):
        result, requests = self.lookup()
        self.assert_private_lookup("no module versions found", result, requests)

    def test_gonoproxy_none_uses_public_proxy(self):
        result, requests = self.lookup(GONOPROXY="none")
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual(result.stdout.strip(), "1.2.3")
        self.assertIn(f"/public/{VERSION_LIST}", requests)
        self.assertTrue(all(path.startswith(f"/public/{MODULE}/") for path in requests), requests)


if __name__ == "__main__":
    binary_name = "osdk.exe" if os.name == "nt" else "osdk"
    default_binary = Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "debug" / binary_name
    PrivateGoProxyTests.osdk = Path(sys.argv.pop(1) if len(sys.argv) > 1 else default_binary).resolve()
    unittest.main(verbosity=2)
