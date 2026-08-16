//! The message catalog: key -> (english, chinese).
//!
//! Keys are grouped by prefix: `msg.*` runtime messages, `err.*` error text,
//! `help.*` CLI help (about/long/args), `ex.*` examples. Chinese may be empty
//! to fall back to English.

use std::collections::HashMap;

/// Build the catalog. Called once behind a `Lazy`.
pub fn build() -> HashMap<&'static str, (&'static str, &'static str)> {
    let mut m: HashMap<&'static str, (&'static str, &'static str)> = HashMap::new();

    // ---- runtime messages -------------------------------------------------
    m.insert(
        "msg.installing",
        ("installing {tool} ...", "正在安装 {tool} ..."),
    );
    m.insert("msg.installed", ("installed {tool}", "已安装 {tool}"));
    m.insert(
        "msg.already_installed",
        ("{tool} already installed", "{tool} 已安装"),
    );
    m.insert("msg.uninstalled", ("uninstalled {tool}", "已卸载 {tool}"));
    m.insert("msg.cancelled", ("cancelled", "已取消"));
    m.insert(
        "msg.config_trusted",
        (
            "trusted project config {path} ({hash})",
            "已信任项目配置 {path}（{hash}）",
        ),
    );
    m.insert(
        "msg.config_untrusted",
        (
            "removed trust for project config {path}",
            "已取消信任项目配置 {path}",
        ),
    );
    m.insert(
        "msg.config_was_not_trusted",
        (
            "project config was not trusted: {path}",
            "项目配置原本未受信任：{path}",
        ),
    );
    m.insert(
        "msg.nothing_to_install",
        (
            "nothing to install (no tools given and no config pins found)",
            "没有可安装项（未提供工具，且配置中无固定版本）",
        ),
    );
    m.insert(
        "msg.pinned_global",
        (
            "pinned {tool}@{ver} in user config",
            "已在用户配置中固定 {tool}@{ver}",
        ),
    );
    m.insert(
        "msg.pinned_project",
        (
            "pinned {tool}@{ver} in {path}",
            "已在 {path} 中固定 {tool}@{ver}",
        ),
    );
    m.insert(
        "msg.no_tools_installed",
        ("no tools installed yet", "尚未安装任何工具"),
    );
    m.insert(
        "msg.no_matching_versions",
        ("(no matching versions)", "（没有匹配的版本）"),
    );
    m.insert(
        "msg.no_active",
        (
            "no active versions for this directory",
            "当前目录没有生效的版本",
        ),
    );
    m.insert(
        "msg.pruned",
        (
            "pruned {count} object(s), {size} freed",
            "已清理 {count} 个对象，释放 {size}",
        ),
    );
    m.insert(
        "msg.pruned_store",
        (
            "pruned {count} store object(s), {size} freed",
            "已清理存储区 {count} 个对象，释放 {size}",
        ),
    );
    m.insert(
        "msg.prune_dry_run",
        (
            "(dry-run) prune does not delete; run `osdk prune` to reclaim space",
            "（演练）prune 不会删除；运行 `osdk prune` 以回收空间",
        ),
    );
    m.insert(
        "msg.reshimmed",
        ("regenerated {count} shim(s)", "已重新生成 {count} 个 shim"),
    );
    m.insert(
        "msg.shim_bin_missing",
        (
            "warning: osdk-shim binary not found; skipping shim generation",
            "警告：未找到 osdk-shim 可执行文件；跳过 shim 生成",
        ),
    );
    m.insert(
        "msg.checksum_verified",
        ("checksum verified: {file}", "校验和已验证：{file}"),
    );
    m.insert(
        "msg.probing",
        ("probing sources for {tool} ...", "正在探测 {tool} 的源 ..."),
    );
    m.insert("msg.unreachable", ("unreachable", "不可达"));
    m.insert(
        "msg.source_added",
        (
            "added custom source {id} for {tool}",
            "已为 {tool} 添加自定义源 {id}",
        ),
    );
    m.insert(
        "msg.source_removed",
        (
            "removed custom source {id} from {tool}",
            "已从 {tool} 移除自定义源 {id}",
        ),
    );
    m.insert(
        "msg.source_not_found",
        (
            "no custom source {id} found for {tool}",
            "{tool} 未找到自定义源 {id}",
        ),
    );
    m.insert(
        "msg.source_pinned",
        ("pinned {tool} to source {id}", "已将 {tool} 固定到源 {id}"),
    );
    m.insert(
        "msg.source_unpinned",
        ("unpinned {tool}", "已取消 {tool} 的源固定"),
    );
    m.insert(
        "msg.sources_header",
        (
            "sources for {tool} (selection: {mode}):",
            "{tool} 的源（选择策略：{mode}）：",
        ),
    );
    m.insert(
        "msg.cache_cleared",
        (
            "cleared downloaded archives (CAS store + installs kept)",
            "已清理下载的归档（保留 CAS 存储区与已安装内容）",
        ),
    );
    m.insert(
        "prompt.uninstall",
        (
            "Uninstall {tool} and reclaim its unreferenced store objects?",
            "要卸载 {tool} 并回收其未引用的存储对象吗？",
        ),
    );
    m.insert(
        "prompt.cache_clean",
        (
            "Remove all downloaded archives from the shared cache?",
            "要从共享缓存中删除所有下载归档吗？",
        ),
    );
    m.insert(
        "prompt.prune",
        (
            "Delete all unreferenced objects from the content store?",
            "要删除内容存储中所有未引用的对象吗？",
        ),
    );
    m.insert(
        "prompt.trust_config",
        (
            "Trust project config {path} until its normalized content or path changes?",
            "要信任项目配置 {path}，直到其规范化内容或路径发生变化吗？",
        ),
    );
    m.insert("prompt.yes_no", ("[y/N]:", "[是/否]："));
    m.insert("label.pinned", ("[pinned]", "[已固定]"));
    m.insert("label.error", ("error", "错误"));
    m.insert("label.trusted", ("trusted", "已信任"));
    m.insert("label.stale", ("stale", "已失效"));

    // ---- user-visible tracing logs (info!/warn!) --------------------------
    // Structured fields (url/source/attempt/...) stay as machine-readable
    // fields; only the human message is localized. debug!/trace! stay English.
    m.insert(
        "log.checksum_verified",
        ("checksum verified", "校验和已验证"),
    );
    m.insert(
        "log.signature_verified",
        ("signature verified (minisign)", "签名已验证（minisign）"),
    );
    m.insert(
        "log.rustup_dist_server",
        ("rustup dist server", "rustup 分发服务器"),
    );
    m.insert(
        "log.download_failover",
        (
            "download failed, trying next source: {err}",
            "下载失败，尝试下一个源：{err}",
        ),
    );
    m.insert(
        "log.yarn_download_failed",
        ("yarn download failed: {err}", "yarn 下载失败：{err}"),
    );
    m.insert(
        "log.index_fetch_failover",
        (
            "index fetch failed, trying next: {err}",
            "索引获取失败，尝试下一个：{err}",
        ),
    );
    m.insert(
        "log.go_index_fetch_failed",
        ("go index fetch failed: {err}", "go 索引获取失败：{err}"),
    );
    m.insert(
        "log.pnpm_packument_failed",
        (
            "pnpm packument fetch failed: {err}",
            "pnpm packument 获取失败：{err}",
        ),
    );
    m.insert(
        "log.binary_download_failed",
        ("binary download failed: {err}", "二进制下载失败：{err}"),
    );
    m.insert(
        "log.stale_python_cache",
        (
            "network failed; using stale cached python catalog",
            "网络失败；改用过期的 python 目录缓存",
        ),
    );
    m.insert(
        "log.pbs_metadata_failed",
        (
            "pbs metadata fetch failed: {err}",
            "PBS 元数据获取失败：{err}",
        ),
    );
    m.insert(
        "log.pbs_sha256sums_failed",
        (
            "pbs SHA256SUMS fetch failed: {err}",
            "PBS SHA256SUMS 获取失败：{err}",
        ),
    );

    // ---- doctor -----------------------------------------------------------
    m.insert("doctor.title", ("osdk doctor", "osdk 诊断"));
    m.insert("doctor.same_fs_ok", ("hardlinks OK", "硬链接可用"));
    m.insert(
        "doctor.same_fs_no",
        ("will fall back to copy", "将回退为复制"),
    );
    m.insert("doctor.on_path", ("on PATH", "在 PATH 中"));

    // ---- errors -----------------------------------------------------------
    m.insert(
        "err.unknown_backend",
        ("`{name}` is not a known backend", "`{name}` 不是已知的后端"),
    );
    m.insert(
        "err.not_installed",
        ("{tool}@{ver} is not installed", "{tool}@{ver} 尚未安装"),
    );
    m.insert(
        "err.no_usable_source",
        (
            "no usable source for `{tool}`: all {tried} candidate(s) failed or were unreachable",
            "`{tool}` 没有可用的源：全部 {tried} 个候选源均失败或不可达",
        ),
    );
    m.insert(
        "err.checksum_mismatch",
        (
            "checksum mismatch for {name}: expected {expected}, got {actual}",
            "{name} 校验和不匹配：期望 {expected}，实际 {actual}",
        ),
    );
    m.insert(
        "err.version_resolve",
        (
            "could not resolve version `{spec}` for `{tool}`",
            "无法为 `{tool}` 解析版本 `{spec}`",
        ),
    );
    m.insert(
        "err.unsupported_platform",
        (
            "unsupported platform: os={os}, arch={arch}",
            "不支持的平台：os={os}，arch={arch}",
        ),
    );
    m.insert(
        "err.confirmation_non_interactive",
        (
            "confirmation required in non-interactive mode for: {question}; rerun with --yes or OSDK_YES=true",
            "非交互模式需要确认：{question}；请使用 --yes 或 OSDK_YES=true 重新运行",
        ),
    );
    m.insert(
        "err.untrusted_config",
        (
            "project config contains trust-required fields and is not trusted: {path}; review it, then run `osdk --yes trust {path}` or configure OSDK_TRUSTED_CONFIG_PATHS",
            "项目配置包含需信任字段但尚未受信任：{path}；请审阅后运行 `osdk --yes trust {path}`，或配置 OSDK_TRUSTED_CONFIG_PATHS",
        ),
    );
    m.insert(
        "err.invalid_opt",
        (
            "invalid --opt `{val}` (expected key=value)",
            "无效的 --opt `{val}`（应为 key=value）",
        ),
    );
    m.insert(
        "err.invalid_tool_request",
        ("invalid tool request `{val}`", "无效的工具请求 `{val}`"),
    );
    m.insert(
        "err.specify_exact",
        (
            "specify an exact version to uninstall (got `{spec}`)",
            "请指定要卸载的确切版本（收到 `{spec}`）",
        ),
    );
    m.insert(
        "err.no_installed_match",
        (
            "no installed {tool} version matches `{spec}`",
            "没有匹配 `{spec}` 的已安装 {tool} 版本",
        ),
    );
    m.insert(
        "err.unknown_source",
        (
            "unknown source {id} for {tool} (see `osdk source list {tool}`)",
            "{tool} 的未知源 {id}（参见 `osdk source list {tool}`）",
        ),
    );

    // ---- top-level help ---------------------------------------------------
    m.insert(
        "help.about",
        (
            "One SDK manager: unified version, dependency, and cache management for many SDKs",
            "统一的 SDK 管理器：为多种 SDK 提供统一的版本、依赖与缓存管理",
        ),
    );
    m.insert(
        "help.long_about",
        (
            "osdk installs and switches between versions of many SDKs (node, npm, pnpm, yarn, \
             java, python, rust, go, deno, bun, and any github:owner/repo release) across Windows, \
             macOS, and Linux.\n\nHighlights: content-addressed dedup across versions, unified \
             downstream package caches, and automatic fastest-mirror selection with failover.",
            "osdk 可在 Windows、macOS 与 Linux 上安装并切换多种 SDK 的版本（node、npm、pnpm、\
             yarn、java、python、rust、go、deno、bun，以及任意 github:owner/repo 发布物）。\n\n特性：跨版本\
             内容寻址去重、统一的下游包缓存、自动选择最快镜像并支持故障转移。",
        ),
    );

    // global flags
    m.insert(
        "help.flag.verbose",
        (
            "Increase verbosity (repeatable)",
            "提高日志详细程度（可重复）",
        ),
    );
    m.insert(
        "help.flag.quiet",
        ("Suppress progress output", "抑制进度输出"),
    );
    m.insert(
        "help.flag.jobs",
        (
            "Max concurrent downloads/installs",
            "并发下载/安装的最大数量",
        ),
    );
    m.insert(
        "help.flag.yes",
        ("Assume yes for prompts", "对提示默认回答“是”"),
    );
    m.insert(
        "help.flag.source",
        (
            "Force use of a specific source id for this invocation",
            "本次调用强制使用指定的源 id",
        ),
    );
    m.insert(
        "help.flag.refresh_sources",
        (
            "Re-probe sources, ignoring cached speed results",
            "重新探测源，忽略缓存的测速结果",
        ),
    );
    m.insert(
        "help.flag.offline",
        (
            "Disable network access and use cached metadata/artifacts only",
            "禁用网络，仅使用缓存的元数据和制品",
        ),
    );
    m.insert(
        "help.flag.require_checksums",
        (
            "Reject artifacts that have no verifiable checksum",
            "拒绝没有可验证校验值的制品",
        ),
    );
    m.insert(
        "help.flag.attestations",
        (
            "GitHub artifact attestation policy: off|if-available|required",
            "GitHub 制品证明策略：off|if-available|required",
        ),
    );
    m.insert(
        "help.lock.about",
        (
            "Resolve exact project versions into osdk.lock",
            "将项目工具解析为精确版本并写入 osdk.lock",
        ),
    );
    m.insert(
        "help.outdated.about",
        (
            "Show requested tools whose latest resolution is not installed",
            "显示尚未安装最新匹配版本的工具",
        ),
    );
    m.insert(
        "help.upgrade.about",
        (
            "Install current remote resolutions and update osdk.lock",
            "安装当前远端解析结果并更新 osdk.lock",
        ),
    );
    m.insert(
        "help.exec.about",
        (
            "Run a command with one or more managed tools",
            "使用一个或多个托管工具运行命令",
        ),
    );
    m.insert(
        "help.completions.about",
        ("Generate shell completion code", "生成 Shell 补全脚本"),
    );
    m.insert(
        "help.deactivate.about",
        (
            "Remove shell integration and restore the original environment",
            "移除 Shell 集成并恢复原始环境",
        ),
    );
    m.insert(
        "help.alias.about",
        (
            "Manage user-defined version aliases",
            "管理用户自定义版本别名",
        ),
    );
    m.insert(
        "help.flag.lang",
        (
            "Output language (en|zh); overrides locale and OSDK_LANG",
            "输出语言（en|zh）；覆盖 locale 与 OSDK_LANG",
        ),
    );

    // ---- per-command help: about + long + examples -----------------------
    m.insert(
        "help.install.about",
        ("Install one or more tools", "安装一个或多个工具"),
    );
    m.insert(
        "help.install.long",
        (
            "Install one or more tools. With no arguments, installs the versions pinned by the \
             resolved config (osdk.toml / .tool-versions) for this directory.\n\nEXAMPLES:\n  \
             osdk install node@20\n  osdk install go@1.22 python@3.12\n  osdk install rust@stable \
             -o profile=minimal -o components=clippy,rustfmt\n  osdk install github:sharkdp/fd",
            "安装一个或多个工具。若不带参数，则安装当前目录解析配置（osdk.toml / .tool-versions）\
             中固定的版本。\n\n示例：\n  osdk install node@20\n  osdk install go@1.22 python@3.12\n  \
             osdk install rust@stable -o profile=minimal -o components=clippy,rustfmt\n  \
             osdk install github:sharkdp/fd",
        ),
    );
    m.insert(
        "help.install.arg.tools",
        (
            "Tools to install, e.g. `node@20`, `go@1.22`, `github:cli/cli@2.62.0`",
            "要安装的工具，例如 `node@20`、`go@1.22`、`github:cli/cli@2.62.0`",
        ),
    );
    m.insert(
        "help.opt",
        (
            "Backend-specific option as key=value (repeatable), e.g. `-o profile=minimal` (rust), \
             `-o distribution=zulu` (java)",
            "后端特定选项，形如 key=value（可重复），例如 `-o profile=minimal`（rust）、\
             `-o distribution=zulu`（java）",
        ),
    );
    m.insert(
        "help.list.about",
        ("List installed versions", "列出已安装的版本"),
    );
    m.insert(
        "help.list.arg.tool",
        ("Restrict to a single tool", "仅限单个工具"),
    );
    m.insert(
        "help.list_remote.about",
        (
            "List installable versions from the remote index",
            "从远程索引列出可安装的版本",
        ),
    );
    m.insert(
        "help.list_remote.arg.tool",
        (
            "Tool to query, e.g. `node` or `github:sharkdp/fd`",
            "要查询的工具，例如 `node` 或 `github:sharkdp/fd`",
        ),
    );
    m.insert(
        "help.list_remote.arg.filter",
        (
            "Only show versions matching this prefix (e.g. `20`)",
            "仅显示匹配该前缀的版本（例如 `20`）",
        ),
    );
    m.insert(
        "help.use.about",
        (
            "Install (if needed) and set the active version",
            "安装（如有需要）并设置生效版本",
        ),
    );
    m.insert(
        "help.use.long",
        (
            "Install the tool if needed, generate shims, and write a version pin. By default the \
             pin goes to the nearest project config (osdk.toml); use --global to pin in the user \
             config.\n\nEXAMPLES:\n  osdk use node@20            # pin in this project\n  osdk use \
             -g python@3.12          # global default\n  osdk use rust@stable -o profile=minimal",
            "如有需要则安装工具、生成 shim，并写入版本固定。默认写入最近的项目配置（osdk.toml）；\
             使用 --global 写入用户配置。\n\n示例：\n  osdk use node@20            # 固定在本项目\n  \
             osdk use -g python@3.12          # 全局默认\n  osdk use rust@stable -o profile=minimal",
        ),
    );
    m.insert(
        "help.use.arg.tool",
        (
            "Tool and version, e.g. `node@20`",
            "工具与版本，例如 `node@20`",
        ),
    );
    m.insert(
        "help.use.flag.global",
        (
            "Write the pin to the user global config instead of the project",
            "将固定写入用户全局配置而非项目",
        ),
    );
    m.insert(
        "help.uninstall.about",
        ("Uninstall a tool version", "卸载某个工具版本"),
    );
    m.insert(
        "help.uninstall.arg.tool",
        (
            "Tool and version, e.g. `node@20.11.1`",
            "工具与版本，例如 `node@20.11.1`",
        ),
    );
    m.insert(
        "help.current.about",
        (
            "Show the active version of each tool for the current directory",
            "显示当前目录下每个工具的生效版本",
        ),
    );
    m.insert(
        "help.where.about",
        (
            "Print the install directory of a tool version",
            "打印某个工具版本的安装目录",
        ),
    );
    m.insert(
        "help.reshim.about",
        ("Regenerate shim launchers", "重新生成 shim 启动器"),
    );
    m.insert(
        "help.activate.about",
        (
            "Print shell integration to eval",
            "打印用于 eval 的 shell 集成脚本",
        ),
    );
    m.insert(
        "help.activate.long",
        (
            "Print a shell snippet that activates osdk for the current shell. Add it to your \
             shell rc file.\n\nEXAMPLES:\n  eval \"$(osdk activate bash)\"   # ~/.bashrc\n  osdk \
             activate zsh >> ~/.zshrc\n  osdk activate fish | source",
            "打印一段用于当前 shell 的激活脚本，将其加入你的 shell 配置文件。\n\n示例：\n  eval \
             \"$(osdk activate bash)\"   # ~/.bashrc\n  osdk activate zsh >> ~/.zshrc\n  osdk \
             activate fish | source",
        ),
    );
    m.insert(
        "help.activate.arg.shell",
        (
            "Target shell: bash|zsh|fish|powershell",
            "目标 shell：bash|zsh|fish|powershell",
        ),
    );
    m.insert(
        "help.source.about",
        ("Manage download sources (mirrors)", "管理下载源（镜像）"),
    );
    m.insert(
        "help.source.long",
        (
            "Manage per-tool download sources. osdk ships an official source plus authoritative \
             mirrors, auto-selecting the fastest with failover; you can add custom sources or pin \
             one.\n\nEXAMPLES:\n  osdk source list node\n  osdk source test node\n  osdk source \
             pin node tuna\n  osdk source add node --id mycorp --download-url https://m/ --index-url https://m/index.json",
            "管理各工具的下载源。osdk 内置官方源与权威镜像，自动选择最快者并支持故障转移；你也可\
             添加自定义源或固定某个源。\n\n示例：\n  osdk source list node\n  osdk source test node\n  \
             osdk source pin node tuna\n  osdk source add node --id mycorp --download-url https://m/ --index-url https://m/index.json",
        ),
    );
    m.insert(
        "help.config.about",
        ("Inspect or edit configuration", "查看或编辑配置"),
    );
    m.insert(
        "help.trust.about",
        (
            "Trust a project's execution-affecting configuration",
            "信任会影响执行行为的项目配置",
        ),
    );
    m.insert(
        "help.trust.arg.path",
        (
            "Project config file or directory (default: nearest config)",
            "项目配置文件或目录（默认：最近的配置）",
        ),
    );
    m.insert(
        "help.trust.list.about",
        (
            "List content-bound trusted project configurations",
            "列出绑定配置内容的受信任项目配置",
        ),
    );
    m.insert(
        "help.untrust.about",
        (
            "Remove trust for a project configuration",
            "取消对项目配置的信任",
        ),
    );
    m.insert(
        "help.cache.about",
        (
            "Manage the shared caches (store + downstream)",
            "管理共享缓存（存储区 + 下游）",
        ),
    );
    m.insert(
        "help.prune.about",
        (
            "Garbage-collect unreferenced store objects",
            "回收未被引用的存储区对象",
        ),
    );
    m.insert(
        "help.prune.flag.dry_run",
        (
            "Show what would be freed without deleting",
            "仅显示将释放的内容而不删除",
        ),
    );
    m.insert(
        "help.doctor.about",
        (
            "Diagnostics: dirs, mirrors, same-fs, link mode",
            "诊断：目录、镜像、同文件系统、链接模式",
        ),
    );

    // source subcommands
    m.insert(
        "help.source.list.about",
        ("List sources for a tool", "列出某个工具的源"),
    );
    m.insert(
        "help.source.test.about",
        (
            "Probe sources and print the speed ranking",
            "探测源并打印测速排名",
        ),
    );
    m.insert(
        "help.source.add.about",
        ("Add a custom source", "添加自定义源"),
    );
    m.insert(
        "help.source.remove.about",
        ("Remove a custom source", "移除自定义源"),
    );
    m.insert(
        "help.source.pin.about",
        ("Pin a tool to a source id", "将工具固定到某个源 id"),
    );
    m.insert(
        "help.source.unpin.about",
        ("Remove a tool's source pin", "取消某个工具的源固定"),
    );

    // config subcommands
    m.insert(
        "help.config.path.about",
        ("Print config file paths", "打印配置文件路径"),
    );
    m.insert(
        "help.config.list.about",
        ("Print resolved settings", "打印解析后的设置"),
    );

    // cache subcommands
    m.insert(
        "help.cache.dir.about",
        ("Print shared cache directories", "打印共享缓存目录"),
    );
    m.insert(
        "help.cache.env.about",
        (
            "Print downstream package-cache redirections",
            "打印下游包缓存的重定向",
        ),
    );
    m.insert(
        "help.cache.clean.about",
        (
            "Remove downloaded archives (keep store + installs)",
            "删除下载的归档（保留存储区与已安装内容）",
        ),
    );

    m
}
