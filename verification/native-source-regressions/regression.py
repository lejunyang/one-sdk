import argparse, contextlib, http.server, json, os, pathlib, subprocess, tempfile, threading, ssl

parser = argparse.ArgumentParser()
parser.add_argument('--bin', required=True)
parser.add_argument('--output', required=True)
args = parser.parse_args()
binary = str(pathlib.Path(args.bin).resolve())

class Index(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'v1.2.3\n' if self.path.endswith('/@v/list') else b'<a href="fixture-1.2.3-py3-none-any.whl">fixture-1.2.3-py3-none-any.whl</a>'
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass

@contextlib.contextmanager
def fixture():
    with tempfile.TemporaryDirectory(prefix='osdk-source-regression-') as directory:
        root = pathlib.Path(directory)
        for name in ['home', 'data', 'cache', 'config', 'store', 'installs']:
            (root / name).mkdir()
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Index)
        tls_server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Index)
        cert, key = root/'fixture.crt', root/'fixture.key'
        subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(key), '-out', str(cert), '-days', '1', '-subj', '/CN=127.0.0.1', '-addext', 'subjectAltName=IP:127.0.0.1', '-addext', 'basicConstraints=critical,CA:FALSE'], check=True, capture_output=True)
        (root/'empty-cert-dir').mkdir()
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(cert, key)
        tls_server.socket = tls.wrap_socket(tls_server.socket, server_side=True)
        tls_thread = threading.Thread(target=tls_server.serve_forever, daemon=True)
        tls_thread.start()
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        endpoint = f'http://127.0.0.1:{server.server_port}'
        env = {'HOME': str(root/'home'), 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8', 'GOENV': 'off', 'OSDK_TRUSTED_CONFIG_PATHS': str(root)}
        for key, name in [('OSDK_DATA_DIR','data'),('OSDK_CACHE_DIR','cache'),('OSDK_CONFIG_DIR','config'),('OSDK_STORE_DIR','store'),('OSDK_INSTALL_DIR','installs')]:
            env[key] = str(root/name)
        env['FIXTURE_TLS_INDEX'] = f'https://127.0.0.1:{tls_server.server_port}/simple/'
        env['SSL_CERT_FILE'] = str(cert)
        env['SSL_CERT_DIR'] = str(root/'empty-cert-dir')
        try:
            yield root, env, endpoint
        finally:
            tls_server.shutdown()
            tls_server.server_close()
            tls_thread.join()
            server.shutdown()
            server.server_close()
            thread.join()

def write_executable(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o755)

def python_runtime(root):
    path = root/'installs/python/3.12.0'
    write_executable(path/'bin/python3', '''#!/bin/sh
set -eu
[ "$1 $2" = '-m venv' ] || exit 10
mkdir -p "$3/bin"
printf '#!/bin/sh\nset -eu\n[ "${PIP_INDEX_URL:-}" = "$FIXTURE_EXPECT_DEFAULT" ] || exit 12\n[ "${PIP_EXTRA_INDEX_URL:-}" = "${FIXTURE_EXPECT_EXTRA:-}" ] || exit 13\nprintf "#!/bin/sh\\nexit 0\\n" > "${0%%/*}/fixture"\nchmod +x "${0%%/*}/fixture"\n' > "$3/bin/python"
chmod +x "$3/bin/python"
''')
    (path/'.osdk-complete').write_text('')

def go_runtime(root):
    path = root/'installs/go/1.24.0'
    write_executable(path/'bin/go', '''#!/bin/sh
set -eu
case "$1" in
  list) printf '{"Path":"example.com/tool","Version":"v9.0.0","Versions":["v9.0.0"]}\n' ;;
  install) mkdir -p "$GOBIN"; printf '#!/bin/sh\nexit 0\n' > "$GOBIN/tool"; chmod +x "$GOBIN/tool" ;;
  env) printf 'go-env-ok\n' ;;
  *) exit 2 ;;
esac
''')
    for name in ['bin/gofmt', 'pkg/tool/compile']:
        write_executable(path/name, '#!/bin/sh\nexit 0\n')
    (path/'src/runtime').mkdir(parents=True)
    (path/'src/runtime/runtime.go').write_text('package runtime\n')
    (path/'VERSION').write_text('go1.24.0\n')
    (path/'go.env').write_text('GOTOOLCHAIN=local\n')
    (path/'.osdk-complete').write_text('')

def run(root, env, argv):
    return subprocess.run([binary, *argv], cwd=root, env=env, capture_output=True, text=True, timeout=30)

results=[]
def record(name, body):
    try:
        with fixture() as (root, env, endpoint):
            output = body(root, env, endpoint)
            results.append({'name': name, 'passed': output.returncode == 0, 'exit_code': output.returncode, 'stdout': output.stdout, 'stderr': output.stderr})
    except Exception as error:
        results.append({'name': name, 'passed': False, 'error': str(error)})

def pip_case(mode):
    def body(root, env, endpoint):
        python_runtime(root)
        index=env['FIXTURE_TLS_INDEX']
        (root/'osdk.toml').write_text('[registries.python]\nurls = ['+json.dumps(index)+']\n')
        env['FIXTURE_EXPECT_DEFAULT']=index
        if mode=='netrc':
            (root/'home/.netrc').write_text('machine github.com login fixture\n')
        elif mode=='project_uv':
            (root/'pyproject.toml').write_text('[[tool.uv.index]]\nurl="https://pytorch.example/simple"\nexplicit=true\n')
        elif mode=='uv_vars':
            env.update(UV_OFFLINE='1', UV_ENV_FILE='unused.env', UV_FIND_LINKS='unused', UV_NO_INDEX='1')
        elif mode=='uv_extra':
            env['UV_INDEX']=index
            env['FIXTURE_EXPECT_DEFAULT']='https://pypi.org/simple/'
            env['FIXTURE_EXPECT_EXTRA']=index
        elif mode=='uv_named':
            env['UV_INDEX']='corp='+index
            env['FIXTURE_EXPECT_DEFAULT']='https://pypi.org/simple/'
            env['FIXTURE_EXPECT_EXTRA']=index
        elif mode=='pip_empty':
            config=root/'native-pip.conf'
            config.write_text('[global]\nindex-url = '+index+'\nextra-index-url = https://extra.example/simple/\n')
            env.update(PIP_CONFIG_FILE=str(config), PIP_EXTRA_INDEX_URL='')
        elif mode=='precedence':
            (root/'osdk.toml').write_text('[settings.precedence]\npython="osdk"\n[registries.python]\nurls=['+json.dumps(index)+']\n')
            env['PIP_INDEX_URL']='https://user:fixture@private.example/simple'
        return run(root,env,['install','pypi:fixture@1.2.3'])
    return body

for mode in ['netrc','project_uv','uv_vars','uv_extra','uv_named','pip_empty','precedence']:
    record('pypi_'+mode,pip_case(mode))

def go_config(endpoint):
    return '[sources]\nselection="ordered"\n[[sources."go:example.com/tool".custom]]\nid="fixture"\nkind="custom"\ndownload_url='+json.dumps(endpoint)+'\n'

def go_public(root,env,endpoint):
    (root/'osdk.toml').write_text(go_config(endpoint))
    env['GOPRIVATE']='private.example'
    return run(root,env,['list-remote','go:example.com/tool'])
record('go_public_privacy_no_runtime',go_public)

def go_locked(root,env,endpoint):
    go_runtime(root)
    (root/'osdk.toml').write_text('[tools]\ngo="1.24.0"\n"go:example.com/tool"="1.2.3"\n')
    (root/'osdk.lock').write_text('schema=4\n[platforms.linux-x64.tools.go]\nrequest="1.24.0"\nversion="1.24.0"\n[platforms.linux-x64.tools."go:example.com/tool"]\nrequest="1.2.3"\nversion="1.2.3"\n[platforms.linux-x64.tools."go:example.com/tool".native]\nruntime="go"\nruntime_version="1.24.0"\nreplay="version-only"\nsource="https://proxy.golang.org"\nmodule="example.com/tool"\n')
    env['GOPROXY']=endpoint+',direct'
    env['GOPRIVATE']='private.example'
    return run(root,env,['install'])
record('go_locked_proxy_different_native',go_locked)

def go_nested(root,env,endpoint):
    go_runtime(root)
    (root/'osdk.toml').write_text('[tools]\ngo="1.24.0"\n'+go_config(endpoint))
    env['OSDK_TEST_BIN']=binary
    return subprocess.run(['/bin/bash','--noprofile','--norc','-c','set -eu; eval "$("$OSDK_TEST_BIN" env --format bash)"; result=$("$OSDK_TEST_BIN" list-remote go:example.com/tool); [ "$result" = 1.2.3 ]'],cwd=root,env=env,capture_output=True,text=True,timeout=30)
record('go_env_nested_lookup',go_nested)

def goenv_directory(root,env,endpoint):
    go_runtime(root)
    (root/'osdk.toml').write_text('[tools]\ngo="1.24.0"\n')
    env['GOENV']=str(root/'home')
    return run(root,env,['exec','--','go','env','GOPROXY'])
record('go_unreadable_goenv',goenv_directory)

pathlib.Path(args.output).write_text(json.dumps({'binary':binary,'results':results},ensure_ascii=False,indent=2))
for result in results:
    print(('PASS' if result['passed'] else 'FAIL')+' '+result['name'])
raise SystemExit(0 if all(result['passed'] for result in results) else 1)
