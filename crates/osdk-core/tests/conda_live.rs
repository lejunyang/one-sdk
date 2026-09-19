//! Live conda repodata checks.
//!
//! Ignored by default: these hit real channel servers. Run with
//! `cargo test -p osdk-core --test conda_live -- --ignored --nocapture`.
//!
//! Their value is what a fixture cannot show: that the mirrors really serve the
//! layout rattler expects, and that the packages motivating this backend are
//! reachable on the platforms users will actually ask for.

#![cfg(feature = "install")]

use osdk_core::backend::conda::{parse_channels, subdir_for, CondaBackend, DEFAULT_CHANNEL};
use osdk_core::platform::{Arch, Libc, Os, Platform};

fn platform(os: Os, arch: Arch) -> Platform {
    Platform {
        os,
        arch,
        libc: Libc::None,
    }
}

/// Every platform osdk targets must map to a subdir conda actually publishes,
/// so an unsupported platform is reported up front instead of failing deep
/// inside a solve. This needs no network.
#[test]
fn every_supported_platform_maps_to_a_real_conda_subdir() {
    for (os, arch, expected) in [
        (Os::Linux, Arch::X64, "linux-64"),
        (Os::Linux, Arch::Arm64, "linux-aarch64"),
        (Os::Macos, Arch::X64, "osx-64"),
        (Os::Macos, Arch::Arm64, "osx-arm64"),
        (Os::Windows, Arch::X64, "win-64"),
        // conda-forge really does publish win-arm64: its `repodata.json.zst`
        // is served, and clang, python and ripgrep all have builds there.
        (Os::Windows, Arch::Arm64, "win-arm64"),
    ] {
        assert_eq!(subdir_for(platform(os, arch)), Some(expected));
    }
    // 32-bit is the genuine gap; claiming support would produce an empty solve
    // rather than a clear error.
    assert_eq!(subdir_for(platform(Os::Linux, Arch::X86)), None);
}

#[test]
fn a_conda_id_round_trips_through_the_backend() {
    let backend = CondaBackend::from_id("conda:clang").expect("valid id");
    assert_eq!(backend.package(), "clang");
}

/// `clang` is the package that motivated this backend: vfox-clang installs it
/// by shelling out to pixi against conda-forge, and it is the C/C++ toolchain
/// osdk users actually ask for. Every platform osdk maps must really have a
/// build, or `conda:clang` would be a promise the backend cannot keep.
#[tokio::test]
#[ignore = "requires network access to conda channels"]
async fn clang_has_a_build_for_every_platform_osdk_maps() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("build a client");

    let body: serde_json::Value = client
        .get("https://api.anaconda.org/package/conda-forge/clang")
        .send()
        .await
        .expect("reach anaconda.org")
        .json()
        .await
        .expect("parse the package listing");
    let files = body
        .get("files")
        .and_then(|files| files.as_array())
        .expect("a file list");

    for (os, arch) in [
        (Os::Linux, Arch::X64),
        (Os::Linux, Arch::Arm64),
        (Os::Macos, Arch::X64),
        (Os::Macos, Arch::Arm64),
        (Os::Windows, Arch::X64),
        (Os::Windows, Arch::Arm64),
    ] {
        let subdir = subdir_for(platform(os, arch)).expect("a mapped platform");
        let count = files
            .iter()
            .filter(|file| {
                file.get("attrs")
                    .and_then(|attrs| attrs.get("subdir"))
                    .and_then(|value| value.as_str())
                    == Some(subdir)
            })
            .count();
        println!("clang {subdir}: {count} builds");
        assert!(
            count > 0,
            "conda-forge has no `clang` build for {subdir}, which osdk claims to support"
        );
    }
}

/// The mirrors this backend ships as defaults must actually serve conda
/// repodata. A mirror that 404s here would silently push every user onto the
/// slow upstream path.
#[tokio::test]
#[ignore = "requires network access to conda mirrors"]
async fn default_mirrors_serve_conda_repodata() {
    let bases = [
        ("upstream", "https://conda.anaconda.org"),
        (
            "tuna",
            "https://mirrors.tuna.tsinghua.edu.cn/anaconda/cloud",
        ),
        ("bfsu", "https://mirrors.bfsu.edu.cn/anaconda/cloud"),
        ("nju", "https://mirror.nju.edu.cn/anaconda/cloud"),
    ];
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("build a client");

    let mut reachable = 0usize;
    for (name, base) in bases {
        let url = format!("{base}/{DEFAULT_CHANNEL}/noarch/repodata.json.zst");
        match client.head(&url).send().await {
            Ok(response) if response.status().is_success() => {
                reachable += 1;
                println!("{name}: ok ({})", response.status());
            }
            Ok(response) => println!("{name}: unexpected status {}", response.status()),
            Err(error) => println!("{name}: unreachable ({error})"),
        }
    }
    assert!(
        reachable >= 2,
        "expected at least two reachable conda sources, got {reachable}"
    );
}

/// The reason multi-channel support is not optional: `cuda-toolkit`, the
/// package this backend was asked for, has no `win-64` build on conda-forge.
/// A Windows user must reach the `nvidia` channel, so a single-channel design
/// (which is what mise implements) cannot express this request at all.
#[tokio::test]
#[ignore = "requires network access to conda channels"]
async fn cuda_toolkit_on_windows_exists_only_in_the_nvidia_channel() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("build a client");

    async fn has_package(client: &reqwest::Client, channel: &str, package: &str) -> bool {
        let url = format!("https://api.anaconda.org/package/{channel}/{package}");
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => {
                let body: serde_json::Value = match response.json().await {
                    Ok(body) => body,
                    Err(_) => return false,
                };
                body.get("files")
                    .and_then(|files| files.as_array())
                    .is_some_and(|files| {
                        files.iter().any(|file| {
                            file.get("attrs")
                                .and_then(|attrs| attrs.get("subdir"))
                                .and_then(|subdir| subdir.as_str())
                                == Some("win-64")
                        })
                    })
            }
            _ => false,
        }
    }

    let on_nvidia = has_package(&client, "nvidia", "cuda-toolkit").await;
    let on_conda_forge = has_package(&client, "conda-forge", "cuda-toolkit").await;
    println!("cuda-toolkit win-64: nvidia={on_nvidia} conda-forge={on_conda_forge}");

    assert!(
        on_nvidia,
        "cuda-toolkit should have a win-64 build in the nvidia channel"
    );
    assert!(
        !on_conda_forge,
        "conda-forge gained a win-64 cuda-toolkit; the multi-channel rationale \
         in the docs should be revisited"
    );

    // And the option that expresses it must preserve the order it was given.
    let channels = parse_channels("nvidia,conda-forge").expect("valid channels");
    let names: Vec<_> = channels.iter().map(|channel| channel.name()).collect();
    assert_eq!(names, ["nvidia", "conda-forge"]);
}
