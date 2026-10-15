#!/usr/bin/env python3
"""Real, bounded npm/pnpm/uv smoke. State and native caches are temp-isolated.

Usage: python3 scripts/test-deps-monorepo.py [--bin target/debug/osdk]
Optional --state-prefix points at a previously created, isolated test state.
Uses only small packages; never downloads models. This is an opt-in live check.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

parser = argparse.ArgumentParser()
parser.add_argument('--bin', default='target/debug/osdk')
parser.add_argument('--state-prefix')
args = parser.parse_args()
binary = Path(args.bin).resolve()
with tempfile.TemporaryDirectory(prefix='osdk-deps-live-') as temporary:
    base = Path(temporary)
    root = base / 'repo'
    root.mkdir()
    state = Path(args.state_prefix).resolve() if args.state_prefix else base / 'state'
    environment = os.environ.copy()
    for name, directory in [('HOME', base / 'home'), ('OSDK_CONFIG_DIR', state / 'config'),
                            ('OSDK_DATA_DIR', state / 'data'), ('OSDK_CACHE_DIR', state / 'cache'),
                            ('OSDK_INSTALL_DIR', state / 'installs'), ('OSDK_STORE_DIR', state / 'store')]:
        directory.mkdir(parents=True, exist_ok=True)
        environment[name] = str(directory)
    environment.pop('OSDK_TRUSTED_CONFIG_PATHS', None)
    environment.update(UV_CACHE_DIR=str(base / 'uv-cache'), npm_config_cache=str(base / 'npm-cache'))

    def write(path, text):
        path = root / path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def run(*arguments, cwd=root, success=True):
        output = subprocess.run([str(binary), *arguments], cwd=cwd, env=environment,
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=240)
        print('$ osdk ' + ' '.join(arguments), flush=True)
        print(output.stdout, flush=True)
        assert (output.returncode == 0) == success, output.stdout
        return output.stdout

    tools = '[tools]\nnode="24.19.0"\npython="3.12.12"\npnpm="11.25.0"\n"pypi:uv"="0.12.23"\n'
    declarations = '\n'.join(f'[deps.projects."{path}".{provider}]\ntimeout="2m"\n'
                              for path, provider in [('npm', 'npm'), ('pnpm', 'pnpm'),
                                                     ('requirements', 'pip-requirements'), ('js', 'pnpm'), ('py', 'uv')])
    write('osdk.toml', tools + declarations)
    for directory, manager in [('npm', 'npm@11.9.0'), ('pnpm', 'pnpm@11.25.0')]:
        write(f'{directory}/package.json', json.dumps({'name': directory, 'private': True,
              'packageManager': manager, 'dependencies': {'is-number': '7.0.0'}}))
    write('requirements/requirements.txt', 'idna==3.10\n')
    write('js/package.json', '{"name":"js-root","private":true,"packageManager":"pnpm@11.25.0"}')
    write('js/pnpm-workspace.yaml', "packages: ['packages/*', '!packages/excluded']\n")
    for name, package, version in [('a', 'is-number', '7.0.0'), ('b', 'is-odd', '3.0.1')]:
        write(f'js/packages/{name}/package.json', json.dumps({'name': name, 'private': True, 'dependencies': {package: version}}))
    write('js/packages/excluded/package.json', 'invalid excluded sentinel')
    write('py/pyproject.toml', '[project]\nname="py-root"\nversion="0.1.0"\nrequires-python=">=3.12,<3.13"\n[tool.uv]\npackage=false\n[tool.uv.workspace]\nmembers=["packages/*"]\nexclude=["packages/excluded"]\n')
    for name, dependency in [('a', 'idna==3.10'), ('b', 'six==1.17.0')]:
        write(f'py/packages/{name}/pyproject.toml', f'[project]\nname="{name}"\nversion="0.1.0"\ndependencies=["{dependency}"]\n[tool.uv]\npackage=false\n')
    write('py/packages/excluded/pyproject.toml', 'invalid excluded sentinel')
    # One root invocation prepares known tools and installs every closure.
    run('deps')
    assert (root / 'npm/node_modules/is-number/package.json').is_file()
    assert (root / 'pnpm/node_modules/is-number/package.json').is_file()
    assert (root / 'js/packages/a/node_modules/is-number/package.json').is_file()
    assert not (root / 'js/packages/a/pnpm-lock.yaml').exists()
    for directory, modules in [('requirements', 'idna'), ('py', 'idna,six')]:
        python = root / directory / '.venv' / ('Scripts/python.exe' if os.name == 'nt' else 'bin/python')
        subprocess.run([str(python), '-c', 'import ' + modules], env=environment, check=True)
    run('deps')  # Generated locks change the plan once to the locked form.
    assert 'would run in' not in run('deps', '--dry-run')
    text = run('deps', '--dry-run', '--force', '--filter', 'js/packages/a')
    assert text.count('would run in') == 1 and 'coverage expands' in text
    # Workspace declarations, member dependencies and membership affect freshness.
    original = (root / 'js/packages/a/package.json').read_text()
    write('js/packages/a/package.json', original.replace('7.0.0', '6.0.0'))
    assert 'would run in' in run('deps', '--dry-run', '--filter', 'js/packages/a')
    failure = run('deps', '--frozen', '--filter', 'js/packages/a', success=False)
    assert 'lockfile' in failure.lower(), failure
    assert 'would run in' in run('deps', '--dry-run', '--filter', 'js/packages/a')
    write('js/packages/a/package.json', original)
    write('py/packages/c/pyproject.toml', '[project]\nname="c"\nversion="0.1.0"\n[tool.uv]\npackage=false\n')
    assert 'would run in' in run('deps', '--dry-run', '--filter', 'py')
    shutil.rmtree(root / 'py/packages/c')
    saved = (root / 'py/packages/b/pyproject.toml').read_text()
    shutil.rmtree(root / 'py/packages/b')
    assert 'would run in' in run('deps', '--dry-run', '--filter', 'py')
    write('py/packages/b/pyproject.toml', saved)
    # Empty isolated uv cache plus a changed dependency must fail offline.
    environment['UV_CACHE_DIR'] = str(base / 'empty-uv-cache')
    write('requirements/requirements.txt', 'idna==3.9\n')
    failure = run('--offline', 'deps', '--force', '--filter', 'requirements', success=False)
    assert 'offline' in failure.lower() or 'network' in failure.lower(), failure
    assert 'would run in' in run('--offline', 'deps', '--dry-run', '--filter', 'requirements')
    environment['UV_CACHE_DIR'] = str(base / 'uv-cache')
    write('requirements/requirements.txt', 'idna==3.10\n')
    # Inheritance preserves cwd-independent effective configuration.
    write('npm/osdk.toml', 'extends="../osdk.toml"\n[deps.npm]\nauto=false\noutputs=[]\n')
    first = run('deps', '--list', '--all', '--explain', '--filter', 'npm')
    second = run('deps', '--list', '--all', '--explain', '--filter', 'npm', cwd=root / 'npm')
    assert first == second
    shutil.rmtree(root / 'npm/node_modules')
    run('install')
    assert not (root / 'npm/node_modules').exists(), 'auto=false must skip automatic materialization'
    run('deps', '--filter', 'npm')
    assert (root / 'npm/node_modules/is-number/package.json').is_file()
    (root / 'npm/osdk.toml').unlink()
    write('osdk.toml', tools + declarations + '[deps.projects."npm"]\ndisable=["npm"]\n')
    assert '//npm:npm' not in run('deps', '--list', '--all')
    # Success records the full native shared coverage, not only the selected root.
    coverage = '\n'.join(path.read_text() for path in (state / 'cache/deps').glob('*.toml'))
    assert 'coverage' in coverage and 'js/packages/a' in coverage and 'py/packages/b' in coverage
    print('PASS: actual installs, shared coverage, freshness, filters, auto/disable, inheritance, locked/offline failures', flush=True)
