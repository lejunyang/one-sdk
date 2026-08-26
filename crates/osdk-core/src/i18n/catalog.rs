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
    m.insert(
        "msg.model_env_enabled",
        (
            "enabled global {provider} model environment{mode}",
            "已启用全局 {provider} 模型环境{mode}",
        ),
    );
    m.insert(
        "msg.model_env_disabled",
        (
            "disabled global {provider} model environment",
            "已停用全局 {provider} 模型环境",
        ),
    );
    m.insert(
        "msg.model_env_refresh",
        (
            "active osdk shells refresh on the next prompt",
            "已激活 osdk 的 shell 会在下一个提示符自动刷新",
        ),
    );
    m.insert("label.enabled", ("enabled", "已启用"));
    m.insert("label.disabled", ("disabled", "已停用"));
    m.insert("label.force", (" (force)", "（强制覆盖）"));
    m.insert("msg.registry_manager_header", ("{manager}:", "{manager}："));
    m.insert("label.registry_pass_through", ("pass-through", "透传"));
    m.insert("label.registry_healthy", ("healthy", "健康"));
    m.insert("label.registry_unavailable", ("unavailable", "不可用"));
    m.insert("label.registry_selected", ("selected", "已选择"));
    m.insert("label.registry_ok", ("ok", "正常"));
    m.insert(
        "msg.registry_no_healthy_candidate",
        ("no healthy candidate", "没有健康候选"),
    );
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
        "msg.node_migrate_skip_npm",
        (
            "skip npm itself (managed by the target Node installation)",
            "跳过 npm 自身（由目标 Node 安装管理）",
        ),
    );
    m.insert(
        "msg.node_migrate_skip_native",
        (
            "skip native or install-script package {package}",
            "跳过原生或含安装脚本的包 {package}",
        ),
    );
    m.insert(
        "msg.node_migrate_nothing",
        (
            "no global npm packages need migration",
            "没有需要迁移的全局 npm 包",
        ),
    );
    m.insert(
        "msg.node_migrate_plan",
        ("would install {package}", "将安装 {package}"),
    );
    m.insert(
        "msg.node_migrate_dry_run",
        (
            "dry-run only; rerun with --apply to migrate",
            "当前仅演练；使用 --apply 重新运行以执行迁移",
        ),
    );
    m.insert(
        "msg.node_migrate_applied",
        (
            "migrated {count} global package(s) to Node {version}",
            "已将 {count} 个全局包迁移到 Node {version}",
        ),
    );
    m.insert(
        "msg.rust_markers_repaired",
        (
            "Rust markers repaired: {created} created, {removed} removed",
            "Rust 标记已修复：创建 {created} 个，移除 {removed} 个",
        ),
    );
    m.insert(
        "msg.rust_override_imported",
        (
            "imported rustup override {toolchain} into {path}",
            "已将 rustup override {toolchain} 导入 {path}",
        ),
    );
    m.insert(
        "msg.rust_override_exported",
        (
            "exported Rust {toolchain} override for {path}",
            "已为 {path} 导出 Rust {toolchain} override",
        ),
    );
    m.insert(
        "msg.rust_toolchain_linked",
        (
            "linked Rust toolchain {name} -> {path}",
            "已链接 Rust 工具链 {name} -> {path}",
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
    m.insert(
        "label.npm_lock_graph",
        ("locked npm graph for {tool}", "{tool} 的 npm 锁定依赖图"),
    );

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
    m.insert(
        "log.github_public_fallback",
        (
            "GitHub API quota exhausted; using best-effort public release metadata (recent, public releases only)",
            "GitHub API 配额已耗尽；改用尽力而为的公开 Release 元数据（仅近期公开 Release）",
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
        "err.registry_command_not_started",
        (
            "no dependency registry is available for {manager}; command was not started",
            "{manager} 没有可用的依赖 Registry；命令未启动",
        ),
    );
    m.insert(
        "err.registry_command_not_started_details",
        (
            "no dependency registry is available for {manager}; command was not started: {details}",
            "{manager} 没有可用的依赖 Registry；命令未启动：{details}",
        ),
    );
    m.insert(
        "err.registry_test_unavailable",
        (
            "no dependency registry is available for {managers}",
            "以下包管理器没有可用的依赖 Registry：{managers}",
        ),
    );
    m.insert(
        "err.registry_preflight_runtime",
        (
            "creating registry preflight runtime: {error}",
            "创建 Registry 预检运行时失败：{error}",
        ),
    );
    m.insert(
        "err.registry_preflight",
        (
            "registry preflight failed: {error}",
            "Registry 预检失败：{error}",
        ),
    );
    m.insert(
        "err.registry_unavailable",
        (
            "no reachable package registry for `{executable}`",
            "`{executable}` 没有可达的包 Registry",
        ),
    );
    m.insert(
        "err.registry_unavailable_details",
        (
            "no reachable package registry for `{executable}` ({details})",
            "`{executable}` 没有可达的包 Registry（{details}）",
        ),
    );
    m.insert(
        "err.node_migrate_rolled_back",
        (
            "global package migration failed; restored the target Node package set",
            "全局包迁移失败；已恢复目标 Node 原有包集合",
        ),
    );
    m.insert(
        "err.node_migrate_rollback_failed",
        (
            "global package migration failed and rollback also failed",
            "全局包迁移失败，且回滚也失败",
        ),
    );
    m.insert(
        "err.python_not_found",
        (
            "no matching Python interpreter found",
            "未找到匹配的 Python 解释器",
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
    m.insert(
        "err.npm_installer_invalid",
        (
            "invalid npm installer `{installer}` (expected auto|aube|npm|pnpm)",
            "无效的 npm 安装器 `{installer}`（应为 auto|aube|npm|pnpm）",
        ),
    );
    m.insert(
        "err.npm_scope_invalid",
        (
            "invalid npm tool scope `{scope}` (expected project|global)",
            "无效的 npm 工具作用域 `{scope}`（应为 project|global）",
        ),
    );
    m.insert(
        "err.npm_installer_option_type",
        (
            "npm installer option must be a string (auto|aube|npm|pnpm)",
            "npm installer 选项必须是字符串（auto|aube|npm|pnpm）",
        ),
    );
    m.insert(
        "err.npm_project_file_not_regular",
        (
            "{path} must be a regular file and must not be a symlink",
            "{path} 必须是常规文件且不能是符号链接",
        ),
    );
    m.insert(
        "err.npm_manager_lock_owner_conflict",
        (
            "{manifest} declares package manager `{manager}` but {lock} belongs to `{owner}`",
            "{manifest} 声明的包管理器为 `{manager}`，但 {lock} 属于 `{owner}`",
        ),
    );
    m.insert(
        "err.npm_declared_installer_unsupported",
        (
            "{source} declares unsupported npm-tool installer `{installer}`; expected aube, npm, or pnpm",
            "{source} 声明了不受支持的 npm 工具安装器 `{installer}`；应为 aube、npm 或 pnpm",
        ),
    );
    m.insert(
        "err.npm_aube_lock_format_unsupported",
        (
            "installer `aube` cannot read unsupported lock format `{format}` at {path}; use installer `{owner}`",
            "安装器 `aube` 无法读取 {path} 中不受支持的锁文件格式 `{format}`；请使用安装器 `{owner}`",
        ),
    );
    m.insert(
        "err.npm_installer_lock_conflict",
        (
            "installer `{installer}` conflicts with {path} owned by `{owner}`",
            "安装器 `{installer}` 与属于 `{owner}` 的锁文件 {path} 冲突",
        ),
    );
    m.insert(
        "err.npm_project_lock_ambiguous",
        (
            "ambiguous npm project: multiple recognized lockfiles exist: {paths}",
            "npm 项目存在歧义：发现多个可识别的锁文件：{paths}",
        ),
    );
    m.insert(
        "err.npm_native_lock_parse",
        (
            "parsing npm lockfile {path}: {error}",
            "解析 npm 锁文件 {path} 失败：{error}",
        ),
    );
    m.insert(
        "err.npm_native_lock_version_numeric_required",
        (
            "{path} is missing a numeric lockfileVersion",
            "{path} 缺少数值类型的 lockfileVersion",
        ),
    );
    m.insert(
        "err.npm_native_lock_version_missing",
        (
            "{path} is missing lockfileVersion",
            "{path} 缺少 lockfileVersion",
        ),
    );
    m.insert(
        "err.npm_native_lock_version_malformed",
        (
            "{path} has malformed lockfileVersion",
            "{path} 的 lockfileVersion 格式错误",
        ),
    );
    m.insert(
        "err.npm_aube_lock_version_unsupported",
        (
            "{path} uses unsupported aube lockfile version {version}; expected v9",
            "{path} 使用了不受支持的 aube 锁文件版本 {version}；应为 v9",
        ),
    );
    m.insert(
        "err.package_manager_manifest_read",
        (
            "reading package-manager manifest {path}: {error}",
            "读取包管理器清单 {path} 失败：{error}",
        ),
    );
    m.insert(
        "err.package_manager_manifest_parse",
        (
            "parsing package-manager manifest {path}: {error}",
            "解析包管理器清单 {path} 失败：{error}",
        ),
    );
    m.insert(
        "err.package_manager_field_type",
        (
            "{path} packageManager must be a string",
            "{path} 中的 packageManager 必须是字符串",
        ),
    );
    m.insert(
        "err.package_manager_dev_engines_empty",
        (
            "{path} devEngines.packageManager must not be an empty array",
            "{path} 中的 devEngines.packageManager 不能为空数组",
        ),
    );
    m.insert(
        "err.package_manager_name_missing",
        (
            "{path} devEngines.packageManager is missing name",
            "{path} 中的 devEngines.packageManager 缺少 name",
        ),
    );
    m.insert(
        "err.package_manager_version_missing",
        (
            "{path} devEngines.packageManager is missing version",
            "{path} 中的 devEngines.packageManager 缺少 version",
        ),
    );
    m.insert(
        "err.package_manager_declaration_invalid",
        (
            "{path} packageManager must be `<manager>@<exact-version>`",
            "{path} 中的 packageManager 必须为 `<manager>@<exact-version>`",
        ),
    );
    m.insert(
        "err.package_manager_unsupported",
        (
            "{path} has unsupported package manager `{manager}`",
            "{path} 使用了不受支持的包管理器 `{manager}`",
        ),
    );
    m.insert(
        "err.package_manager_name_invalid",
        (
            "{path} has invalid package manager name `{manager}`",
            "{path} 中的包管理器名称 `{manager}` 无效",
        ),
    );
    m.insert(
        "err.package_manager_version_not_exact",
        (
            "{path} package manager `{manager}` requires an exact semver without URL/hash suffix: `{version}`",
            "{path} 中的包管理器 `{manager}` 必须使用不含 URL/hash 后缀的精确 semver：`{version}`",
        ),
    );
    m.insert(
        "err.npm_managed_node_dependency_required",
        (
            "npm tools require a managed Node dependency",
            "npm 工具需要受管的 Node 依赖",
        ),
    );
    m.insert(
        "err.npm_package_backend_invalid",
        (
            "invalid npm package backend `{package}`",
            "无效的 npm 包后端 `{package}`",
        ),
    );
    m.insert(
        "err.shim_generation_conflict",
        (
            "refusing to generate managed shim `{name}` because it is provided by multiple installed tools: {owners}",
            "拒绝生成受管 shim `{name}`，因为多个已安装工具均提供该命令：{owners}",
        ),
    );
    m.insert(
        "err.shim_managed_node_required",
        (
            "`{tool}` requires a managed Node installation",
            "`{tool}` 需要受管的 Node 安装",
        ),
    );
    m.insert(
        "err.shim_dynamic_route_conflict",
        (
            "refusing to route `{tool}` because multiple installed tools provide it: {owners}",
            "拒绝路由 `{tool}`，因为多个已安装工具均提供该命令：{owners}",
        ),
    );
    m.insert(
        "err.npm_allow_builds_invalid",
        (
            "allow_builds must be a boolean or a non-empty package list",
            "allow_builds 必须是布尔值或非空包列表",
        ),
    );
    m.insert(
        "err.npm_global_aube_offline_unsupported",
        (
            "global Aube installs are unavailable in offline mode because Aube 2.1 does not support offline global add",
            "Aube 2.1 不支持全局离线添加，因此离线模式下无法执行 Aube 全局安装",
        ),
    );
    m.insert(
        "err.npm_global_registry_isolation",
        (
            "cannot safely isolate global {manager} install: registry preflight passed through ({reason})",
            "无法安全隔离全局 {manager} 安装：Registry 预检要求透传（{reason}）",
        ),
    );
    m.insert(
        "err.npm_lock_graph_option_missing",
        (
            "locked npm graph is missing private option `{key}`",
            "npm 锁定依赖图缺少内部选项 `{key}`",
        ),
    );
    m.insert(
        "err.npm_lock_graph_identity_mismatch",
        (
            "locked npm graph identity mismatch: expected {expected_tool} for package {expected_package}, got {actual_tool} for package {actual_package}",
            "npm 锁定依赖图身份不匹配：期望工具 {expected_tool} 对应包 {expected_package}，实际为工具 {actual_tool} 对应包 {actual_package}",
        ),
    );
    m.insert(
        "err.npm_lock_graph_format_unsupported",
        (
            "unsupported locked npm graph format `{format}` for {tool}; expected {expected}",
            "{tool} 使用了不支持的 npm 锁定依赖图格式 `{format}`；期望 {expected}",
        ),
    );
    m.insert(
        "err.npm_lock_graph_digest_invalid",
        (
            "locked npm graph for {tool} has an invalid SHA-256 digest",
            "{tool} 的 npm 锁定依赖图包含无效的 SHA-256 摘要",
        ),
    );
    m.insert(
        "err.npm_lock_graph_tool_mismatch",
        (
            "cannot prepare npm lock graph for {expected} using resolved tool {actual}",
            "无法使用已解析工具 {actual} 为 {expected} 准备 npm 锁定依赖图",
        ),
    );
    m.insert(
        "err.npm_lock_graph_not_produced",
        (
            "aube did not produce a lock graph for {package}@{version}",
            "aube 未能为 {package}@{version} 生成锁定依赖图",
        ),
    );
    m.insert(
        "err.npm_install_package_missing",
        (
            "embedded npm install did not materialize {package} under {path}",
            "嵌入式 npm 安装未在 {path} 下生成 {package}",
        ),
    );
    m.insert(
        "err.npm_install_bin_dir_missing",
        (
            "embedded npm install did not produce node_modules/.bin for {package}",
            "嵌入式 npm 安装未为 {package} 生成 node_modules/.bin",
        ),
    );
    m.insert(
        "err.npm_offline_lock_graph_required",
        (
            "cannot install {tool}@{version} offline without a locked npm dependency graph; run `osdk lock` online first",
            "没有 npm 锁定依赖图，无法离线安装 {tool}@{version}；请先联网运行 `osdk lock`",
        ),
    );
    m.insert(
        "err.npm_package_sri_missing",
        (
            "npm package {package}@{version} has no supported SRI checksum",
            "npm 包 {package}@{version} 没有受支持的 SRI 校验和",
        ),
    );
    m.insert(
        "err.npm_dynamic_no_validated_executables",
        (
            "dynamic npm tool {tool} exposes no validated executables",
            "动态 npm 工具 {tool} 未提供任何已验证的可执行文件",
        ),
    );
    m.insert(
        "err.npm_dynamic_managed_node_required",
        (
            "dynamic npm tools require a managed Node installation",
            "动态 npm 工具需要受管的 Node 安装",
        ),
    );
    m.insert(
        "err.managed_node_bin_dir_missing",
        (
            "managed Node {version} has no executable bin directory",
            "受管 Node {version} 没有可执行文件目录",
        ),
    );
    m.insert(
        "err.npm_bin_outside_install_root",
        (
            "bin `{name}` resolves outside install root {path}",
            "可执行文件 `{name}` 解析到了安装根目录 {path} 之外",
        ),
    );
    m.insert(
        "err.npm_bins_not_discovered",
        (
            "no executable bins discovered under {path}",
            "未在 {path} 下发现可执行文件",
        ),
    );
    m.insert(
        "err.npm_bin_target_unresolved",
        (
            "unable to resolve executable target for `{name}` in {path}",
            "无法解析 {path} 中 `{name}` 的可执行文件目标",
        ),
    );
    m.insert(
        "err.npm_lock_payload_read",
        (
            "reading npm lock payload {path}",
            "读取 npm 锁定载荷 {path}",
        ),
    );
    m.insert(
        "err.npm_lock_payload_not_utf8",
        (
            "npm lock payload {path} is not UTF-8",
            "npm 锁定载荷 {path} 不是 UTF-8 编码",
        ),
    );
    m.insert(
        "err.npm_project_manifest_identity_mismatch",
        (
            "npm project manifest does not pin {package}@{version}",
            "npm 项目清单未固定 {package}@{version}",
        ),
    );
    m.insert(
        "err.npm_project_manifest_build_policy_mismatch",
        (
            "npm project manifest build policy mismatch for {package}",
            "npm 项目清单中 {package} 的构建策略不匹配",
        ),
    );
    m.insert(
        "err.npm_graph_parse_invalid",
        (
            "invalid npm dependency graph at {path}: {error}",
            "{path} 中的 npm 依赖图无效：{error}",
        ),
    );
    m.insert(
        "err.npm_graph_root_missing",
        (
            "npm dependency graph is missing requested root package {package}",
            "npm 依赖图缺少请求的根包 {package}",
        ),
    );
    m.insert(
        "err.npm_graph_root_version_mismatch",
        (
            "npm dependency graph root version mismatch for {package}: expected {expected}, got {actual}",
            "npm 依赖图中根包 {package} 的版本不匹配：期望 {expected}，实际为 {actual}",
        ),
    );
    m.insert(
        "err.npm_graph_resolved_root_missing",
        (
            "npm dependency graph is missing resolved root package {package}@{version}",
            "npm 依赖图缺少已解析的根包 {package}@{version}",
        ),
    );
    m.insert(
        "err.npm_graph_root_integrity_missing",
        (
            "npm dependency graph root package {package}@{version} has no integrity value",
            "npm 依赖图中的根包 {package}@{version} 缺少完整性校验值",
        ),
    );
    m.insert(
        "err.npm_graph_root_integrity_invalid",
        (
            "npm dependency graph root package {package}@{version} has invalid integrity",
            "npm 依赖图中根包 {package}@{version} 的完整性校验值无效",
        ),
    );
    m.insert(
        "err.npm_graph_root_source_mismatch",
        (
            "npm dependency graph root package {package}@{version} uses an unexpected source",
            "npm 依赖图中的根包 {package}@{version} 使用了非预期来源",
        ),
    );
    m.insert(
        "err.lock_npm_legacy_inline_regenerate",
        (
            "npm entry `{backend}` uses a legacy inline graph and must be regenerated",
            "npm 条目 `{backend}` 使用旧版内联依赖图，必须重新生成",
        ),
    );
    m.insert(
        "err.lockfile_not_utf8",
        (
            "lockfile {path} is not UTF-8",
            "锁文件 {path} 不是 UTF-8 编码",
        ),
    );
    m.insert(
        "err.lock_npm_installer_unsupported",
        (
            "unsupported npm installer `{installer}` in lock metadata",
            "锁元数据中的 npm 安装器 `{installer}` 不受支持",
        ),
    );
    m.insert(
        "err.lock_schema3_npm_artifact_forbidden",
        (
            "schema 3 npm entry `{backend}` for platform `{platform}` cannot carry a generic artifact receipt",
            "平台 `{platform}` 的 schema 3 npm 条目 `{backend}` 不能包含通用制品收据",
        ),
    );
    m.insert(
        "err.lock_schema3_npm_private_option_forbidden",
        (
            "schema 3 npm entry `{backend}` for platform `{platform}` cannot carry private option `{key}`",
            "平台 `{platform}` 的 schema 3 npm 条目 `{backend}` 不能包含内部选项 `{key}`",
        ),
    );
    m.insert(
        "err.lock_schema3_npm_metadata_missing",
        (
            "schema 3 npm entry `{backend}` on `{platform}` is missing npm metadata",
            "平台 `{platform}` 上的 schema 3 npm 条目 `{backend}` 缺少 npm 元数据",
        ),
    );
    m.insert(
        "err.lock_schema3_npm_legacy_metadata",
        (
            "schema 3 npm entry `{backend}` on `{platform}` uses legacy graph metadata",
            "平台 `{platform}` 上的 schema 3 npm 条目 `{backend}` 使用了旧版依赖图元数据",
        ),
    );
    m.insert(
        "err.lock_schema2_npm_schema3_metadata",
        (
            "schema 2 npm entry `{backend}` on `{platform}` uses schema 3 metadata",
            "平台 `{platform}` 上的 schema 2 npm 条目 `{backend}` 使用了 schema 3 元数据",
        ),
    );
    m.insert(
        "err.lock_npm_native_format_unsupported",
        (
            "unsupported native npm lock format `{format}` for `{backend}` and owner `{owner}`",
            "`{backend}` 的原生 npm 锁文件格式 `{format}` 不受支持，其所有者为 `{owner}`",
        ),
    );
    m.insert(
        "err.lock_npm_node_version_not_exact",
        (
            "npm entry `{backend}` has non-exact node version `{version}`; expected a complete semantic version",
            "npm 条目 `{backend}` 的 Node 版本 `{version}` 不精确；应为完整的语义版本",
        ),
    );
    m.insert(
        "err.lock_npm_native_metadata_missing",
        (
            "native npm lock metadata is missing `{key}`",
            "原生 npm 锁元数据缺少 `{key}`",
        ),
    );
    m.insert(
        "err.lock_schema2_npm_artifact_forbidden",
        (
            "schema 2 npm entry `{backend}` for platform `{platform}` cannot carry a generic artifact receipt",
            "平台 `{platform}` 的 schema 2 npm 条目 `{backend}` 不能包含通用制品收据",
        ),
    );
    m.insert(
        "err.lock_npm_package_mismatch",
        (
            "npm graph package mismatch for `{backend}` on `{platform}`: expected `{expected}`, got `{actual}`",
            "平台 `{platform}` 上 `{backend}` 的 npm 依赖图包不匹配：期望 `{expected}`，实际为 `{actual}`",
        ),
    );
    m.insert(
        "err.lock_npm_graph_format_unsupported",
        (
            "unsupported npm graph format `{format}` for `{backend}` on `{platform}`; expected `{expected}`",
            "平台 `{platform}` 上 `{backend}` 的 npm 依赖图格式 `{format}` 不受支持；期望 `{expected}`",
        ),
    );
    m.insert(
        "err.lock_npm_graph_path_unsafe_platform",
        (
            "unsafe npm graph path `{graph}` for `{backend}` on `{platform}`; expected `{expected}`",
            "平台 `{platform}` 上 `{backend}` 的 npm 依赖图路径 `{graph}` 不安全；期望 `{expected}`",
        ),
    );
    m.insert(
        "err.lock_npm_node_entry_required",
        (
            "npm entry `{backend}` on `{platform}` requires a locked `node` entry",
            "平台 `{platform}` 上的 npm 条目 `{backend}` 需要锁定的 `node` 条目",
        ),
    );
    m.insert(
        "err.lock_npm_node_version_mismatch",
        (
            "npm graph node version mismatch for `{backend}` on `{platform}`: expected `{expected}`, got `{actual}`",
            "平台 `{platform}` 上 `{backend}` 的 npm 依赖图 Node 版本不匹配：期望 `{expected}`，实际为 `{actual}`",
        ),
    );
    m.insert(
        "err.lock_npm_graph_missing",
        (
            "npm entry `{backend}` on `{platform}` is missing its dependency graph",
            "平台 `{platform}` 上的 npm 条目 `{backend}` 缺少依赖图",
        ),
    );
    m.insert(
        "err.lock_schema2_npm_legacy_inline",
        (
            "schema 2 npm entry `{backend}` on `{platform}` uses the legacy inline graph representation",
            "平台 `{platform}` 上的 schema 2 npm 条目 `{backend}` 使用了旧版内联依赖图表示",
        ),
    );
    m.insert(
        "err.lock_non_npm_graph_metadata",
        (
            "non-npm entry `{backend}` on `{platform}` cannot carry npm graph metadata",
            "平台 `{platform}` 上的非 npm 条目 `{backend}` 不能包含 npm 依赖图元数据",
        ),
    );
    m.insert(
        "err.lock_npm_graph_sha256_invalid",
        (
            "npm graph for `{backend}` has invalid SHA-256 `{sha256}`; expected exactly 64 lowercase hexadecimal characters",
            "`{backend}` 的 npm 依赖图包含无效的 SHA-256 `{sha256}`；应为恰好 64 个小写十六进制字符",
        ),
    );
    m.insert(
        "err.lock_npm_graph_path_unsafe",
        (
            "unsafe npm graph path `{graph}` for `{backend}`; expected `{expected}`",
            "`{backend}` 的 npm 依赖图路径 `{graph}` 不安全；期望 `{expected}`",
        ),
    );
    m.insert(
        "err.lock_npm_sidecar_read",
        (
            "reading npm graph sidecar {path}",
            "读取 npm 依赖图 sidecar {path}",
        ),
    );
    m.insert(
        "err.lock_npm_sidecar_checksum_mismatch",
        (
            "npm graph sidecar checksum mismatch for `{backend}`: expected {expected}, got {actual}",
            "`{backend}` 的 npm 依赖图 sidecar 校验和不匹配：期望 {expected}，实际为 {actual}",
        ),
    );
    m.insert(
        "err.lock_npm_sidecar_not_utf8",
        (
            "npm graph sidecar {path} is not UTF-8",
            "npm 依赖图 sidecar {path} 不是 UTF-8 编码",
        ),
    );
    m.insert(
        "err.lock_npm_graph_path_symlink",
        (
            "npm graph path contains symlink {path}",
            "npm 依赖图路径包含符号链接 {path}",
        ),
    );
    m.insert(
        "err.lock_npm_sidecar_symlink",
        (
            "npm graph sidecar cannot be a symlink: {path}",
            "npm 依赖图 sidecar 不能是符号链接：{path}",
        ),
    );
    m.insert(
        "err.file_size_limit_exceeded",
        (
            "file exceeds maximum size of {maximum} bytes",
            "文件超过 {maximum} 字节的大小上限",
        ),
    );
    m.insert(
        "err.lock_backend_id_unsafe",
        (
            "unsafe backend id `{backend}` in lockfile",
            "锁文件中的后端 id `{backend}` 不安全",
        ),
    );
    m.insert(
        "err.lock_version_unsafe",
        (
            "unsafe version `{version}` for `{backend}` in lockfile",
            "锁文件中 `{backend}` 的版本 `{version}` 不安全",
        ),
    );
    m.insert(
        "err.lock_artifact_filename_unsafe",
        (
            "unsafe artifact file name `{file_name}` for `{backend}` in lockfile",
            "锁文件中 `{backend}` 的制品文件名 `{file_name}` 不安全",
        ),
    );
    m.insert(
        "err.lock_artifact_subdir_unsafe",
        (
            "unsafe artifact subdirectory `{subdir}` for `{backend}` in lockfile",
            "锁文件中 `{backend}` 的制品子目录 `{subdir}` 不安全",
        ),
    );
    m.insert(
        "err.lock_schema1_npm_migration_requires_graph",
        (
            "schema 1 npm entry `{backend}` on `{platform}` cannot be migrated without a dependency graph; regenerate the lock",
            "平台 `{platform}` 上的 schema 1 npm 条目 `{backend}` 缺少依赖图，无法迁移；请重新生成锁文件",
        ),
    );
    m.insert(
        "err.lock_npm_resolved_node_required",
        (
            "cannot lock `{backend}` without a resolved `node` entry",
            "缺少已解析的 `node` 条目，无法锁定 `{backend}`",
        ),
    );
    m.insert(
        "err.lockfile_size_limit_exceeded",
        (
            "lockfile exceeds maximum size of {maximum} bytes",
            "锁文件超过 {maximum} 字节的大小上限",
        ),
    );
    m.insert(
        "err.lock_atomic_path_filename_missing",
        ("path has no file name: {path}", "路径缺少文件名：{path}"),
    );
    m.insert(
        "err.fs_directory_create",
        ("creating {path}", "创建目录 {path}"),
    );
    m.insert("err.fs_file_create", ("creating {path}", "创建文件 {path}"));
    m.insert("err.fs_file_write", ("writing {path}", "写入文件 {path}"));
    m.insert("err.fs_file_sync", ("syncing {path}", "同步文件 {path}"));
    m.insert(
        "err.fs_file_replace",
        ("replacing {path}", "替换文件 {path}"),
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
             downstream package caches, automatic fastest-mirror selection with failover, and \
             immutable Hugging Face / ModelScope model snapshots.",
            "osdk 可在 Windows、macOS 与 Linux 上安装并切换多种 SDK 的版本（node、npm、pnpm、\
             yarn、java、python、rust、go、deno、bun，以及任意 github:owner/repo 发布物）。\n\n特性：跨版本\
             内容寻址去重、统一的下游包缓存、自动选择最快镜像并支持故障转移，以及不可变的 Hugging Face / \
             ModelScope 模型快照。",
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
        "help.flag.prerelease",
        (
            "Pre-release policy: never|if-explicit|allow",
            "预发布版本策略：never|if-explicit|allow",
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
            "Install a tool if needed and make it active. For ordinary SDKs, the default writes a \
             project pin; --global writes the user default. For `npm:<package>` inside a Node \
             project, the default modifies package.json and its native lockfile, then records the npm \
             selection in project osdk.toml and exact Node/npm metadata in osdk.lock. `--global` ignores the current \
             project, installs into an osdk-managed prefix, and updates the user config and lock. \
             Installer auto-selection prefers embedded Aube when the project lock is compatible; npm \
             package build scripts are disabled by default.\n\nEXAMPLES:\n  osdk use node@20\n  osdk use \
             npm:prettier@3 -o installer=auto\n  osdk use -g npm:prettier@3 -o allow_builds=false",
            "如有需要则安装工具并使其生效。对于普通 SDK，默认写入项目版本固定；--global 写入用户默认值。\
             对 Node 项目中的 `npm:<package>`，默认会修改 package.json 及其原生锁文件，然后在项目 \
             osdk.toml 中记录 npm 选择，并在 osdk.lock 中记录精确的 Node/npm 元数据。`--global` 会忽略当前项目，安装到 \
             osdk 管理的隔离前缀，并更新用户配置和锁文件。安装器自动选择会在项目锁格式兼容时优先使用\
             内嵌 Aube；npm 包构建脚本默认禁用。\n\n示例：\n  \
             osdk use node@20\n  osdk use npm:prettier@3 -o installer=auto\n  osdk use -g \
             npm:prettier@3 -o allow_builds=false",
        ),
    );
    m.insert(
        "help.use.arg.tool",
        (
            "Tool and version, e.g. `node@20` or `npm:prettier@3`",
            "工具与版本，例如 `node@20` 或 `npm:prettier@3`",
        ),
    );
    m.insert(
        "help.use.flag.global",
        (
            "Use global scope; npm packages ignore the current project and use an osdk-managed prefix and user lock",
            "使用全局作用域；npm 包会忽略当前项目，使用 osdk 管理的隔离前缀和用户锁文件",
        ),
    );
    m.insert(
        "help.use.flag.opt",
        (
            "Backend option as key=value (repeatable); npm packages support `installer=auto|aube|npm|pnpm` (auto prefers compatible Aube) and `allow_builds=false|true|package,...` for non-project installs (npm accepts booleans only; project installs always disable scripts)",
            "后端选项，形如 key=value（可重复）；npm 包支持 `installer=auto|aube|npm|pnpm`（auto 优先使用兼容的 Aube），非项目安装支持 `allow_builds=false|true|包名,...`（npm 仅接受布尔值；项目安装始终禁用脚本）",
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
        "help.uninstall.flag.global",
        (
            "Remove a user-global npm package installation, configuration, lock entry, and shims",
            "删除用户级全局 npm 包安装及其配置、锁条目和 shim",
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
        "help.where.flag.global",
        (
            "Resolve an npm package from global scope, ignoring project selection",
            "从全局作用域定位 npm 包，忽略项目选择",
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
            "Manage SDK and model-provider download sources. osdk auto-selects fast endpoints with \
             failover; model probes use a real target repository and bounded file download.\n\nEXAMPLES:\n  \
             osdk source list node\n  osdk source test node\n  osdk source test modelscope --model \
             Qwen/Qwen2.5-0.5B-Instruct@master\n  osdk source pin modelscope modelscope-cn",
            "管理 SDK 与模型 Provider 下载源。osdk 自动选择快速 endpoint 并支持故障转移；模型探测会使用\
             真实目标仓库与有界文件下载。\n\n示例：\n  osdk source list node\n  osdk source test node\n  \
             osdk source test modelscope --model Qwen/Qwen2.5-0.5B-Instruct@master\n  \
             osdk source pin modelscope modelscope-cn",
        ),
    );
    m.insert(
        "help.registry.about",
        (
            "Inspect project dependency registry selection",
            "查看项目依赖 Registry 的选择",
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
        "help.node.about",
        ("Manage Node-specific workflows", "管理 Node 专属工作流"),
    );
    m.insert(
        "help.node.migrate.about",
        (
            "Plan or apply global npm package migration",
            "规划或执行全局 npm 包迁移",
        ),
    );
    m.insert(
        "help.node.migrate.arg.from",
        ("Source managed Node version", "来源受管 Node 版本"),
    );
    m.insert(
        "help.node.migrate.arg.to",
        ("Target managed Node version", "目标受管 Node 版本"),
    );
    m.insert(
        "help.node.migrate.flag.apply",
        (
            "Apply the migration instead of printing a plan",
            "执行迁移，而不是只打印计划",
        ),
    );
    m.insert(
        "help.python.about",
        ("Manage Python-specific workflows", "管理 Python 专属工作流"),
    );
    m.insert(
        "help.model.about",
        (
            "Manage immutable local large-model snapshots",
            "管理不可变的本地大模型快照",
        ),
    );
    m.insert(
        "help.model.pull.about",
        (
            "Resolve and download an immutable model snapshot",
            "解析并下载不可变模型快照",
        ),
    );
    m.insert(
        "help.model.pull.arg.name",
        ("Local logical model name", "本地逻辑模型名称"),
    );
    m.insert(
        "help.model.pull.arg.reference",
        (
            "Provider reference such as hf:Qwen/Qwen2.5-7B-Instruct@main",
            "模型源引用，例如 hf:Qwen/Qwen2.5-7B-Instruct@main",
        ),
    );
    m.insert(
        "help.model.pull.flag.endpoint",
        ("Override the provider endpoint", "覆盖模型源 endpoint"),
    );
    m.insert(
        "help.model.pull.flag.forward_credentials",
        (
            "Allow an explicit custom endpoint to receive provider credentials",
            "允许显式自定义 endpoint 接收模型源凭据",
        ),
    );
    m.insert(
        "help.model.pull.flag.include",
        (
            "Include files matching a glob (repeatable)",
            "包含匹配 glob 的文件（可重复）",
        ),
    );
    m.insert(
        "help.model.pull.flag.exclude",
        (
            "Exclude files matching a glob (repeatable)",
            "排除匹配 glob 的文件（可重复）",
        ),
    );
    m.insert(
        "help.model.pull.flag.variant",
        (
            "Optional format or quantization label",
            "可选的格式或量化变体标签",
        ),
    );
    m.insert(
        "help.model.pull.flag.no_lock",
        (
            "Do not update the nearest project osdk.lock",
            "不更新最近的项目 osdk.lock",
        ),
    );
    m.insert(
        "help.model.list.about",
        ("List local model snapshots", "列出本地模型快照"),
    );
    m.insert(
        "help.model.path.about",
        ("Print a model snapshot path", "打印模型快照路径"),
    );
    m.insert(
        "help.model.verify.about",
        (
            "Verify every file in a model snapshot",
            "验证模型快照中的每个文件",
        ),
    );
    m.insert(
        "help.model.remove.about",
        (
            "Remove local snapshots for a model",
            "移除某个模型的本地快照",
        ),
    );
    m.insert(
        "help.model.env.about",
        (
            "Manage provider environment exported by shell activation",
            "管理由 shell activation 导出的模型 Provider 环境",
        ),
    );
    m.insert(
        "help.model.env.enable.about",
        (
            "Persistently export provider endpoint and cache variables",
            "持久导出模型 Provider endpoint 与缓存变量",
        ),
    );
    m.insert(
        "help.model.env.enable.arg.provider",
        (
            "Provider to enable; omit to enable both",
            "要启用的 Provider；省略则同时启用两个",
        ),
    );
    m.insert(
        "help.model.env.enable.flag.force",
        (
            "Override provider variables already set by the user",
            "覆盖用户已设置的模型 Provider 变量",
        ),
    );
    m.insert(
        "help.model.env.disable.about",
        (
            "Stop exporting provider variables and restore original values",
            "停止导出模型 Provider 变量并恢复原值",
        ),
    );
    m.insert(
        "help.model.env.list.about",
        (
            "Show persisted adapter state and effective exports",
            "显示持久 adapter 状态与生效导出变量",
        ),
    );
    m.insert(
        "help.python.find.about",
        (
            "Find managed, PATH, and system Python interpreters",
            "发现受管、PATH 和系统 Python 解释器",
        ),
    );
    m.insert(
        "help.python.find.arg.request",
        (
            "Optional request such as pypy-3.11 or 3.14+freethreaded",
            "可选请求，例如 pypy-3.11 或 3.14+freethreaded",
        ),
    );
    m.insert(
        "help.rust.about",
        (
            "Manage isolated Rust components, targets, overrides, and linked toolchains",
            "管理隔离的 Rust 组件、目标、override 和链接工具链",
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
            "Probe sources and print the speed ranking; model providers require --model",
            "探测源并打印测速排名；模型源需要 --model",
        ),
    );
    m.insert(
        "help.source.test.flag.model",
        (
            "Target model repository for huggingface/modelscope probes",
            "huggingface/modelscope 探测使用的目标模型仓库",
        ),
    );
    m.insert(
        "help.source.add.about",
        ("Add a custom source", "添加自定义源"),
    );
    m.insert(
        "help.source.add.flag.forward_credentials",
        (
            "Allow this custom endpoint to receive provider credentials",
            "允许该自定义 endpoint 接收模型源凭据",
        ),
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

    // dependency-registry subcommands
    m.insert(
        "help.registry.test.about",
        (
            "Probe dependency registries and show the selection plan",
            "探测项目依赖 Registry 并显示选择方案",
        ),
    );
    m.insert(
        "help.registry.test.arg.manager",
        (
            "Package manager to test; omit to test all supported managers",
            "要测试的包管理器；省略则测试所有支持的包管理器",
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    const NPM_SCOPE_AND_SCHEMA3_KEYS: &[&str] = &[
        "err.npm_installer_invalid",
        "err.npm_scope_invalid",
        "err.npm_installer_option_type",
        "err.npm_project_file_not_regular",
        "err.npm_manager_lock_owner_conflict",
        "err.npm_declared_installer_unsupported",
        "err.npm_aube_lock_format_unsupported",
        "err.npm_installer_lock_conflict",
        "err.npm_project_lock_ambiguous",
        "err.npm_native_lock_parse",
        "err.npm_native_lock_version_numeric_required",
        "err.npm_native_lock_version_missing",
        "err.npm_native_lock_version_malformed",
        "err.npm_aube_lock_version_unsupported",
        "err.package_manager_manifest_read",
        "err.package_manager_manifest_parse",
        "err.package_manager_field_type",
        "err.package_manager_dev_engines_empty",
        "err.package_manager_name_missing",
        "err.package_manager_version_missing",
        "err.package_manager_declaration_invalid",
        "err.package_manager_unsupported",
        "err.package_manager_name_invalid",
        "err.package_manager_version_not_exact",
        "err.lock_npm_installer_unsupported",
        "err.lock_schema3_npm_artifact_forbidden",
        "err.lock_schema3_npm_private_option_forbidden",
        "err.lock_schema3_npm_metadata_missing",
        "err.lock_schema3_npm_legacy_metadata",
        "err.lock_schema2_npm_schema3_metadata",
        "err.lock_npm_native_format_unsupported",
        "err.lock_npm_node_version_not_exact",
        "err.lock_npm_native_metadata_missing",
        "help.use.long",
        "help.use.arg.tool",
        "help.use.flag.global",
        "help.use.flag.opt",
        "help.uninstall.flag.global",
        "help.where.flag.global",
    ];

    #[test]
    fn npm_scope_and_schema3_messages_are_bilingual_with_matching_placeholders() {
        let catalog = build();
        for key in NPM_SCOPE_AND_SCHEMA3_KEYS {
            let &(english, chinese) = catalog.get(key).unwrap_or_else(|| panic!("missing {key}"));
            assert!(!english.is_empty(), "missing English for {key}");
            assert!(!chinese.is_empty(), "missing Chinese for {key}");
            assert_ne!(english, chinese, "Chinese is not translated for {key}");
            assert_eq!(
                placeholders(english),
                placeholders(chinese),
                "placeholder mismatch for {key}"
            );
        }
    }

    #[test]
    fn npm_scope_errors_and_use_help_have_chinese_regressions() {
        let catalog = build();
        let conflict = crate::i18n::interpolate(
            catalog["err.npm_manager_lock_owner_conflict"].1,
            &[
                ("manifest", "/repo/package.json"),
                ("manager", "npm"),
                ("lock", "/repo/pnpm-lock.yaml"),
                ("owner", "pnpm"),
            ],
        );
        assert!(conflict.contains("声明的包管理器"));
        assert!(conflict.contains("/repo/package.json"));
        assert!(conflict.contains("/repo/pnpm-lock.yaml"));

        let schema = crate::i18n::interpolate(
            catalog["err.lock_schema3_npm_metadata_missing"].1,
            &[("backend", "npm:prettier"), ("platform", "linux-x64")],
        );
        assert!(schema.contains("缺少 npm 元数据"));
        assert!(schema.contains("npm:prettier"));

        let help = catalog["help.use.long"].1;
        for expected in [
            "package.json",
            "原生锁文件",
            "osdk.toml",
            "osdk.lock",
            "--global",
            "忽略当前项目",
            "隔离前缀",
            "Aube",
            "构建脚本默认禁用",
        ] {
            assert!(
                help.contains(expected),
                "Chinese use help misses {expected}"
            );
        }
        let option = catalog["help.use.flag.opt"].1;
        for expected in [
            "installer=auto|aube|npm|pnpm",
            "allow_builds",
            "项目安装始终禁用脚本",
        ] {
            assert!(
                option.contains(expected),
                "Chinese -o help misses {expected}"
            );
        }
    }

    fn placeholders(message: &str) -> Vec<&str> {
        let mut result = Vec::new();
        let mut rest = message;
        while let Some(open) = rest.find('{') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('}') else {
                break;
            };
            result.push(&rest[..close]);
            rest = &rest[close + 1..];
        }
        result.sort_unstable();
        result
    }
}
