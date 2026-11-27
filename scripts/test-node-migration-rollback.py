#!/usr/bin/env python3
"""Exercise real npm rollback on Linux with two pinned, managed Node releases."""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import threading
import unittest


SOURCE, TARGET = "20.19.0", "22.17.0"
CLI = "osdk-migration-cli"
MISSING = "osdk-migration-source-only"
VERSION = "1.0.0"
MODULES = "lib/node_modules"


def isolated_env(root):
    env = {"PATH": os.defpath, "LC_ALL": "C.UTF-8"}
    for key, dirname in {
        "HOME": "home",
        "OSDK_CONFIG_DIR": "config",
        "OSDK_DATA_DIR": "data",
        "OSDK_CACHE_DIR": "cache",
        "OSDK_STORE_DIR": "store",
        "OSDK_INSTALL_DIR": "installs",
    }.items():
        directory = root / dirname
        directory.mkdir(parents=True)
        env[key] = str(directory)
    return env


def run(env, cwd, *command, code=0, timeout=60):
    result = subprocess.run(command, env=env, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    if result.returncode != code:
        raise AssertionError(
            f"{command} exited {result.returncode}, expected {code}\n"
            f"{result.stdout}{result.stderr}"
        )
    return result


def npm(env, prefix, *args, **kwargs):
    env = env | {"PATH": f"{prefix / 'bin'}{os.pathsep}{env['PATH']}"}
    return run(env, Path(env["HOME"]), prefix / "bin/npm", *args, "--prefix", prefix, **kwargs)


def write_cli(path, marker):
    path.write_text(f"#!/usr/bin/env node\nconsole.log({json.dumps(marker)})\n", encoding="utf-8")
    path.chmod(0o755)


def write_project(project, name, marker):
    project.mkdir()
    manifest = {"name": name, "version": VERSION, "bin": {name: "cli.js"}}
    (project / "package.json").write_text(json.dumps(manifest), encoding="utf-8")
    write_cli(project / "cli.js", marker)


def pack(project, tarball):
    with tarfile.open(tarball, "w:gz") as archive:
        archive.add(project, arcname="package")
    return json.loads((project / "package.json").read_text(encoding="utf-8")), tarball


class Registry(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        path = self.path
        package = self.server.packages.get(path.removeprefix("/"))
        archive_name = path.removeprefix("/tarballs/").removesuffix(".tgz")
        if package:
            manifest, _ = package
            name, version = manifest["name"], manifest["version"]
            body = json.dumps({
                "name": name,
                "dist-tags": {"latest": version},
                "versions": {version: manifest | {
                    "dist": {"tarball": f"{self.server.url}/tarballs/{name}.tgz"},
                }},
            }).encode()
            content_type = "application/json"
        elif path.startswith("/tarballs/") and archive_name != MISSING and archive_name in self.server.packages:
            _, tarball = self.server.packages[archive_name]
            body, content_type = tarball.read_bytes(), "application/octet-stream"
        else:
            self.server.requests.append((path, 404))
            self.send_error(404)
            return
        self.server.requests.append((path, 200))
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class MigrationRollback(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Keep discovery outside the checkout even if its task runner sets TMPDIR.
        temporary = tempfile.TemporaryDirectory(prefix="osdk-node-migration-", dir="/tmp")
        cls.addClassCleanup(temporary.cleanup)
        cls.root = Path(temporary.name)
        env = isolated_env(cls.root / "seed")
        run(env, cls.root, cls.osdk, "install", f"node@{SOURCE}", f"node@{TARGET}", timeout=240)
        cls.nodes = Path(env["OSDK_INSTALL_DIR"]) / "node"
        for node_version, npm_version in [(SOURCE, "10.8.2"), (TARGET, "10.9.2")]:
            prefix = cls.nodes / node_version
            assert run(env, cls.root, prefix / "bin/node", "--version").stdout.strip() == f"v{node_version}"
            actual = npm(env, prefix, "--version").stdout.strip()
            assert actual == npm_version, (node_version, actual, npm_version)
        print(f"managed Node {SOURCE} (npm 10.8.2) -> {TARGET} (npm 10.9.2)", flush=True)

    def test_npm_link(self):
        self.verify_failure("npm-link", "LOCAL-BEFORE", "REGISTRY-1.0.0")

    def test_local_tarball(self):
        self.verify_failure("local-tarball", "LOCAL_TARBALL", "REGISTRY_TARBALL")

    def verify_failure(self, scenario, local_marker, registry_marker):
        root = self.root / scenario
        root.mkdir()
        local = root / "local"
        remote = root / "registry"
        missing = root / "missing"
        write_project(local, CLI, local_marker)
        write_project(remote, CLI, registry_marker)
        write_project(missing, MISSING, "NEW")
        _, local_tarball = pack(local, root / "local.tgz")
        corepack = self.nodes / TARGET / MODULES / "corepack"
        packages = {
            CLI: pack(remote, root / "registry.tgz"),
            MISSING: pack(missing, root / "missing.tgz"),
            "corepack": pack(corepack, root / "corepack.tgz"),
        }
        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Registry) as server:
            server.packages = packages
            server.url = f"http://127.0.0.1:{server.server_port}"
            server.requests = []
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                for method in ["npm", "osdk"]:
                    with self.subTest(method=method):
                        self.verify_target(
                            root / method, server, scenario, method, local,
                            local_tarball, local_marker,
                        )
            finally:
                server.shutdown()
                thread.join()

    def verify_target(self, root, server, scenario, method, local, local_tarball, local_marker):
        env = isolated_env(root) | {
            "npm_config_registry": server.url,
            "npm_config_fetch_retries": "0",
            "npm_config_audit": "false",
            "npm_config_fund": "false",
            "npm_config_update_notifier": "false",
        }
        source, target = (Path(env["OSDK_INSTALL_DIR"]) / "node" / v for v in (SOURCE, TARGET))
        shutil.copytree(self.nodes / TARGET, target, symlinks=True)
        project = root / "local project"
        shutil.copytree(local, project)
        if scenario == "npm-link":
            npm(env | {"npm_config_cache": str(root / "target-setup-cache")}, target, "link", project)
        else:
            npm(env | {"npm_config_cache": str(root / "target-setup-cache")}, target, "install", "-g", local_tarball)

        if method == "osdk":
            shutil.copytree(self.nodes / SOURCE, source, symlinks=True)
            source_cli = server.packages[CLI][1] if scenario == "npm-link" else local_tarball
            npm(env | {"npm_config_cache": str(root / "source-setup-cache")}, source,
                "install", "-g", source_cli, server.packages[MISSING][1])
        env["npm_config_cache"] = str(root / "empty-failure-cache")
        before = self.snapshot(env, target)
        self.assertEqual(before["output"], local_marker)
        if scenario == "npm-link":
            self.assertIsNotNone(before["package_link"])
            self.assertEqual(Path(before["resolved_bin"]), project / "cli.js")
        else:
            self.assertIsNone(before["package_link"])
            self.assertEqual(Path(before["resolved_bin"]), target / MODULES / CLI / "cli.js")
        self.assertIsNotNone(before["bin_link"])

        server.requests.clear()
        if method == "osdk":
            command = (self.osdk, "node", "migrate-packages", "--from", SOURCE, "--to", TARGET)
            preview = run(env, root, *command)
            planned = [line for line in preview.stdout.splitlines() if "would install" in line]
            self.assertEqual(planned, [f"would install {MISSING}@{VERSION}"])
            self.assertEqual(server.requests, [])
            failure = run(env, root, *command, "--apply", code=1)
            self.assertIn("restored the target Node package set", failure.stderr)
        else:
            failure = npm(env, target, "install", "-g", f"{MISSING}@{VERSION}", code=1)

        requests = list(server.requests)
        after = self.snapshot(env, target)
        if scenario == "npm-link":
            write_cli(project / "cli.js", "LOCAL-AFTER")
        edited_output = self.snapshot(env, target)["output"]
        print(json.dumps({
            "scenario": scenario, "method": method, "before": before, "after": after,
            "output_after_local_edit": edited_output, "exit": failure.returncode,
            "stderr": failure.stderr, "requests": requests,
        }, ensure_ascii=False), flush=True)
        self.assertIn("404", failure.stderr)
        self.assertIn((f"/{MISSING}", 200), requests)
        self.assertIn((f"/tarballs/{MISSING}.tgz", 404), requests)
        self.assertFalse(os.path.lexists(target / MODULES / MISSING))
        self.assertFalse(os.path.lexists(target / "bin" / MISSING))
        self.assertEqual(after, before)
        self.assertEqual(edited_output, "LOCAL-AFTER" if scenario == "npm-link" else local_marker)
        self.assertFalse(any(path.startswith(f"/{CLI}") or path == f"/tarballs/{CLI}.tgz" for path, _ in requests))

    @staticmethod
    def snapshot(env, prefix):
        package = prefix / MODULES / CLI
        launcher = prefix / "bin" / CLI
        packages = json.loads(npm(env, prefix, "ls", "-g", "--depth=0", "--json").stdout)
        return {
            "packages": {name: meta["version"] for name, meta in packages["dependencies"].items()},
            "package_link": os.readlink(package) if package.is_symlink() else None,
            "bin_link": os.readlink(launcher) if launcher.is_symlink() else None,
            "resolved_bin": str(launcher.resolve(strict=True)),
            "cli_sha256": hashlib.sha256((package / "cli.js").read_bytes()).hexdigest(),
            "output": run(env | {"PATH": f"{prefix / 'bin'}{os.pathsep}{env['PATH']}"},
                          prefix, launcher).stdout.strip(),
        }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--osdk", type=Path, required=True)
    args = parser.parse_args()
    MigrationRollback.osdk = args.osdk.resolve(strict=True)
    unittest.main(argv=[__file__], verbosity=2)
