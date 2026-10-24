# 声明式 Backend

配置参考：[适用的 tool_order 来源](../config/tool-selection#tool-order-reference)、[项目来源诊断](../config/tool-selection#inspect-sources)、[本工具的选择器、托管变量与缓存覆盖](../config/tool-environment#per-tool-environment)。

osdk 启动时自动加载以下目录直接子级的 `*.toml`，无需单独安装命令：

```text
<config>/plugins/*.toml
<data>/plugins/*.toml
```

config 目录先加载，再加载 data 目录。任何非法定义或与内置/其他 backend 的 ID/别名
冲突都会使 registry 加载失败，不允许 shadow。只加载普通文件，不递归；每个文件
最大 1 MiB。

## Schema 1 示例

```toml
schema = 1
id = "acme"
bin_paths = ["bin"]
bin_names = ["acme", "acmectl"]
idiomatic_files = [".acme-version"]

[versions]
values = ["1.0.0", "1.2.3"]
# 或 url = "https://example.test/{os}/{arch}/versions.txt"

[archive]
url = "https://example.test/{version}/{file}"
file = "acme-{version}-{os}-{arch}.tar.gz"
kind = "tar.gz"          # tar.gz|tar.xz|tar.zst|zip|7z
strip_root = true

[archive.checksum]
algorithm = "sha256"     # sha256|sha512|blake3
url = "{archive_url}.sha256"
# 或 value = "<固定十六进制摘要>"
# 或者当上游根本不发布摘要文件时：
#   attestation = { repo = "owner/repo" }

# 可选。编译工具链仅靠 PATH 无法使用，需要通过这类变量把构建系统指向它。
[env]
CC = "{install_path}/bin/acme-gcc"
SYSROOT = "{install_path}/sysroot"
```

## 描述工具链环境

`PATH` 和 shim 让工具的命令可以运行，对解释器或 CLI 来说这就够了。C/C++ 工具链
不同：CMake、Autoconf、Make 通过环境变量定位交叉编译器，因此只把 `bin/` 加入
`PATH` 的定义，装出来的编译器构建系统依然找不到。

可选的 `[env]` 表补上这个缺口。它声明的每个变量都会在该版本激活时导出——包括
`osdk exec`、shell 激活以及 shim。

```toml
[env]
CC = "{install_path}/bin/aarch64-none-elf-gcc"
CXX = "{install_path}/bin/aarch64-none-elf-g++"
AR = "{install_path}/bin/aarch64-none-elf-ar"
SYSROOT = "{install_path}/aarch64-none-elf"
ACME_RELEASE = "{version}"
```

`{install_path}` 展开为该版本自己的安装根，因此取值不必写出宿主机绝对路径；
`{version}` 和 `{id}` 同样可用。

由于这些变量会进入子进程，它是数据式定义唯一可能把构建指向任意宿主状态的地方。
取值因此受到限制，且下列规则都在**解析定义时**而不是激活时强制执行：

- 变量名只能使用 ASCII 字母、数字和 `_`，且不能以数字开头；
- `PATH` 为保留名——目录请通过 `bin_paths` 声明，以保证 shim 生成与激活行为一致。
  `LD_PRELOAD`、`LD_LIBRARY_PATH`、`DYLD_INSERT_LIBRARIES`、`DYLD_LIBRARY_PATH`
  同样保留，因为它们会把进程或动态加载器重定向到安装根之外。保留名不区分大小写；
- 取值不能是绝对路径、不能包含 `..`、不能包含控制字符，从而始终留在
  `{install_path}` 锚定的安装根内；
- 仅接受 `{install_path}`、`{version}`、`{id}`；其他占位符一律失败，渲染后仍残留
  占位符会直接报错，而不是导出一个奇怪的值。

声明了 `[env]` 的定义仍然不能执行代码：它只描述变量，由 osdk 负责导出。

## 上游改名时怎么办

单一模板隐含一个假设：上游会永远保持一致的命名。真实项目不会。LLVM 就是活例子：
Linux x86-64 上它先发布 `clang+llvm-18.1.8-x86_64-linux-gnu-ubuntu-18.04.tar.xz`，
到 19.1.0 改成了 `LLVM-19.1.0-Linux-X64.tar.xz`——词序不同、大小写不同，还嵌了一个
无法从任何平台信息推导出来的发行版号。而 Windows 至今没改，所以**同一个 release 里
两套命名同时存在**。

`[[archive.overrides]]` 就是为此存在。每条 override 声明它适用的条件和要替换的字段：

```toml
[archive]
url = "https://github.com/llvm/llvm-project/releases/download/llvmorg-{version}/{file}"
file = "LLVM-{version}-Linux-X64.tar.xz"
kind = "tar.xz"
strip_root = true

# 19.1.0 之前 Linux 用旧命名。`ubuntu-18.04` 这段无法推导，只能写死。
[[archive.overrides]]
versions = "<19.1.0"
os = "linux"
arch = "x86_64"
file = "clang+llvm-{version}-x86_64-linux-gnu-ubuntu-18.04.tar.xz"

# Windows 在这个分界线两侧都保持旧命名。
[[archive.overrides]]
os = "windows"
arch = "x86_64"
file = "clang+llvm-{version}-x86_64-pc-windows-msvc.tar.xz"
```

条件包括 `versions`（semver 需求，如 `<19.1.0` 或 `>=17, <18`）以及 `os`、`arch`、
`libc`。它们是**联合**关系：只有一条 override 设置的所有条件都成立时才适用——这正是
LLVM 需要的，因为改名是按平台发生的，而不是整个 release 一起改。

override 可替换 `url`、`file`、`kind`、`strip_root` 和 `checksum`。未设置的字段回退到
`[archive]`，所以只改文件名的 override 不必重复其余内容。

两条规则保证结果可预测：

- **最具体的匹配优先**，按设置的条件数量计算，**与声明顺序无关**。重排定义文件不会
  改变最终安装哪个归档；
- **两条同等具体的匹配会报错**，在解析该版本与平台时报出。按顺序取其一会让结果依赖
  一个很容易被无意改动的因素，因此 osdk 要求你收窄其中一条。

条件同时接受两种 arch 写法（`x64` 与 `x86_64` 都匹配 x86-64），因此不必在
`{arch_llvm}` 之外再学一套词汇。非 semver 的版本永远不满足 `versions` 需求，此时回退
到默认值而不是让安装失败。

## 用 attestation 作为摘要来源

有些上游根本不发布摘要文件。LLVM 就是例子：它的 release 带 `.sig`（GPG），从 19.1.0
起还带 `.jsonl` sigstore bundle，但没有 `.sha256`。而 sigstore bundle 的 in-toto
subject 里本来就含有该制品的 SHA-256，因此这个 attestation 既是签名也是摘要来源——不需
要再去取第二个文件。

```toml
[archive.checksum]
algorithm = "sha256"
attestation = { repo = "llvm/llvm-project" }
```

这与 `gh attestation verify --repo llvm/llvm-project` 做的是同一件事，且**不需要**
`gh` CLI：osdk 用内嵌的 trusted root 验证 bundle，并要求证书来自 GitHub Actions 的
OIDC issuer、且来自你指定仓库的工作流。

由于摘要只有在字节落盘之后才能得知，验证发生在下载**之后**而不是之前。但归档仍然不会
在未验证的情况下被解压——attested 摘要会成为流水线实际强制校验的 checksum。

依赖它之前有两点需要知道：

- **此时验证是强制的。** 当 attestation **就是**摘要来源时，osdk 会无视全局
  `attestations` 设置（其默认值是 `off`）强制要求验证。否则一个声明了 attestation 的
  定义反而会装上一个完全没有完整性证据的归档；
- **覆盖范围并不完整。** 只有上游启用 attestation 之后、由 GitHub Actions 工作流构建
  的制品才有。对 LLVM 而言意味着 19.1.0 及之后，而且即便如此每个 release 也只有部分
  制品有——它的 Windows 归档完全没有 attestation。`[[archive.overrides]]` 之所以能按
  版本和平台切换摘要来源，正是为了应对这种情况。

## 验证与安全边界

- `versions.values` 与 `versions.url` 必须且只能设置一个，最多 10,000 个版本；
- ID 首字符为小写 ASCII 字母或数字，其余仅小写字母、数字、`-`、`_`，并保留
  `github` namespace；
- `bin_paths`、`bin_names` 不能为空，所有路径/名称必须安全且留在安装根内；
- archive URL 或文件名至少一处必须随 `{version}` 变化；每条 `[[archive.overrides]]`
  同样如此，且按其继承后的字段计算；
- 每条 `[[archive.overrides]]` 必须至少设置一个条件（`versions`、`os`、`arch`、
  `libc`），并且至少替换一个字段——这样一条 override 既不会无条件覆盖默认值，也不会
  占用一次匹配却什么都不改；`versions` 必须是合法的 semver 需求。最具体的匹配优先，
  两条同等具体的匹配会报错而不是按顺序取其一；
- archive 类型仅为 `tar.gz|tar.xz|tar.zst|zip|7z`；支持 `.7z` 是因为 Windows GCC
  工具链通常只以该格式发布，解压时会校验条目路径，归档无法写到解压目录之外；
- checksum 的 `value`、`url`、`attestation` 必须且只能设置一个；长度必须符合算法。
  `attestation` 要求 `algorithm = "sha256"`，因为 GitHub attestation 的 subject 带的
  就是 SHA-256 摘要；其 `repo` 必须是纯粹的 `owner/repo`——该值会成为 sigstore 的证书
  身份策略，因此会被校验而不是原样透传。使用 attestation 时摘要一定会被验证，即使全局
  `attestations` 设置为 `off`；
- versions/archive URL 只接受 HTTP(S)，checksum URL 额外可基于 `{archive_url}`；
- 允许的模板变量按位置为 `{id}`、`{version}`、`{os}`、`{arch}`、`{arch_llvm}`、
  `{libc}`、`{file}`、`{archive_url}`；不支持的变量会失败；`[env]` 取值仅接受
  `{install_path}`、`{version}`、`{id}`；
- `{arch}` 渲染 osdk 的短 token（`x64`、`arm64`、`x86`、`arm`），`{arch_llvm}` 渲染
  LLVM target triple 的 CPU 部分（`x86_64`、`aarch64`、`i686`、`armv7`）。编译器与
  工具链归档通常按 triple 命名发布，这类情况用 `{arch_llvm}`；遵循 Node 式命名的
  runtime 用 `{arch}`；
- `[env]` 变量名不能是 `PATH` 或动态加载器变量，取值必须是相对路径、不含 `..`
  且锚定在安装根内；
- schema 使用严格未知字段拒绝，因此不能加入 hook 或 install script。

声明式 backend 只描述数据，不能执行自定义代码；安装仍经过统一下载、checksum、
受限解压和 CAS 物化管线。`osdk lock` 记录 artifact receipt 后，后续不带参数的
`osdk install` 会先使用其中锁定的 URL、文件名、checksum 与可选子目录，再考虑当前
插件模板。因此即使版本/checksum 端点不可用，或本地插件定义后来发生变化，已缓存产物
仍能离线重装。

Fish 激活和撤销会把不存在的可选保存变量当作成功的空操作；脚本中重复激活或撤销
不会因此得到非零退出状态。

新的 Go/Cargo、npm、HTTP、GitHub 和 Conda 动态安装也会在发布前记录完整性清单。旧安装若没有基线，必须重装；校验不会用可能已被改动的文件自动补造基线。

`install --force` 会在持有安装锁时清除精确解析出的动态安装身份的完成标记，使 Go/Cargo 原生工具真正重建，而不是静默复用旧产物。
