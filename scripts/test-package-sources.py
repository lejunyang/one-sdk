#!/usr/bin/env python3
"""Native adapter contract checks. All files, caches and installs are temporary.
Run with PATH containing the desired Node/npm, pnpm, Bun and uv versions.
No dependency lifecycle scripts run; the fixture server serves only local data.
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
import subprocess
import tarfile
import tempfile
import threading
import zipfile


def main():
    parser = argparse.ArgumentParser()
    for manager in ('npm', 'pnpm', 'bun', 'uv'):
        parser.add_argument('--' + manager, default=shutil.which(manager))
    parser.add_argument('--yarn-classic')
    parser.add_argument('--yarn-berry')
    args = parser.parse_args()
    if not all(getattr(args, name) for name in ('npm', 'pnpm', 'bun', 'uv')):
        parser.error('npm, pnpm, bun and uv are required on PATH or through explicit paths')
    with tempfile.TemporaryDirectory(prefix='osdk-native-sources-') as tmp:
        root = Path(tmp)
        home = root / 'home'
        home.mkdir()
        env = {key: value for key, value in os.environ.items() if not key.upper().startswith(('NPM_', 'PNPM_', 'YARN_', 'BUN_', 'UV_', 'PIP_', 'OSDK_', 'XDG_'))}
        env.update(HOME=str(home), USERPROFILE=str(home), XDG_CONFIG_HOME=str(home / '.config'), NPM_CONFIG_CACHE=str(root / 'npm-cache'), UV_CACHE_DIR=str(root / 'uv-cache'), UV_PYTHON_DOWNLOADS='never')
        requests = []
        package = io.BytesIO()
        with tarfile.open(fileobj=package, mode='w:gz') as archive:
            data = json.dumps({'name': '@corp/fixture', 'version': '1.0.0'}).encode()
            info = tarfile.TarInfo('package/package.json'); info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
        tarball = package.getvalue()
        wheels = {}
        for version in ('1.0.0', '2.0.0', '9.0.0'):
            data = io.BytesIO()
            with zipfile.ZipFile(data, 'w') as wheel:
                prefix = f'fixture_pkg-{version}.dist-info/'
                wheel.writestr(prefix + 'METADATA', f'Metadata-Version: 2.1\nName: fixture-pkg\nVersion: {version}\n')
                wheel.writestr(prefix + 'WHEEL', 'Wheel-Version: 1.0\nGenerator: fixture\nRoot-Is-Purelib: true\nTag: py3-none-any\n')
                wheel.writestr(prefix + 'RECORD', '')
            wheels[version] = data.getvalue()

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_HEAD(self):
                self.do_GET()

            def do_GET(self):
                requests.append((self.path, self.headers.get('Authorization')))
                path = self.path
                body = b''
                content_type = 'application/json'
                if path.startswith('/corp/'):
                    if path.endswith('.tgz'):
                        body = tarball
                    else:
                        body = json.dumps({'name': '@corp/fixture', 'dist-tags': {'latest': '1.0.0'}, 'versions': {'1.0.0': {'name': '@corp/fixture', 'version': '1.0.0', 'dist': {'tarball': base + '/corp/fixture.tgz', 'integrity': 'sha512-' + base64.b64encode(hashlib.sha512(tarball).digest()).decode()}}}}).encode()
                elif path.startswith('/wheels/'):
                    version = path.split('fixture_pkg-')[1].split('-py3')[0]
                    body = wheels[version]
                    content_type = 'application/octet-stream'
                elif path.rstrip('/').endswith('fixture-pkg'):
                    version = {'first': '1.0.0', 'second': '2.0.0', 'default': '9.0.0'}[path.strip('/').split('/')[0]]
                    url = f'{base}/wheels/fixture_pkg-{version}-py3-none-any.whl'
                    body = f'<a href="{url}#sha256={hashlib.sha256(wheels[version]).hexdigest()}">fixture_pkg-{version}-py3-none-any.whl</a>'.encode()
                    content_type = 'text/html'
                else:
                    self.send_error(404); return
                self.send_response(200)
                self.send_header('Content-Type', content_type)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                if self.command != "HEAD": self.wfile.write(body)

        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        base = f'http://127.0.0.1:{server.server_port}'
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()

        def run(command, cwd, extra=None):
            result = subprocess.run(command, cwd=cwd, env=env | (extra or {}), capture_output=True, text=True, timeout=40)
            if result.returncode:
                # Fixture-only values; never print the inherited environment.
                raise AssertionError(f'{Path(command[0]).name} exit {result.returncode}: {result.stderr}')
            return result.stdout.strip()

        try:
            for manager in ('npm', 'pnpm', 'bun'):
                executable = getattr(args, manager)
                project = root / manager; project.mkdir()
                native = f'registry={base}/native/\n@corp:registry={base}/corp/\n//127.0.0.1:{server.server_port}/corp/:_authToken=fixture-only-token\n'
                (project / '.npmrc').write_text(native)
                (project / 'package.json').write_text(json.dumps({'name': 'fixture-root', 'private': True, 'dependencies': {'@corp/fixture': '1.0.0'}}))
                key = {'npm': 'npm_config_registry', 'pnpm': 'pnpm_config_registry', 'bun': 'BUN_CONFIG_REGISTRY'}[manager]
                override = {key: base + '/osdk/'}
                if manager == 'pnpm': override['npm_config_registry'] = base + '/osdk/'
                if manager != 'bun':
                    assert run([executable, 'config', 'get', 'registry'], project, override) == base + '/osdk/'
                start = len(requests)
                command = [executable, 'install', '--ignore-scripts']
                if manager == 'npm': command += ['--no-audit', '--no-fund']
                run(command, project, override)
                hits = requests[start:]
                assert any(path.startswith('/corp/') for path, _ in hits), (manager, hits)
                assert not any(path.startswith('/osdk/@corp') for path, _ in hits), (manager, hits)
                assert any(auth == 'Bearer fixture-only-token' for _, auth in hits), manager
                assert (project / '.npmrc').read_text() == native
                print(f'PASS {manager} {run([executable, "--version"], project)}: default override; native scoped route and auth preserved; file unchanged')
            if args.yarn_classic:
                project = root / 'yarn-classic'; project.mkdir()
                (project / 'package.json').write_text('{"name":"yarn-test","version":"1.0.0"}')
                native = f'registry "{base}/native/"\n'
                (project / '.yarnrc').write_text(native)
                value = run([args.yarn_classic, 'config', 'get', 'registry'], project, {'YARN_REGISTRY': base + '/osdk/'})
                assert value == base + '/osdk/', value
                assert (project / '.yarnrc').read_text() == native
                print(f'PASS Yarn Classic {run([args.yarn_classic, "--version"], project)}: environment overrides native registry without rewriting files')
            if args.yarn_berry:
                project = root / 'yarn-berry'; project.mkdir()
                (project / 'package.json').write_text('{"name":"yarn-test","packageManager":"yarn@4.10.3"}')
                native = f'npmRegistryServer: "{base}/native/"\nnpmScopes:\n  corp:\n    npmRegistryServer: "{base}/corp/"\n    npmAuthToken: "fixture-only-token"\n'
                (project / '.yarnrc.yml').write_text(native)
                extra = {'YARN_NPM_REGISTRY_SERVER': base + '/osdk/', 'YARN_ENABLE_TELEMETRY': '0'}
                value = run([args.yarn_berry, 'config', 'get', 'npmRegistryServer'], project, extra)
                assert value == base + '/osdk/', value
                scopes = run([args.yarn_berry, 'config', 'get', 'npmScopes', '--json'], project, extra)
                assert base + '/corp/' in scopes, scopes
                assert (project / '.yarnrc.yml').read_text() == native
                print(f'PASS Yarn Berry {run([args.yarn_berry, "--version"], project)}: default env override retains native scoped/auth configuration (configuration query)')
            project = root / 'python'; project.mkdir()
            (project / 'requirements.in').write_text('fixture-pkg\n')
            (project / 'uv.toml').write_text(f'index-url="{base}/default/"\n')
            result = run([args.uv, 'pip', 'compile', 'requirements.in', '--python-version', '3.12', '--no-header', '--no-annotate'], project, {'UV_DEFAULT_INDEX': base + '/default/', 'UV_INDEX': base + '/first/ ' + base + '/second/', 'UV_INDEX_STRATEGY': 'first-index'})
            assert 'fixture-pkg==1.0.0' in result, result
            assert 'fixture-pkg==9.0.0' not in result
            print(f'PASS {run([args.uv, "--version"], project)}: additional-index order and first-index; default remains lowest priority')
        finally:
            server.shutdown(); server.server_close(); thread.join()


if __name__ == '__main__':
    main()
