# Java 与 JVM 工具

本页覆盖 Java JDK / JRE，以及 Maven、Gradle、Kotlin 等 JVM 工具。

## Java JDK 与 JRE

```bash
osdk install java@21
osdk install java@zulu-17.0.1
osdk install java@21 -o distribution=zulu -o package-type=jdk
osdk install java@21 -o package-type=jre
```

| 选项 | 值 | 默认值 |
| --- | --- | --- |
| `distribution` | Foojay distribution ID，如 `temurin`、`zulu` | `temurin` |
| `package-type` | `jdk` 或 `jre` | `jdk` |

发行版也可写进版本请求，例如 `java@temurin-21`。Foojay 查询按发行版、操作系统、
架构、archive 类型、JDK/JRE 和 Linux libc 过滤。JRE identity 为
`jre-<resolved-version>`，可与相同版本的 JDK 共存；执行 JDK/JRE 时导出 `JAVA_HOME`。

Temurin 版本带 build 号（如 `21.0.12+8`），PSU 还会有第四段（如 `21.0.12.1+1`）。
版本请求可以省略 build 号：`java@21.0.12` 会匹配同一核心版本的 `21.0.12+8`；只有当
同一核心版本不存在时，才回退匹配四段式 PSU。

内置 Temurin LTS catalog 包含 8、11、17、21、25，可在空 metadata 缓存下解析；
已锁定的 artifact 在 Foojay 不可用时也可安装。可设置兼容 Foojay `/packages` 的
端点或静态镜像：

```toml
[settings.java]
catalog_url = "https://mirror.example/disco/v3.0/packages"
```

未设置这个单一显式 catalog 时，Java 会按 source 排序逐个查询 Foojay-compatible
endpoint；某个 endpoint 可访问但没有请求的 distribution/JDK/JRE 组合时继续后备源。
对应的 checksum detail 也按相同顺序回退，最终 vendor 归档仍由通用校验下载管线处理。

## JVM 工具

```bash
osdk install maven@3.9.16
osdk install "gradle@=9.3.1"
osdk install kotlin@2.4.10
```

**Gradle 按上游版本索引解析**，因此任何历史版本都可安装：`osdk list-remote gradle`
列出全部正式发布（2026-09-14 实测 179 个），每个版本的下载地址与 SHA-256 都取自索引，
无需在 osdk 内逐版本硬编码。索引里的 nightly、`-rc-N` 与 `-milestone-N` 会被判为非
稳定，所以 `latest` 只会落到正式版；需要预览版请按名字显式指定。若某条记录没有校验和，
安装会被拒绝而不是降级为不校验。

Maven 与 Kotlin 的上游没有同类的可机读索引，仍是固定集合：Maven 仅 `3.9.16`
（SHA-512）、Kotlin 仅 `2.4.10`（SHA-256），请求其他版本会失败。三者都拥有独立安装
目录和 shim；Kotlin 的 GitHub 下载还提供代理候选。Maven/Kotlin 使用排序后的有效
source URL；Gradle 会逐源读取 index，并把 index 中的绝对 distribution URL 重映射到
每个排序 source，因此 index 可读但该源缺少目标 zip 时仍会回退。

::: tip 与 Gradle Wrapper 的关系
项目里已有 `gradle/wrapper/gradle-wrapper.properties` 时，**wrapper 仍是权威来源** ——
它的 `distributionSha256Sum` 是比版本 pin 更强的保障。osdk 的 gradle pin 适用于没有
wrapper 的场景（例如新建项目或直接用 `gradle` 命令）。两者的摘要同源：实测索引中
9.3.1 的 checksum 与 wrapper 声明的 `distributionSha256Sum` 逐字符一致。
:::
