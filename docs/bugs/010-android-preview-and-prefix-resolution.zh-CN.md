# 010 — Android 预览包发布在稳定通道；`android-36` 被前缀匹配吃掉

**状态**：已修复 · **严重度**：高 · **实测环境**：Windows x64、osdk 0.0.2、Google `repository2-4.xml` 与 `sys-img/google_apis/sys-img2-3.xml`（2026-09-14 实抓）

## 现象

两个独立缺陷，因为都表现为「装错了 Android 包」而长期被当成一个。

**A：`latest` 解析到预览版。** 尽管 prerelease 策略是默认的 `if-explicit`：

```
$ osdk outdated android-system-images
android-system-images android-35;google_apis;x86_64
  -> android-37.2-beta3;google_apis_ps16k;x86_64
```

**B：`android-36` 命中 `android-36.1`。**

```
$ osdk install "android-platforms@android-36"
android-platforms@android-36.1 already installed
```

而项目的 `compileSdk`/`targetSdk` 是 36，`android-36.1` 的 `AndroidVersion.ApiLevel`
是 **36.1**，不是 36。当时也没有精确锁定的写法可用：

```
$ osdk install "android-platforms@=android-36"
error: could not resolve version `=android-36`
```

## 根因

### A：`<channelRef>` 不是预览信号

`crates/osdk-core/src/backend/android.rs` 的 `list_remote_versions()`：

```rust
stable: package.channel == Channel::Stable,
```

看着完全合理，但**与 Google 的实际发布方式不符**。实抓验证：

| 包 | api-level | codename | channelRef | license |
| --- | --- | --- | --- | --- |
| `platforms;android-37.2-beta3` | 37.1 | DEV | **channel-0** | android-sdk-license |
| `platforms;android-CANARY` | 37.1 | CANARY | **channel-0** | android-sdk-license |
| `system-images;android-37.2-beta3;google_apis_ps16k;x86_64` | 37.1 | DEV | **channel-0** | android-sdk-license |
| `platforms;android-36`（成品） | 36 | — | channel-0 | android-sdk-license |

预览包与成品包在 channel 和 license 上**逐字段相同**。于是 `stable` 对一个 DEV
预览返回 `true`，`if-explicit` 策略过滤时看不到任何「不稳定」候选，预览被放行。

真正的信号是 `<type-details>` 里的 `<codename>` 与 `<beta-api-level>`，而
`parse_remote_package()`（`android/repo.rs`）**把整个 `<type-details>` 丢掉了**。

`install` 里的 channel 门（`package.channel > allowed`）同样盲，原因一样。

### B：版本尾是包标识符，不是版本号

`RemotePackage::version()` 返回 path tail（`android-36.1`），这是**包标识符**；
`version_has_prefix()` 按 `.` 分段比较，于是 `android-36` 是 `android-36.1` 的
component 前缀，而 `select_version` 从新到旧扫，先撞上 `android-36.1`。

即：目录里明明有一个字面叫 `android-36` 的包，请求 `android-36` 却拿不到它。

## 影响

- **A**：任何 `latest`（含 `osdk exec -t android-system-images` 这种隐式 latest）
  都可能装到预览版；系统镜像单个 2.4 GB，代价直接可见。
- **B**：固定 `compileSdk = 36` 的项目静默编译到 API 36.1，**不报错**。这类"版本悄悄
  变高"的问题只会在运行期或 lint 阶段以别的形态暴露。
- 两者叠加时，用户唯一能落到 API 36 的写法是 `android-36-ext19`（ApiLevel 报告为
  `36x` 的 side-by-side 扩展包），而这既不直观也不是本意。

## 修复

**A** — 解析 `<type-details>` 并综合三个信号判定预览（`RemotePackage::is_preview`）：
`<codename>`/`<beta-api-level>`、非稳定 `<channelRef>`、版本尾的预发布标签。**三者都
需要**，因为没有任何一个覆盖全部家族：platforms 只有 codename、ndk/emulator 用
channel、build-tools 的 rc 只有版本尾（`37.0.0-rc2`，且它的 channel 是 channel-0）。

版本尾判定刻意是**白名单**（`rc`/`beta`/`alpha`/… + 可选序号），不是「有连字符就算」：
`android-36-ext19` 的尾是 `ext19`，它是已发布的扩展包。这一点是被自己写的测试抓出来
的 —— 第一版实现用了 `split_prerelease().1.is_some()`，直接把 `android-36-ext19`
误判成预览，而它恰恰是当时唯一能落到 API 36 的包。

顺带把解析出的 `api_level` 用在 `osdk list-remote`：

```
android-36-ext19 (API 36x)
android-36 (API 36)
android-36.1 (API 36.1)
```

**名字看不出 API 级别**正是本次难以诊断的根源，现在列表里直接可读。

**B** — 两处，都只加能力、不改既有语义：

1. `VersionSpec::Pinned`：`=android-36` 逐字符匹配一个已发布版本，且**不像 `Exact`
   那样回退**到更宽松的匹配层级。
2. 精确标识符优先：请求串与某候选完全相等时该候选胜出，然后才走前缀扫描。不是
   published identifier 的前缀（如 `20` 对 Node）行为完全不变。

**刻意没做**：重新定义 platforms 的"版本"语义（例如让 `android-36` 去匹配
`api-level = 36`）。回归面太大，且 `android-36` 与 `android-36.1` 谁算"36"本身有歧义；
`36x` 这类非数字值还要额外规则。

### 关于 `36.0`

`osdk install "android-platforms@36.0"` 命不中任何东西，这是**目录事实，不是缺陷**：
Google 只发布 `android-36` 与 `android-36.1`，没有 `android-36.0`；而这个家族的标识符
都带 `android-` 命名空间，所以裸数字 `36.0` / `36` 谁都匹配不上。正确写法是
`=android-36`。已用 `a_bare_numeric_selector_does_not_reach_a_namespaced_identifier`
锁住，防止有人日后为它加特例映射。

## 回归防线

- `type_details_identify_stable_channel_previews` — fixture 直接复刻实抓的字段组合，
  断言预览包的 `channel` **是** `Stable`、`license` 与成品相同，而 `is_preview()` 仍为
  真。同时断言 `android-36-ext19` 不是预览、`build-tools;37.0.0-rc2` 是。
- `only_known_prerelease_tags_count_as_a_preview_tail` — 白名单边界，含
  `1.0-rchive` / `1.0-betamax` 这类"以标签字母开头但不是标签"的反例。
- `exact_identifier_beats_a_longer_prefix_sibling` — 候选按清单的从旧到新排列，所以
  修复前的从新到旧扫描必然先撞 `android-36.1`；另有一组不含基础包的候选，断言前缀
  扫描的原有行为未变。
- `pinned_specs_match_only_a_literal_published_version` — 断言 `Pinned` 不做 `Exact`
  的点分前缀回退（`=21.0.12` 匹配不到 `21.0.12.1+1`，而 `Exact("21.0.12")` 可以）。
- `a_pin_counts_as_explicit_for_the_prerelease_policy` — 锁定的预览在 `if-explicit`
  与 `allow` 下都可达，`never` 下被拒；同时断言裸 `latest` 仍停在稳定版。

## 教训

**"通道"这个名字暗示了它没有的权威性。** 这与本目录 README 记录的 005–008 是同一个
形状：某个机制的作用域比它的名字暗示的要小或大。这里是 `channelRef` 看起来就该是
"稳定还是预览"的答案，实际上 Google 用它表达别的东西，真正的答案在一个我们从未解析
的元素里。**判断依据必须来自上游实际怎么发布，而不是字段名读起来像什么** —— 而这只能
靠抓真实清单来确认，任何离线 fixture 都会复刻我们自己的误解。
