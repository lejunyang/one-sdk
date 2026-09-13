//! 测试用途：验证 Go 运行时后端向 go 命令注入的 GOPROXY 覆盖镜像回退顺序、
//! 尊重用户显式设置，并在配置禁用全部候选时不擅自改写，防止镜像选择对
//! `go build` / `go test` 静默失效的缺陷回归。

use super::go::{GoBackend, GO_MODULE_PROXY_TOOL};
use super::{Backend, Ctx};
use crate::config::{Config, Settings, SourcesConfig, ToolSources};
use crate::dirs::Dirs;
use crate::platform::{Arch, Libc, Os, Platform};
use crate::source::Source;
use crate::store::Cas;
use crate::version::ToolVersion;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// GOPROXY 是进程级环境变量，而 Rust 测试默认并行执行。若不串行化，
/// 一个用例设置的值会泄漏到另一个用例，产出与代码无关的假红或假绿。
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 在移除 GOPROXY 的干净环境里求值，结束后恢复原值，避免污染同进程其他用例。
fn without_goproxy<T>(body: impl FnOnce() -> T) -> T {
    let _guard = env_lock();
    let original = std::env::var_os("GOPROXY");
    std::env::remove_var("GOPROXY");
    let result = body();
    match original {
        Some(value) => std::env::set_var("GOPROXY", value),
        None => std::env::remove_var("GOPROXY"),
    }
    result
}

fn test_ctx(root: &std::path::Path) -> Ctx {
    let dirs = Dirs::resolve_from(|key| match key {
        "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
        "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
        "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
        "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
        "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
        _ => None,
    })
    .unwrap();
    dirs.ensure().unwrap();
    Ctx {
        cas: Arc::new(Cas::new(dirs.store.clone())),
        dirs,
        platform: Platform {
            os: Os::Linux,
            arch: Arch::X64,
            libc: Libc::Glibc,
        },
        config: Config {
            settings: Settings::default(),
            sources: SourcesConfig::default(),
            tools: Default::default(),
            tool_configs: Default::default(),
            global_tools: Default::default(),
            global_tool_configs: Default::default(),
            tool_origins: Default::default(),
            aliases: Default::default(),
            project_config_path: None,
        },
        client: reqwest::Client::new(),
        show_progress: false,
    }
}

fn goproxy_of(ctx: &Ctx) -> Option<String> {
    let version = ToolVersion::new("go", "1.26.5");
    GoBackend
        .exec_env(ctx, &version)
        .unwrap()
        .get("GOPROXY")
        .cloned()
}

/// 缺陷本体：镜像只作用于工具链归档下载，模块拉取仍直连 proxy.golang.org。
/// 断言 GOPROXY 真的被注入、官方源在前、镜像随后、`direct` 兜底。
///
/// 分隔符必须是 `|` 而非 `,`：逗号只在 404/410 时回退，连接超时被视为终止错误，
/// 于是首个不可达的代理就让整条链失效——而这恰恰是需要镜像的场景。
#[test]
fn exec_env_publishes_ranked_module_proxy_list() {
    let temporary = tempfile::tempdir().unwrap();
    let ctx = test_ctx(temporary.path());
    let proxy = without_goproxy(|| goproxy_of(&ctx)).expect("GOPROXY must be injected");

    assert_eq!(
        proxy,
        "https://proxy.golang.org|https://goproxy.cn|https://mirrors.aliyun.com/goproxy|direct"
    );
    assert!(
        !proxy.contains(','),
        "comma only falls back on 404/410, so a timeout would end the chain: {proxy}"
    );
}

/// 用户显式设置（含 `off` / `direct` 这类策略值）必须优先于 osdk 的默认，
/// 否则会把用户刻意选择的离线或直连策略悄悄改掉。
#[test]
fn explicit_goproxy_is_never_overridden() {
    let temporary = tempfile::tempdir().unwrap();
    let ctx = test_ctx(temporary.path());

    let _guard = env_lock();
    for chosen in ["https://private.example.test", "off", "direct"] {
        std::env::set_var("GOPROXY", chosen);
        assert_eq!(
            goproxy_of(&ctx),
            None,
            "osdk overrode a user-set GOPROXY of `{chosen}`"
        );
    }
    std::env::remove_var("GOPROXY");
}

/// 镜像集受 `[sources."go-modules"]` 管理：禁用某个源后它必须从回退列表里消失，
/// 且这套配置与工具链归档源 `[sources.go]` 相互独立。
#[test]
fn module_proxy_honours_disabled_and_custom_sources() {
    let temporary = tempfile::tempdir().unwrap();
    let mut ctx = test_ctx(temporary.path());
    ctx.config.sources.per_tool.insert(
        GO_MODULE_PROXY_TOOL.to_string(),
        ToolSources {
            disable: vec!["proxy.golang.org".into()],
            custom: vec![Source::mirror("corp", "https://goproxy.corp.example", 1)],
            ..Default::default()
        },
    );

    let proxy = without_goproxy(|| goproxy_of(&ctx)).expect("GOPROXY must be injected");

    assert!(
        !proxy.contains("proxy.golang.org"),
        "disabled source still offered: {proxy}"
    );
    assert!(
        proxy.starts_with("https://goproxy.corp.example|"),
        "custom source did not win by priority: {proxy}"
    );
    assert!(proxy.ends_with("|direct"), "missing direct fallback: {proxy}");
}
