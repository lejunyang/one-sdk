//! 交互延迟基准：`hook-env` 与 shim 路径上的遍历量与耗时。
//!
//! 为什么不用 criterion：本仓库把依赖数量当产品指标（见 AGENTS.md「二进制体积」），
//! 而这里真正要守的指标是**遍历量**——目录数与 stat 次数。它是确定性的，
//! 在噪声环境里也能稳定暴露退化；墙钟只作参考。criterion 的统计能力用不上，
//! 却要新增一整棵依赖树。
//!
//! 用 `cargo bench -p osdk-core` 运行。基准自己搭临时目录树，
//! 不读用户真实的 SDK 状态（AGENTS.md 的硬性要求）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use osdk_core::inventory::{
    scan_installs, DynamicToolBin, DynamicToolManifest, ScanOptions, INVENTORY_FILE,
};
use osdk_core::tool::{InstallIdentity, InstallScope};

/// 一次扫描走过的目录数与 stat 次数。
///
/// 直接数文件系统条目，而不是包装 walkdir：基准要独立于被测实现，
/// 用同一份代码统计就无法发现被测实现少走了该走的地方。
struct WalkCost {
    dirs: usize,
    entries: usize,
}

fn measure_tree(root: &Path) -> WalkCost {
    let mut dirs = 0;
    let mut entries = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&current) else {
            continue;
        };
        dirs += 1;
        for entry in read.flatten() {
            entries += 1;
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let name = entry.file_name();
                // 与扫描器一致：跳过隐藏目录（`.locks`、暂存树）
                if !name.to_string_lossy().starts_with('.') {
                    stack.push(entry.path());
                }
            }
        }
    }
    WalkCost { dirs, entries }
}

/// 按真实布局写一个动态安装：`installs/<tool>/<version>/<install_id>/`。
fn write_install(installs: &Path, tool: &str, version: &str, payload_dirs: usize) -> PathBuf {
    let identity = InstallIdentity::new(
        tool,
        version,
        "linux-x64",
        InstallScope::Isolated,
        &BTreeMap::new(),
        Vec::new(),
        BTreeMap::from([("root-sri".into(), "sha512-example".into())]),
    )
    .expect("identity");
    let root = installs
        .join(osdk_core::dirs::sanitize_tool_id(&identity.tool))
        .join(osdk_core::dirs::sanitize_version_component(
            &identity.version,
        ))
        .join(osdk_core::dirs::install_id_component(&identity.install_id).expect("install id"));

    let mut manifest = DynamicToolManifest::from_identity(identity).expect("manifest");
    let relative = "bin/tool";
    let absolute = root.join(relative);
    std::fs::create_dir_all(absolute.parent().expect("parent")).expect("bin dir");
    std::fs::write(&absolute, b"#!/bin/sh\n").expect("bin");
    manifest.bins.push(DynamicToolBin {
        name: "tool".into(),
        path: relative.into(),
        ..Default::default()
    });

    // 安装负载：conda prefix 里是完整的 Python/mingw 发行版，
    // 这正是扫描不该进入的部分。
    for i in 0..payload_dirs {
        let nested = root
            .join("share")
            .join(format!("pkg{i}"))
            .join("internal")
            .join("deep");
        std::fs::create_dir_all(&nested).expect("payload");
        std::fs::write(nested.join("data.txt"), b"payload").expect("payload file");
    }

    manifest.write_atomic(&root).expect("write manifest");
    root
}

/// 静态 backend 的目录：从不含 manifest，但扫描仍会走进去。
/// android-sdk 在本机是 4,309 个目录、zig 是 2,242 个，全都没有 manifest。
fn write_static_backend(installs: &Path, name: &str, dirs: usize) {
    for i in 0..dirs {
        let nested = installs
            .join(name)
            .join("24.1.0")
            .join("payload")
            .join(format!("part{i}"));
        std::fs::create_dir_all(&nested).expect("static dir");
        std::fs::write(nested.join("file.bin"), b"x").expect("static file");
    }
}

fn bench(label: &str, iterations: u32, mut body: impl FnMut()) -> f64 {
    // 先跑一次预热：首次遍历要把目录项读进系统缓存，
    // 把它算进平均值会淹没真实差异。
    body();
    let started = Instant::now();
    for _ in 0..iterations {
        body();
    }
    let per_call = started.elapsed().as_secs_f64() * 1000.0 / f64::from(iterations);
    // 快路径会低于 0.01 ms，用两位小数会全部显示成 0.00，
    // 那样就看不出退化了——按量级切换单位。
    if per_call < 0.01 {
        println!("  {label:<48} {:>8.1} µs/次", per_call * 1000.0);
    } else {
        println!("  {label:<48} {per_call:>8.2} ms/次");
    }
    per_call
}

fn main() {
    println!("== 交互延迟基准（每个提示符都会付一次 hook-env）==\n");

    let temporary = tempfile::tempdir().expect("tempdir");
    let installs = temporary.path();

    // 复刻本机形态：动态安装集中在一个 backend 下，
    // 而绝大多数遍历量来自不含 manifest 的静态 backend。
    let mut a_real_install = None;
    for tool in [
        "conda:nasm",
        "conda:cmake",
        "conda:ninja",
        "conda:m2-bash",
        "conda:m2-sed",
    ] {
        let root = write_install(installs, tool, "1.0.0", 12);
        a_real_install.get_or_insert(root);
    }
    let a_real_install = a_real_install.expect("至少写入一个安装");
    // 多段 id：确认它们没有因为深度裁剪而被漏扫
    write_install(installs, "github:owner/repo", "2.0.0", 4);
    write_install(installs, "npm:@scope/pkg", "3.0.0", 4);
    write_install(installs, "go:github.com/user/cmd/tool", "4.0.0", 4);

    write_static_backend(installs, "android-sdk", 40);
    write_static_backend(installs, "zig", 30);
    write_static_backend(installs, "node", 10);

    let cost = measure_tree(installs);
    println!("固定装置：{} 个目录 / {} 个条目", cost.dirs, cost.entries);

    let report = scan_installs(installs, &ScanOptions::default()).expect("scan");
    println!("扫描发现 {} 个动态安装\n", report.installs.len());

    // 多段 id 必须全部被发现。深度裁剪会让 go:（展开 7 段、manifest 在深度 8）
    // 被漏扫，而只装 conda:（2 段）的机器上测不出来——这条断言就是拦这个的。
    let mut found: Vec<&str> = report
        .installs
        .iter()
        .map(|install| install.canonical_id.as_str())
        .collect();
    found.sort_unstable();
    assert!(
        found.contains(&"go:github.com/user/cmd/tool"),
        "多段 id 被漏扫，扫描范围收得太紧：{found:?}"
    );
    assert_eq!(found.len(), 8, "预期 8 个动态安装，实际 {found:?}");

    println!("-- 扫描耗时 --");
    bench("scan_installs（fail-closed）", 20, || {
        let report = scan_installs(installs, &ScanOptions::default()).expect("scan");
        assert_eq!(report.installs.len(), 8);
    });
    bench("scan_installs（tolerant）", 20, || {
        let report = scan_installs(installs, &ScanOptions::tolerant()).expect("scan");
        assert_eq!(report.installs.len(), 8);
    });

    println!("\n-- 遍历量（确定性指标，退化时先看这里）--");
    // 装置里每个 install 都埋了负载子树。扫描在 manifest 处剪枝，
    // 所以它走过的目录数应当远小于整棵树。
    let payload_dirs = count_payload_dirs(installs);
    println!(
        "  整棵树目录数                                  {:>8}",
        cost.dirs
    );
    println!("  其中属于安装负载（不该进入）                    {payload_dirs:>8}");
    println!(
        "  剪枝理应避开的比例                            {:>7.1}%",
        100.0 * payload_dirs as f64 / cost.dirs as f64
    );

    // 静态 backend 的子树是遍历量的主体（本机 android-sdk 单独 4,309 个
    // 目录、zig 2,242 个，两者都没有一个 manifest），扫描只需浅探一层。
    //
    // 这条护栏靠诱饵生效，而不是靠数目录：埋在静态子树深处的 manifest，
    // 其 identity 与所在路径不符，fail-closed 扫描一旦走到就会报错。所以
    // 「扫描仍然成功」等价于「那棵树没有被走穿」，且判断必然经过
    // `inventory.rs` 的裁剪判据本身。
    //
    // 曾经在这里数「扫描走过多少目录」，办法是在基准里复刻一份判据——那
    // 与产品代码脱钩，改判据它照样输出同一个数字，等于没测。
    write_decoy_manifest(installs, "zig", &a_real_install);
    let guarded = scan_installs(installs, &ScanOptions::default());
    assert!(
        guarded.is_ok(),
        "扫描走进了静态 backend 的子树并撞上诱饵 manifest：裁剪判据失效",
    );
    assert_eq!(
        guarded.expect("scan").installs.len(),
        8,
        "诱饵不该改变扫描结果",
    );

    println!("\n-- 激活片段渲染（纯字符串，作为对照基线）--");
    bench("activation_script(powershell)", 2000, || {
        let script =
            osdk_core::activate::activation_script(osdk_core::activate::Shell::Powershell, "osdk");
        assert!(script.contains("function global:prompt"));
    });

    println!("\n提示：只看 ms 会被机器噪声误导，退化时先对比遍历量。");
}

/// 数出属于安装负载的目录——扫描剪枝生效时这些都不该被进入。
fn count_payload_dirs(installs: &Path) -> usize {
    let mut count = 0;
    for entry in walkdir::WalkDir::new(installs)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_dir() {
            continue;
        }
        // 位于某个含 manifest 的目录之下 => 是负载
        let mut ancestor = entry.path().parent();
        while let Some(dir) = ancestor {
            if dir.join(INVENTORY_FILE).is_file() {
                count += 1;
                break;
            }
            if dir == installs {
                break;
            }
            ancestor = dir.parent();
        }
    }
    count
}

/// 在静态 backend 子树深处埋一个 identity 与路径不符的 manifest。
///
/// 裁剪生效时扫描进不到这里，它是不可见的；裁剪一旦失效，fail-closed
/// 扫描走到就会因 identity 不匹配而报错。这样护栏的失败信号来自被测
/// 代码自己的行为，而不是基准里另写一份判据。
fn write_decoy_manifest(installs: &Path, static_tool: &str, borrow_from: &Path) {
    // 深度刻意超过浅探的一层：放在 <static>/<version>/<id> 这个
    // 「看起来像合法 install root」的位置上，裁剪失效才会走到。
    let decoy = installs.join(static_tool).join("0.13.0").join("deadbeef");
    std::fs::create_dir_all(&decoy).expect("decoy dir");
    // 直接借用一个真实 manifest：它声明的 identity 与这个路径完全不符，
    // 正是 fail-closed 扫描必须拒绝的东西。借用而不是另写一份，是为了
    // 不让诱饵因为格式演进而悄悄失效（那会让护栏变成空操作）。
    let source = borrow_from.join(".osdk-install.json");
    std::fs::copy(&source, decoy.join(".osdk-install.json")).expect("decoy manifest");
}
