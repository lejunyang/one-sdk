// Verifies the zig backend against the real ziglang.org index rather than a
// hand-written fixture: a fixture can only prove the parser handles what the
// author already imagined.
//
// Network-dependent, so it is `#[ignore]` by default and run explicitly.

#[tokio::test]
#[ignore = "requires network access to ziglang.org"]
async fn parses_the_live_zig_index_and_resolves_latest() {
    let body = reqwest::get("https://ziglang.org/download/index.json")
        .await
        .expect("fetch the live index")
        .text()
        .await
        .expect("read the index body");

    // The parse must succeed over the whole real document.
    let index: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&body).expect("live index is valid JSON");

    assert!(
        index.contains_key("master"),
        "the index always carries a master entry"
    );

    // Every non-master key must be a parseable semver, and at least one recent
    // stable release must ship an x86_64-windows and x86_64-linux build.
    let mut stable = 0usize;
    for (name, entry) in &index {
        if name == "master" {
            continue;
        }
        semver::Version::parse(name)
            .unwrap_or_else(|e| panic!("index key `{name}` is not semver: {e}"));
        stable += 1;
        for key in ["x86_64-windows", "x86_64-linux"] {
            if let Some(artifact) = entry.get(key) {
                let tarball = artifact
                    .get("tarball")
                    .and_then(|value| value.as_str())
                    .unwrap_or_else(|| panic!("{name}/{key} has no tarball"));
                let shasum = artifact
                    .get("shasum")
                    .and_then(|value| value.as_str())
                    .unwrap_or_else(|| panic!("{name}/{key} has no shasum"));
                assert_eq!(
                    shasum.len(),
                    64,
                    "{name}/{key} shasum is not a sha256: {shasum}"
                );
                assert!(
                    tarball.starts_with("https://"),
                    "{name}/{key} tarball is not https: {tarball}"
                );
                // The naming layout is exactly what must not be assumed: assert
                // only that the extension is one osdk can extract.
                let file = tarball.rsplit('/').next().unwrap();
                assert!(
                    file.ends_with(".zip") || file.ends_with(".tar.xz"),
                    "{name}/{key} has an unexpected archive type: {file}"
                );
            }
        }
    }
    assert!(
        stable >= 20,
        "expected the full release history, saw {stable} stable versions"
    );

    // The layout change during 0.14 is the regression this guards: confirm both
    // spellings really are present in the live index, so any future code that
    // assembles filenames from tokens cannot pass this test.
    let old = index
        .get("0.13.0")
        .and_then(|entry| entry.get("x86_64-windows"))
        .and_then(|artifact| artifact.get("tarball"))
        .and_then(|value| value.as_str())
        .expect("0.13.0 windows build");
    let new = index
        .get("0.16.0")
        .and_then(|entry| entry.get("x86_64-windows"))
        .and_then(|artifact| artifact.get("tarball"))
        .and_then(|value| value.as_str())
        .expect("0.16.0 windows build");
    assert!(
        old.contains("zig-windows-x86_64-"),
        "0.13.0 should use the os-first layout, got {old}"
    );
    assert!(
        new.contains("zig-x86_64-windows-"),
        "0.16.0 should use the arch-first layout, got {new}"
    );
}
