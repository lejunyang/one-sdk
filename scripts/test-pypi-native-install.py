#!/usr/bin/env python3
"""Real pip/uv source regressions. Run with --osdk, --python and --uv paths.

No package build or external network is used. Wheels and a trusted loopback TLS
certificate are generated locally; every install has fresh HOME and OSDK state.
"""
import argparse
import base64
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import shutil
import ssl
import subprocess
import tempfile
import threading
import unittest
import zipfile


TOP = "osdk-source-fixture"
DEP = "osdk-source-dep"


def wheel(name, version, origin, dependency=False, uv_binary=None):
    module = name.replace("-", "_")
    dist = f"{module}-{version}.dist-info"
    files = {
        f"{dist}/WHEEL": b"Wheel-Version: 1.0\nGenerator: osdk-tests\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        f"{dist}/METADATA": (
            f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"
            + (f"Requires-Dist: {DEP}>=1.0.0\n" if dependency else "")
        ).encode(),
    }
    if uv_binary:
        import shlex
        files[f"uv-{version}.data/scripts/uv"] = (
            f'#!/bin/sh\nexec {shlex.quote(str(Path(uv_binary).resolve()))} "$@"\n'
        ).encode()
    else:
        files[f"{module}/__init__.py"] = f"ORIGIN = {origin!r}\n".encode()
        if name == TOP:
            files[f"{module}/cli.py"] = (
                "import json\nfrom . import ORIGIN\n"
                "def main():\n"
                "    import osdk_source_dep\n"
                "    print(json.dumps({'top': ORIGIN, 'dep': osdk_source_dep.ORIGIN}))\n"
            ).encode()
            files[f"{dist}/entry_points.txt"] = (
                f"[console_scripts]\nsource-fixture = {module}.cli:main\n"
            ).encode()
    record = []
    for path, data in files.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
        record.append(f"{path},sha256={digest},{len(data)}\n")
    record.append(f"{dist}/RECORD,,\n")
    files[f"{dist}/RECORD"] = "".join(record).encode()
    result = io.BytesIO()
    with zipfile.ZipFile(result, "w", zipfile.ZIP_DEFLATED) as archive:
        for path, data in files.items():
            info = zipfile.ZipInfo(path)
            info.external_attr = 0o755 << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, data)
    return f"{module}-{version}-py3-none-any.whl", result.getvalue()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        self.server.requests.append(self.path)
        if self.server.auth:
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="fixture"')
            self.end_headers()
            return
        parts = self.path.strip("/").split("/")
        if len(parts) == 2 and parts[0] == "simple":
            packages = self.server.packages.get(parts[1], [])
            if not packages:
                self.send_error(404)
                return
            body = "".join(
                f'<a href="/files/{filename}#sha256={hashlib.sha256(data).hexdigest()}">{filename}</a>'
                for filename, data in packages
            ).encode()
            content_type = "text/html"
        elif len(parts) == 2 and parts[0] == "files":
            body = next((data for packages in self.server.packages.values() for filename, data in packages if filename == parts[1]), None)
            if body is None:
                self.send_error(404)
                return
            content_type = "application/octet-stream"
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)


class SourceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="osdk-real-python-sources-")
        cls.addClassCleanup(cls.temp.cleanup)
        cls.root = Path(cls.temp.name)
        cls.cert = cls.root / "loopback.crt"
        key = cls.root / "loopback.key"
        subprocess.run([
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(key), "-out", str(cls.cert), "-days", "1",
            "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
            "-addext", "basicConstraints=critical,CA:FALSE",
        ], check=True, capture_output=True)
        cls.uv_wheel = wheel("uv", "0.13.1", "fixture", uv_binary=cls.uv)
        cls.evidence = []

    @classmethod
    def tearDownClass(cls):
        if cls.report:
            Path(cls.report).write_text(json.dumps(cls.evidence, ensure_ascii=False, indent=2))

    def server(self, packages, auth=False):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.packages, server.auth, server.requests = packages, auth, []
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(self.cert, self.root / "loopback.key")
        server.socket = context.wrap_socket(server.socket, server_side=True)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(thread.join)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return server, f"https://localhost:{server.server_port}/simple/"

    def state(self):
        temporary = tempfile.TemporaryDirectory(dir=self.root)
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        for name in ["home", "data", "cache", "config", "store", "installs", "certs"]:
            (root / name).mkdir()
        runtime = root / "installs/python/3.12.14"
        (runtime / "bin").mkdir(parents=True)
        (runtime / "bin/python3").symlink_to(self.python)
        (runtime / ".osdk-complete").touch()
        env = {
            "HOME": str(root / "home"), "USERPROFILE": str(root / "home"),
            "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8",
            "SSL_CERT_FILE": str(self.cert), "SSL_CERT_DIR": str(root / "certs"),
            "PIP_CERT": str(self.cert), "PIP_NO_INPUT": "1",
            "PIP_DISABLE_PIP_VERSION_CHECK": "1", "UV_SYSTEM_CERTS": "1",
            "OSDK_TRUSTED_CONFIG_PATHS": str(root),
        }
        env.update({f"OSDK_{name}_DIR": str(root / name.lower()) for name in ["DATA", "CACHE", "CONFIG", "STORE", "INSTALL"]})
        # The install root is named installs, not install.
        env["OSDK_INSTALL_DIR"] = str(root / "installs")
        env["LD_LIBRARY_PATH"] = str(Path(self.uv).parent.parent / "lib")
        return root, env

    def run_osdk(self, root, env, *args):
        result = subprocess.run([self.osdk, *args], cwd=root, env=env, capture_output=True, text=True, timeout=90)
        self.evidence.append({"test": self.id(), "args": args, "exit_code": result.returncode, "stdout": result.stdout, "stderr": result.stderr})
        return result

    def bootstrap_uv(self, root, env):
        _, index = self.server({"uv": [self.uv_wheel]})
        (root / "osdk.toml").write_text(f'[registries.python]\nurls = [{json.dumps(index)}]\n')
        result = self.run_osdk(root, env, "install", "pypi:uv@0.13.1")
        self.assertEqual(result.returncode, 0, result.stderr)
        candidates = list((root / "installs/pypi/uv").glob("*/b3-v2-*/bin/uv"))
        self.assertEqual(len(candidates), 1)
        # pip does not retain executable mode on generated wheel script fixtures.
        candidates[0].chmod(0o755)
        check = subprocess.run([str(candidates[0]), "--version"], env=env, capture_output=True, text=True)
        self.assertEqual(check.returncode, 0, check.stderr)
        self.assertIn("uv 0.13.1", check.stdout)

    def installed_origin(self, root):
        bins = list((root / "installs/pypi" / TOP).glob("*/b3-v2-*/bin/source-fixture"))
        self.assertEqual(len(bins), 1)
        result = subprocess.run([str(bins[0])], capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        receipt = json.loads((bins[0].parent.parent / ".osdk-pypi-env.json").read_text()) if (bins[0].parent.parent / ".osdk-pypi-env.json").exists() else None
        return json.loads(result.stdout), receipt

    def managed_case(self, installer, private_auth):
        root, env = self.state()
        if installer == "uv":
            self.bootstrap_uv(root, env)
        good, index = self.server({TOP: [wheel(TOP, "1.0.0", "osdk", dependency=True)], DEP: [wheel(DEP, "1.0.0", "osdk")]})
        bad, private = self.server({TOP: [wheel(TOP, "1.0.0", "native", dependency=True)], DEP: [wheel(DEP, "9.0.0", "native")]}, auth=private_auth)
        (root / "osdk.toml").write_text(f'[settings.precedence]\npython = "osdk"\n[registries.python]\nurls = [{json.dumps(index)}]\n')
        env.update(PIP_EXTRA_INDEX_URL=private, UV_INDEX=private, UV_EXTRA_INDEX_URL=private)
        result = self.run_osdk(root, env, "install", f"pypi:{TOP}@1.0.0")
        self.evidence.append({"test": self.id(), "managed_requests": good.requests, "native_requests": bad.requests})
        self.assertEqual(bad.requests, [], "osdk precedence contacted a native additional index")
        self.assertEqual(result.returncode, 0, result.stderr)
        origin, _ = self.installed_origin(root)
        self.assertEqual(origin, {"top": "osdk", "dep": "osdk"})
        self.assertTrue(any(path.endswith(".whl") for path in good.requests))

    def uv_fallback_case(self, version):
        root, env = self.state()
        first, primary = self.server({TOP: [wheel(TOP, "1.0.0", "native", dependency=True)], DEP: [wheel(DEP, "1.0.0", "native")]})
        second, default = self.server({TOP: [wheel(TOP, "2.0.0", "default", dependency=True)], DEP: [wheel(DEP, "9.0.0", "default")]})
        (root / "osdk.toml").write_text("")
        env.update(UV_INDEX=primary, UV_DEFAULT_INDEX=default)
        result = self.run_osdk(root, env, "install", f"pypi:{TOP}@{version}")
        self.evidence.append({"test": self.id(), "primary_requests": first.requests, "default_requests": second.requests})
        self.assertNotEqual(result.returncode, 0, "pip silently changed uv first-index semantics")
        self.assertIn("requires uv", result.stderr)
        self.assertEqual(first.requests, [])
        self.assertEqual(second.requests, [])
        self.assertFalse(list((root / "installs/pypi" / TOP).glob("*/b3-v2-*/bin/source-fixture")))

    def test_uv_extra_without_uv_rejects_top_level_version(self):
        self.uv_fallback_case("2.0.0")

    def test_uv_extra_without_uv_rejects_dependency_merge(self):
        self.uv_fallback_case("1.0.0")

    def test_real_uv_preserves_first_index_for_top_and_dependency(self):
        root, env = self.state()
        self.bootstrap_uv(root, env)
        first, primary = self.server({TOP: [wheel(TOP, "1.0.0", "native", dependency=True)], DEP: [wheel(DEP, "1.0.0", "native")]})
        second, default = self.server({TOP: [wheel(TOP, "2.0.0", "default", dependency=True)], DEP: [wheel(DEP, "9.0.0", "default")]})
        (root / "osdk.toml").write_text("")
        env.update(UV_INDEX=primary, UV_DEFAULT_INDEX=default)
        rejected = self.run_osdk(root, env, "install", f"pypi:{TOP}@2.0.0")
        self.assertNotEqual(rejected.returncode, 0)
        installed = self.run_osdk(root, env, "install", f"pypi:{TOP}@1.0.0")
        self.assertEqual(installed.returncode, 0, installed.stderr)
        origin, _ = self.installed_origin(root)
        self.assertEqual(origin, {"top": "native", "dep": "native"})
        self.assertEqual(second.requests, [], "uv consulted a lower index for duplicate names")
        self.evidence.append({"test": self.id(), "primary_requests": first.requests, "default_requests": second.requests, "origin": origin})

    def test_named_uv_environment_override_excludes_old_source(self):
        root, env = self.state()
        self.bootstrap_uv(root, env)
        new, new_url = self.server({})
        old, old_url = self.server({TOP: [wheel(TOP, "1.0.0", "old", dependency=True)], DEP: [wheel(DEP, "1.0.0", "old")]})
        default, default_url = self.server({TOP: [wheel(TOP, "2.0.0", "default", dependency=True)], DEP: [wheel(DEP, "2.0.0", "default")]})
        config = root / "home/.config/uv/uv.toml"
        config.parent.mkdir(parents=True)
        config.write_text(f'[[index]]\nname="corp"\nurl={json.dumps(old_url)}\n')
        (root / "osdk.toml").write_text("")
        env.update(UV_INDEX="corp=" + new_url, UV_DEFAULT_INDEX=default_url)
        # Real uv is the oracle for native named-index replacement.
        oracle = root / "oracle"
        subprocess.run([self.python, "-m", "venv", str(oracle)], env=env, check=True, capture_output=True)
        direct = subprocess.run([self.uv, "pip", "install", "--python", str(oracle / "bin/python"), TOP], cwd=root, env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(direct.returncode, 0, direct.stderr)
        self.assertEqual(old.requests, [])
        oracle_origin = subprocess.check_output([str(oracle / "bin/python"), "-c", "import osdk_source_fixture;print(osdk_source_fixture.ORIGIN)"], text=True).strip()
        self.assertEqual(oracle_origin, "default")
        for server in [new, old, default]:
            server.requests.clear()
        listed = self.run_osdk(root, env, "list-remote", f"pypi:{TOP}")
        self.assertEqual(listed.returncode, 0, listed.stderr)
        self.assertEqual(listed.stdout.strip(), "2.0.0")
        self.assertEqual(old.requests, [], "version lookup resurrected an overridden index")
        result = self.run_osdk(root, env, "install", f"pypi:{TOP}@latest")
        self.assertEqual(result.returncode, 0, result.stderr)
        origin, _ = self.installed_origin(root)
        self.assertEqual(origin, {"top": "default", "dep": "default"})
        self.assertEqual(old.requests, [], "install resurrected an overridden index")
        self.assertTrue(new.requests)
        self.assertTrue(default.requests)
        install = next(path for path in (root / "installs/pypi" / TOP).glob("*/b3-v2-*") if path.is_dir())
        receipt = json.loads((install / ".osdk-pypi-env.json").read_text())
        self.assertEqual(receipt["creator"], "uv")
        self.evidence.append({"test": self.id(), "new_requests": new.requests, "old_requests": old.requests, "default_requests": default.requests, "origin": origin, "creator": receipt["creator"]})

    def test_managed_pip_auth_index_gets_zero_requests(self):
        self.managed_case("pip", True)

    def test_managed_pip_same_name_packages_get_zero_requests(self):
        self.managed_case("pip", False)

    def test_managed_uv_auth_index_gets_zero_requests(self):
        self.managed_case("uv", True)

    def test_managed_uv_same_name_packages_get_zero_requests(self):
        self.managed_case("uv", False)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--osdk", required=True)
    parser.add_argument("--python", required=True)
    parser.add_argument("--uv", required=True)
    parser.add_argument("--report")
    arguments, tests = parser.parse_known_args()
    SourceTests.osdk = str(Path(arguments.osdk).resolve())
    SourceTests.python = str(Path(arguments.python).resolve())
    SourceTests.uv = str(Path(arguments.uv).resolve())
    SourceTests.report = arguments.report
    unittest.main(argv=[__file__, *tests], verbosity=2)
