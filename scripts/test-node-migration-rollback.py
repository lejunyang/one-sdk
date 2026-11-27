#!/usr/bin/env python3
"""Exercise real npm rollback on Linux with two pinned, managed Node releases."""

import argparse
from contextlib import contextmanager
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


def registry_env(root, url):
    return isolated_env(root) | {
        "npm_config_registry": url,
        "npm_config_fetch_retries": "0",
        "npm_config_audit": "false",
        "npm_config_fund": "false",
        "npm_config_update_notifier": "false",
    }


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
            if self.server.before_tarball:
                self.server.before_tarball(archive_name)
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


@contextmanager
def loopback_registry(packages):
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Registry) as server:
        server.packages = packages
        server.url = f"http://127.0.0.1:{server.server_port}"
        server.requests = []
        server.before_tarball = None
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield server
        finally:
            server.shutdown()
            thread.join()


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

    def test_relocated_target(self):
        root = self.root / "relocated-target"
        root.mkdir()
        project = root / "incoming"
        name = "osdk-migration-relocated"
        write_project(project, name, "RELOCATED")
        incoming = pack(project, root / "incoming.tgz")
        with loopback_registry({name: incoming}) as server:
            env = registry_env(root, server.url)
            source, target = (Path(env["OSDK_INSTALL_DIR"]) / "node" / v for v in (SOURCE, TARGET))
            for prefix in (source, target):
                shutil.copytree(self.nodes / prefix.name, prefix, symlinks=True)
            npm(env | {"npm_config_cache": str(root / "source-cache")}, source,
                "install", "-g", incoming[1])
            destination = root / "another directory" / TARGET
            destination.parent.mkdir()
            target.rename(destination)
            target.symlink_to(destination, target_is_directory=True)
            original_link = os.readlink(target)
            launcher = destination / "bin" / name
            self.assertFalse(os.path.lexists(launcher))

            env["npm_config_cache"] = str(root / "migration-cache")
            command = (self.osdk, "node", "migrate-packages", "--from", SOURCE, "--to", TARGET)
            self.assertIn(f"would install {name}@{VERSION}", run(env, root, *command).stdout)
            self.assertEqual(server.requests, [])
            apply = run(env, root, *command, "--apply")
            self.assertEqual(os.readlink(target), original_link)
            self.assertEqual(launcher.resolve(strict=True), destination / MODULES / name / "cli.js")
            output = run(env | {"PATH": f"{target / 'bin'}{os.pathsep}{env['PATH']}"},
                         root, target / "bin" / name).stdout.strip()
            print(json.dumps({"scenario": "relocated-target", "exit": apply.returncode,
                              "prefix_link": original_link, "output": output}), flush=True)
            self.assertEqual(output, "RELOCATED")

    def test_manpage_collision(self):
        self.verify_manpage_collision(force=False)

    def test_forced_manpage_collision(self):
        self.verify_manpage_collision(force=True)

    def test_forced_manpage_collision_in_linked_section(self):
        self.verify_manpage_collision(force=True, section_target="../manual-store/man1")

    def test_forced_manpage_collision_in_section_linked_within_man_tree(self):
        self.verify_manpage_collision(force=True, section_target="manual-pages")

    def test_forced_manpage_collision_from_legacy_uppercase_package(self):
        self.verify_manpage_collision(
            force=True, section_target="../manual-store/man1",
            incoming_name="Osdk-migration-manpage",
        )

    def verify_manpage_collision(self, force, section_target=None, incoming_name="osdk-migration-manpage"):
        root = self.root / self._testMethodName
        root.mkdir()
        original, incoming = root / "original", root / "incoming"
        collision, added = "osdk-collision.1", "osdk-added.1"
        write_project(original, CLI, "ORIGINAL")
        write_project(incoming, incoming_name, "INCOMING")
        for project, names in [(original, [collision]), (incoming, [added, collision])]:
            manifest = project / "package.json"
            data = json.loads(manifest.read_text(encoding="utf-8"))
            data["man"] = names
            if project == incoming and force:
                # Fail only after npm has made both man-page writes.
                data["scripts"] = {"postinstall": "node verify-overwrite.cjs"}
                (project / "verify-overwrite.cjs").write_text(
                    'const { readlinkSync } = require("node:fs");\n'
                    'const section = process.env.npm_config_prefix + "/share/man/man1/";\n'
                    f'const pages = {json.dumps([collision, added])};\n'
                    f'process.exit(pages.every(page => readlinkSync(section + page)'
                    f'.includes("/{incoming_name}/")) ? 47 : 48);\n',
                    encoding="utf-8",
                )
            manifest.write_text(json.dumps(data), encoding="utf-8")
            for name in names:
                (project / name).write_text(project.name, encoding="utf-8")
        _, original_tarball = pack(original, root / "original.tgz")
        incoming_fixture = pack(incoming, root / "incoming.tgz")
        with loopback_registry({incoming_name: incoming_fixture}) as server:
            env = registry_env(root, server.url)
            source, target = (Path(env["OSDK_INSTALL_DIR"]) / "node" / v for v in (SOURCE, TARGET))
            for prefix in (source, target):
                shutil.copytree(self.nodes / prefix.name, prefix, symlinks=True)
            npm(env | {"npm_config_cache": str(root / "source-cache")}, source,
                "install", "-g", "--ignore-scripts", incoming_fixture[1])
            man = target / "share/man/man1"
            if section_target:
                section_dir = man.parent / section_target
                section_dir.parent.mkdir(parents=True, exist_ok=True)
                man.rename(section_dir)
                man.symlink_to(section_target, target_is_directory=True)
            npm(env | {"npm_config_cache": str(root / "target-cache")}, target,
                "install", "-g", original_tarball)
            original_link = os.readlink(man / collision)
            original_destination = (man / collision).resolve(strict=True)
            self.assertEqual((man / collision).read_text(encoding="utf-8"), "original")
            self.assertFalse(os.path.lexists(man / added))

            if section_target:
                manual_dir = man.resolve(strict=True)
                independent = manual_dir / "independent.1"
                independent_link = manual_dir / "independent-link.1"

                def add_independent_manpages(name):
                    if name == incoming_name:
                        independent.write_text("independent page", encoding="utf-8")
                        independent_link.symlink_to(independent.name)

                server.before_tarball = add_independent_manpages

            env["npm_config_cache"] = str(root / "migration-cache")
            if force:
                env["npm_config_force"] = "true"
            failure = run(env, root, self.osdk, "node", "migrate-packages",
                          "--from", SOURCE, "--to", TARGET, "--apply", code=1)
            print(json.dumps({
                "scenario": self._testMethodName, "force": force, "exit": failure.returncode,
                "incoming_package": incoming_name, "section_target": section_target,
                "original_link": original_link, "current_link": os.readlink(man / collision),
                "added_link": os.path.lexists(man / added),
                "independent_file": section_target and independent.exists(),
                "independent_link": section_target and independent_link.is_symlink(),
                "stderr": failure.stderr,
            }), flush=True)
            self.assertIn("restored the target Node package set", failure.stderr)
            self.assertIn("npm error code 47" if force else "EEXIST", failure.stderr)
            self.assertIn((f"/tarballs/{incoming_name}.tgz", 200), server.requests)
            if section_target:
                self.assertEqual(os.readlink(man), section_target)
                self.assertEqual(independent.read_text(encoding="utf-8"), "independent page")
                self.assertEqual(os.readlink(independent_link), independent.name)
            self.assertEqual(os.readlink(man / collision), original_link)
            self.assertEqual((man / collision).resolve(strict=True), original_destination)
            self.assertEqual((man / collision).read_text(encoding="utf-8"), "original")
            self.assertFalse(os.path.lexists(man / added))
            self.assertFalse(os.path.lexists(target / MODULES / incoming_name))

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
        with loopback_registry(packages) as server:
            for method in ["npm", "osdk"]:
                with self.subTest(method=method):
                    self.verify_target(
                        root / method, server, scenario, method, local,
                        local_tarball, local_marker,
                    )

    def verify_target(self, root, server, scenario, method, local, local_tarball, local_marker):
        env = registry_env(root, server.url)
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


def build_osdk():
    repo = Path(__file__).resolve().parent.parent
    build = subprocess.run(
        ["cargo", "build", "--locked", "-p", "osdk-cli", "--bin", "osdk",
         "--message-format=json-render-diagnostics"],
        cwd=repo, stdout=subprocess.PIPE, text=True, check=True,
    )
    for line in build.stdout.splitlines():
        message = json.loads(line)
        if (message["reason"] == "compiler-artifact"
                and message["target"]["name"] == "osdk"
                and message["executable"]):
            return Path(message["executable"]).resolve(strict=True)
    raise RuntimeError("cargo did not report the built osdk executable")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--osdk", help="test this CLI binary instead of building the current checkout")
    args = parser.parse_args()
    MigrationRollback.osdk = Path(args.osdk).resolve(strict=True) if args.osdk else build_osdk()
    unittest.main(argv=[__file__], verbosity=2)
