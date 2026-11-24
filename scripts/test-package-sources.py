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
import sys
import tarfile
import tempfile
import threading
import zipfile


def pypi_uv_merged_indexes(osdk, uv):
    """The root wheel is in project index A, and its dependency is only in user index B."""
    osdk, uv = Path(osdk).resolve(), Path(uv).resolve()
    with tempfile.TemporaryDirectory(prefix='osdk-pypi-uv-') as temporary:
        root = Path(temporary)
        project = root / 'project'
        user_uv = root / 'user/uv'
        for directory in (project, user_uv, root / 'home', root / 'system/uv'):
            directory.mkdir(parents=True)
        (project / 'osdk.toml').touch()
        (root / 'system/uv/uv.toml').touch()
        interpreter = Path(sys.executable).resolve()
        version = '.'.join(map(str, sys.version_info[:3]))
        python_root = root / 'installs/python' / version
        python_bin = python_root / ('python.exe' if os.name == 'nt' else 'bin/python3')
        python_bin.parent.mkdir(parents=True)
        python_bin.symlink_to(interpreter)
        (python_root / '.osdk-complete').touch()

        calls = root / 'uv-calls'
        packages = {}

        def wheel(index, name, files, requires=''):
            filename = f'{name.replace("-", "_")}-1.0.0-py3-none-any.whl'
            metadata = filename.split('-py3-')[0] + '.dist-info/'
            buffer = io.BytesIO()
            with zipfile.ZipFile(buffer, 'w') as archive:
                archive.writestr(metadata + 'METADATA', f'Metadata-Version: 2.1\nName: {name}\nVersion: 1.0.0\n{requires}\n')
                archive.writestr(metadata + 'WHEEL', 'Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n')
                archive.writestr(metadata + 'RECORD', '')
                for path, content in files.items():
                    archive.writestr(path.replace('DIST-INFO/', metadata), content)
            packages[f'/{index}/simple/{name}/'] = ('text/html', f'<a href="{filename}">{filename}</a>'.encode())
            packages[f'/{index}/simple/{name}/{filename}'] = ('application/octet-stream', buffer.getvalue())

        wheel('bootstrap', 'uv', {
            'DIST-INFO/entry_points.txt': '[console_scripts]\nuv = uv_launcher:main\n',
            'uv_launcher.py': f'''import os, sys
def main():
    with open({str(calls)!r}, "a") as log:
        log.write(sys.argv[1] + "\\n")
    os.execv({str(uv)!r}, [{str(uv)!r}, *sys.argv[1:]])
''',
        })
        wheel('a', 'layered-tool', {'layered_tool.py': ''}, 'Requires-Dist: user-dep==1.0.0\n')
        wheel('a', 'single-tool', {'single_tool.py': ''})
        wheel('b', 'user-dep', {'user_dep.py': ''})
        requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_CONNECT(self):
                self.do_GET()

            def do_HEAD(self):
                self.do_GET()

            def do_GET(self):
                requests.append((self.path, self.headers.get('Authorization')))
                package = packages.get(self.path)
                if package is None:
                    self.send_error(404)
                    return
                content_type, body = package
                self.send_response(200)
                self.send_header('Content-Type', content_type)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                if self.command != 'HEAD':
                    self.wfile.write(body)

        with http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler) as server:
            base = f'http://127.0.0.1:{server.server_port}'
            env = {key: os.environ[key] for key in ('SYSTEMROOT', 'WINDIR') if key in os.environ}
            for name, directory in {
                'HOME': 'home', 'USERPROFILE': 'home', 'APPDATA': 'user', 'XDG_CONFIG_HOME': 'user',
                'PROGRAMDATA': 'system', 'XDG_CONFIG_DIRS': 'system', 'OSDK_DATA_DIR': 'data',
                'OSDK_CONFIG_DIR': 'config', 'OSDK_CACHE_DIR': 'cache', 'OSDK_STORE_DIR': 'store',
                'OSDK_INSTALL_DIR': 'installs', 'UV_CACHE_DIR': 'native-uv-cache',
            }.items():
                env[name] = str(root / directory)
            env.update(HTTP_PROXY=base, HTTPS_PROXY=base, NO_PROXY='127.0.0.1', UV_PYTHON_DOWNLOADS='never')
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                def run(*command, **overrides):
                    return subprocess.run(command, cwd=project, env=env | overrides,
                                          capture_output=True, text=True, timeout=30)

                bootstrap = run(osdk, 'install', 'pypi:uv@1.0.0',
                                PIP_INDEX_URL=base + '/bootstrap/simple/', PIP_CONFIG_FILE=os.devnull)
                assert bootstrap.returncode == 0, bootstrap.stderr
                a = base + '/a/simple/'
                b = base + '/b/simple/'
                (project / 'uv.toml').write_text(f'extra-index-url = ["{a}"]\n')
                (user_uv / 'uv.toml').write_text(f'extra-index-url = ["{b}"]\n')
                control = root / 'control-venv'
                created = run(uv, 'venv', '--python', interpreter, control)
                assert created.returncode == 0, created.stderr
                control_python = control / ('Scripts/python.exe' if os.name == 'nt' else 'bin/python')
                requests.clear()
                positive = run(uv, 'pip', 'install', '--python', control_python, 'layered-tool==1.0.0', UV_DEFAULT_INDEX=a)
                assert positive.returncode == 0, positive.stderr
                imported = run(control_python, '-c', 'import layered_tool, user_dep')
                assert imported.returncode == 0, imported.stderr
                assert any(path.startswith('/b/simple/user-dep/user_dep-') for path, _ in requests), requests
                assert all(path.startswith(('/a/', '/b/')) and auth is None for path, auth in requests), requests
                print('PASS: real uv downloads the dependency that exists only in user index B')

                calls.touch()
                for command in [('list-remote', 'pypi:layered-tool'), ('install', 'pypi:layered-tool@1.0.0')]:
                    requests.clear()
                    calls.write_text('')
                    result = run(osdk, *command)
                    assert result.returncode != 0 and 'uv configures multiple indexes' in result.stderr, (command, result)
                    assert not requests, requests
                    assert set(calls.read_text().splitlines()) == {'--version'}, calls.read_text()
                print('PASS: osdk rejects both version lookup and exact install before network or installer work')
                (user_uv / 'uv.toml').unlink()
                requests.clear()
                calls.write_text('')
                single = run(osdk, 'install', 'pypi:single-tool')
                assert single.returncode == 0, single.stderr
                assert 'venv' in calls.read_text().splitlines() and 'pip' in calls.read_text().splitlines()
                assert any(path.startswith('/a/simple/single-tool/single_tool-') for path, _ in requests), requests
                assert all(path.startswith('/a/') and auth is None for path, auth in requests), requests
                print('PASS: a single project index still serves version lookup and real uv installation')
            finally:
                server.shutdown()
                thread.join()


def main():
    parser = argparse.ArgumentParser()
    for manager in ('npm', 'pnpm', 'bun', 'uv'):
        parser.add_argument('--' + manager, default=shutil.which(manager))
    parser.add_argument('--yarn-classic')
    parser.add_argument('--yarn-berry')
    parser.add_argument('--osdk', help='run the standalone pypi/uv check only (also needs uv and venv-capable Python)')
    args = parser.parse_args()
    if args.osdk:
        if not args.uv:
            parser.error('the standalone pypi check requires --uv or uv on PATH')
        pypi_uv_merged_indexes(args.osdk, args.uv)
        return
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
            for false_value in ('0', 'false'):
                result = run([args.uv, '--verbose', 'pip', 'compile', 'requirements.in', '--python-version', '3.12', '--no-header', '--no-annotate'], project, {'UV_NO_CONFIG': false_value, 'UV_OFFLINE': false_value})
                assert 'fixture-pkg==9.0.0' in result, result
            # The sibling tool.uv settings must not override uv.toml.
            (project / 'pyproject.toml').write_text(f'[tool.uv]\nindex-url="{base}/first/"\n')
            result = run([args.uv, 'pip', 'compile', 'requirements.in', '--python-version', '3.12', '--no-header', '--no-annotate'], project)
            assert 'fixture-pkg==9.0.0' in result, result
            workspace = root / 'uv-workspace'; workspace.mkdir()
            member = workspace / 'member'; member.mkdir()
            (workspace / 'pyproject.toml').write_text('[project]\nname="workspace-root"\nversion="0.1.0"\nrequires-python=">=3.9"\ndependencies=["fixture-pkg"]\n[tool.uv]\nindex-url="' + base + '/first/"\n[tool.uv.workspace]\nmembers=["member"]\n')
            (member / 'pyproject.toml').write_text('[project]\nname="workspace-member"\nversion="0.1.0"\nrequires-python=">=3.9"\n[tool.uv]\nindex-url="' + base + '/second/"\n')
            (member / 'uv.toml').write_text(f'index-url="{base}/second/"\n')
            run([args.uv, 'lock'], member)
            locked = (workspace / 'uv.lock').read_text()
            assert 'version = "1.0.0"' in locked and 'version = "2.0.0"' not in locked, locked
            print(f'PASS {run([args.uv, "--version"], project)}: additional-index order, first-index, false booleans, uv.toml shadowing and workspace-root configuration')
        finally:
            server.shutdown(); server.server_close(); thread.join()


if __name__ == '__main__':
    main()
