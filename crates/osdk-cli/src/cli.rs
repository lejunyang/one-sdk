//! Clap command tree for the `osdk` binary.

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "osdk",
    version,
    about = "One SDK manager: unified version, dependency, and cache management for many SDKs",
    long_about = None,
    propagate_version = true,
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, clap::Args)]
pub struct GlobalArgs {
    /// Increase verbosity (repeatable).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress progress output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Max concurrent downloads/installs.
    #[arg(short = 'j', long, global = true, env = "OSDK_JOBS")]
    pub jobs: Option<usize>,

    /// Assume yes for prompts.
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,

    /// Force use of a specific source id for this invocation.
    #[arg(long, global = true, value_name = "ID")]
    pub source: Option<String>,

    /// Re-probe sources, ignoring cached speed results.
    #[arg(long, global = true)]
    pub refresh_sources: bool,

    /// Disable network access and use cached metadata/artifacts only.
    #[arg(long, global = true, env = "OSDK_OFFLINE")]
    pub offline: bool,

    /// Reject artifacts that have no verifiable checksum.
    #[arg(long, global = true, env = "OSDK_REQUIRE_CHECKSUMS")]
    pub require_checksums: bool,

    /// GitHub artifact attestation policy: off|if-available|required.
    #[arg(long, global = true, env = "OSDK_ATTESTATIONS")]
    pub attestations: Option<osdk_core::config::AttestationPolicy>,

    /// Pre-release policy: never|if-explicit|allow.
    #[arg(long, global = true, env = "OSDK_PRERELEASE")]
    pub prerelease: Option<osdk_core::config::PrereleasePolicy>,

    /// Output language (en|zh); overrides locale and OSDK_LANG.
    #[arg(long, global = true, value_name = "LANG")]
    pub lang: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install one or more tools (from args, or from resolved config).
    #[command(alias = "i")]
    Install {
        /// e.g. `node@20`, `go@1.22`, `python@3.12`. Empty = install from config.
        tools: Vec<String>,
        /// Backend-specific option as key=value (repeatable), e.g.
        /// `-o profile=minimal -o components=clippy,rustfmt` (rust),
        /// `-o distribution=zulu` (java). Applied to all listed tools.
        #[arg(short = 'o', long = "opt", value_name = "KEY=VALUE")]
        opts: Vec<String>,
    },

    /// Resolve project tools and write exact versions to osdk.lock.
    Lock {
        /// Optional tool requests; empty resolves the current project config.
        tools: Vec<String>,
        /// Backend-specific option as key=value (repeatable).
        #[arg(short = 'o', long = "opt", value_name = "KEY=VALUE")]
        opts: Vec<String>,
    },

    /// Show installed versions that differ from the current remote resolution.
    Outdated {
        /// Optional tool requests; empty checks the current project config.
        tools: Vec<String>,
    },

    /// Install the latest versions matching project or explicit requests.
    Upgrade {
        /// Optional tool requests; empty upgrades the current project config.
        tools: Vec<String>,
        /// Backend-specific option as key=value (repeatable).
        #[arg(short = 'o', long = "opt", value_name = "KEY=VALUE")]
        opts: Vec<String>,
    },

    /// Install tools if needed and run a command with their exact environment.
    Exec {
        /// Tool request to expose, repeatable (e.g. --tool node@20).
        #[arg(short = 't', long = "tool", required = true)]
        tools: Vec<String>,
        /// Command and arguments after `--`.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// Generate shell completion code.
    Completions {
        /// Target shell.
        shell: clap_complete::Shell,
    },

    /// Manage user-defined version aliases.
    Alias {
        #[command(subcommand)]
        command: AliasCommand,
    },

    /// List installed versions.
    #[command(alias = "ls")]
    List {
        /// Restrict to a single tool.
        tool: Option<String>,
    },

    /// List installable versions from the remote index.
    #[command(alias = "lsr", name = "list-remote")]
    ListRemote {
        tool: String,
        /// Only show versions matching this prefix (e.g. `20`).
        filter: Option<String>,
    },

    /// Install a tool if needed and make it active.
    #[command(alias = "u")]
    Use {
        /// Tool and version, e.g. `node@20`, `npm:prettier@3`, or `go:golang.org/x/tools/gopls@0.20.0`.
        tool: String,
        /// Use global selection; npm packages use a controlled global prefix, while Go tools use the user config/lock.
        #[arg(short, long)]
        global: bool,
        /// Backend option as key=value (repeatable); npm supports installer/allow_builds and Go tools support tags/env.
        #[arg(short = 'o', long = "opt", value_name = "KEY=VALUE")]
        opts: Vec<String>,
    },

    /// Uninstall a tool version.
    #[command(alias = "rm")]
    Uninstall {
        /// e.g. `node@20.11.1`.
        tool: String,
        /// Remove a user-global npm package installation and its selection state; other backends are not supported.
        #[arg(short, long)]
        global: bool,
    },

    /// Show the active version of each tool for the current directory.
    Current { tool: Option<String> },

    /// Print the install directory of a tool version.
    Where {
        /// e.g. `node` or `node@20.11.1`.
        tool: String,
        /// Resolve an npm package from global scope, ignoring project selection; other backends are not supported.
        #[arg(short, long)]
        global: bool,
    },

    /// Regenerate shim launchers for all installed tools.
    Reshim,

    /// Print shell integration to eval, e.g. `eval "$(osdk activate bash)"`.
    Activate {
        /// Target shell.
        shell: String,
    },

    /// Print shell code that removes osdk integration and restores the environment.
    Deactivate {
        /// Target shell.
        shell: String,
    },

    /// Internal: emit env changes for the current directory (used by activate).
    #[command(hide = true)]
    HookEnv {
        #[arg(long, default_value = "bash")]
        shell: String,
    },

    /// Manage download sources.
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },

    /// Inspect project dependency registry selection.
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },

    /// Inspect or edit configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Trust project configuration that can affect execution or download sources.
    Trust {
        /// Project config file or directory; defaults to the nearest project config.
        path: Option<std::path::PathBuf>,
        #[command(subcommand)]
        command: Option<TrustCommand>,
    },

    /// Remove trust for a project configuration.
    Untrust {
        /// Project config file or directory; defaults to the nearest project config.
        path: Option<std::path::PathBuf>,
    },

    /// Manage Node-specific workflows.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },

    /// Manage Python-specific workflows.
    Python {
        #[command(subcommand)]
        command: PythonCommand,
    },

    /// Manage Android SDK-specific workflows.
    Android {
        #[command(subcommand)]
        command: AndroidCommand,
    },

    /// Manage local large-model snapshots.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },

    /// Manage Rust components, targets, status, overrides, and linked toolchains.
    Rust {
        #[command(subcommand)]
        command: RustCommand,
    },

    /// Manage the shared caches (SDK store + downstream package caches).
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },

    /// Inspect native container runtimes, builders, and their caches.
    Container {
        #[command(subcommand)]
        command: ContainerCommand,
    },

    /// Garbage-collect unreferenced store objects.
    Prune {
        #[arg(long)]
        dry_run: bool,
    },

    /// Diagnostics: dirs, mirrors, same-fs, link mode.
    Doctor,
}

#[derive(Debug, Subcommand)]
pub enum SourceCommand {
    /// List sources for a tool (with last-probe results if available).
    List { tool: String },
    /// Probe sources for a tool or model provider now and print the speed ranking.
    Test {
        tool: String,
        /// Target model repository for huggingface/modelscope probes.
        #[arg(long)]
        model: Option<String>,
    },
    /// Add a custom source for a tool.
    Add {
        tool: String,
        /// Unique source id.
        #[arg(long)]
        id: String,
        /// Base URL for archive downloads.
        #[arg(long = "download-url")]
        download_url: String,
        /// Version-index / metadata URL (if different from downloads).
        #[arg(long = "index-url")]
        index_url: Option<String>,
        /// Allow this custom endpoint to receive provider credentials.
        #[arg(long)]
        forward_credentials: bool,
    },
    /// Remove a custom source from a tool.
    Remove { tool: String, id: String },
    /// Pin a tool to a specific source id.
    Pin { tool: String, id: String },
    /// Remove a tool's source pin.
    Unpin { tool: String },
}

#[derive(Debug, Subcommand)]
pub enum RegistryCommand {
    /// Probe dependency registries and show the selection plan.
    Test {
        /// Package manager to test; omit to test all supported managers.
        manager: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AliasCommand {
    /// Set an alias for a tool, e.g. `node default 20`.
    Set {
        tool: String,
        name: String,
        target: String,
    },
    /// List aliases, optionally for one tool.
    List { tool: Option<String> },
    /// Remove an alias.
    Unset { tool: String, name: String },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the resolved config directory / file path.
    Path,
    /// Print resolved settings.
    List,
}

#[derive(Debug, Subcommand)]
pub enum TrustCommand {
    /// List content-bound trusted project configurations.
    List,
}

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// Plan or apply migration of global npm packages between managed Node versions.
    MigratePackages {
        /// Source managed Node version.
        #[arg(long)]
        from: String,
        /// Target managed Node version.
        #[arg(long)]
        to: String,
        /// Apply the plan. Without this flag, no packages are changed.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum PythonCommand {
    /// Find managed, PATH, and system Python interpreters.
    Find {
        /// Optional Python request, e.g. `pypy-3.11` or `3.14+freethreaded`.
        request: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AndroidCommand {
    /// Inspect and record Android SDK license acceptance.
    Licenses {
        #[command(subcommand)]
        command: AndroidLicensesCommand,
    },
    /// Inspect and repair the shared SDK root Google's tools read.
    SdkRoot {
        #[command(subcommand)]
        command: AndroidSdkRootCommand,
    },
    /// Manage Android virtual devices without going through avdmanager.
    Avd {
        #[command(subcommand)]
        command: AndroidAvdCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum AndroidSdkRootCommand {
    /// Print the shared SDK root and what each expected entry resolves to.
    Show,
    /// Rewrite the `package.xml` index for every installed package.
    ///
    /// Google's tools discover packages by parsing that file rather than by
    /// asking a manager, so a package installed before osdk wrote it stays
    /// invisible to `avdmanager` until this runs.
    Repair,
}

#[derive(Debug, Subcommand)]
pub enum AndroidAvdCommand {
    /// List the virtual devices osdk manages.
    List,
    /// Create a virtual device from an installed system image.
    Create {
        /// Name for the new device, e.g. `pixel-35`.
        name: String,
        /// Installed system image, e.g. `android-35;google_apis;x86_64`.
        #[arg(long, value_name = "IMAGE")]
        image: String,
        /// Replace an existing device of the same name.
        #[arg(long)]
        force: bool,
        /// Size of the userdata partition, e.g. `8G`.
        #[arg(long, value_name = "SIZE")]
        data_size: Option<String>,
        /// Size of the emulated SD card, e.g. `512M`.
        #[arg(long, value_name = "SIZE")]
        sdcard_size: Option<String>,
    },
    /// Delete a virtual device.
    Delete {
        /// Name of the device to remove.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum AndroidLicensesCommand {
    /// Print the full agreement text a package requires, without installing.
    Show {
        /// Tool request, e.g. `android-ndk@29.0.14206865`.
        tool: String,
        /// Print only the license id and digest instead of the full text.
        #[arg(long)]
        digest_only: bool,
    },
    /// List which licenses are currently recorded as accepted.
    Status,
    /// Write the recorded acceptances into an SDK root for Gradle to reuse.
    Export {
        /// Destination SDK root; its `licenses/` directory is created.
        #[arg(long, value_name = "DIR")]
        sdk_root: std::path::PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Resolve and download an immutable model snapshot.
    Pull {
        /// Local logical name for the model.
        name: String,
        /// Provider reference, e.g. hf:Qwen/Qwen2.5-7B-Instruct@main.
        reference: String,
        /// Override the provider endpoint.
        #[arg(long)]
        endpoint: Option<String>,
        /// Allow an explicit custom endpoint to receive provider credentials.
        #[arg(long)]
        forward_credentials: bool,
        /// Include files matching a glob (repeatable).
        #[arg(long)]
        include: Vec<String>,
        /// Exclude files matching a glob (repeatable).
        #[arg(long)]
        exclude: Vec<String>,
        /// Optional format or quantization label.
        #[arg(long)]
        variant: Option<String>,
        /// Do not update the nearest project osdk.lock.
        #[arg(long)]
        no_lock: bool,
    },
    /// List locally materialized model snapshots.
    List,
    /// Print the current local snapshot path.
    Path { name: String },
    /// Verify all files in a local snapshot.
    Verify { name: String },
    /// Remove all local snapshots for a logical model name.
    Remove { name: String },
    /// Manage provider environment exported by shell activation.
    Env {
        #[command(subcommand)]
        command: ModelEnvCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ModelEnvCommand {
    /// Persistently export provider endpoint/cache variables.
    Enable {
        /// Provider to enable; omit to enable both.
        provider: Option<osdk_core::model::ProviderId>,
        /// Override provider variables already set by the user.
        #[arg(long)]
        force: bool,
    },
    /// Stop exporting provider variables and restore original values.
    Disable {
        /// Provider to disable; omit to disable both.
        provider: Option<osdk_core::model::ProviderId>,
    },
    /// Show persisted adapter state and the variables it would export.
    List,
}

#[derive(Debug, Subcommand)]
pub enum RustCommand {
    /// Manage installed rustup components.
    Component {
        #[command(subcommand)]
        command: RustItemCommand,
    },
    /// Manage installed rustup targets.
    Target {
        #[command(subcommand)]
        command: RustItemCommand,
    },
    /// Check updates and repair osdk markers from isolated rustup state.
    Check {
        /// Repair stale or missing osdk marker directories.
        #[arg(long)]
        repair: bool,
    },
    /// Explicitly import or export rustup directory overrides.
    Override {
        #[command(subcommand)]
        command: RustOverrideCommand,
    },
    /// Manage linked/custom Rust toolchains.
    Toolchain {
        #[command(subcommand)]
        command: RustToolchainCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum RustItemCommand {
    /// Install a component or target.
    Add {
        name: String,
        #[arg(long, default_value = "stable")]
        toolchain: String,
    },
    /// Uninstall a component or target.
    Remove {
        name: String,
        #[arg(long, default_value = "stable")]
        toolchain: String,
    },
    /// List installed and available components or targets.
    List {
        #[arg(long, default_value = "stable")]
        toolchain: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum RustOverrideCommand {
    /// Import the rustup override for a directory into osdk.toml.
    Import { path: Option<std::path::PathBuf> },
    /// Export the active osdk Rust pin as an explicit rustup override.
    Export { path: Option<std::path::PathBuf> },
}

#[derive(Debug, Subcommand)]
pub enum RustToolchainCommand {
    /// Link a local toolchain path under an osdk/rustup name.
    Link {
        name: String,
        path: std::path::PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Print the shared cache directories.
    Dir,
    /// Print the downstream package-manager cache redirections.
    Env,
    /// Remove downloaded archives (keeps the CAS store + installs).
    Clean,
}

#[derive(Debug, Subcommand)]
pub enum ContainerCommand {
    /// Pull an image through one selected native runtime.
    Pull {
        /// Canonical OCI image reference to pull.
        #[arg(value_name = "IMAGE")]
        image: osdk_core::container::ImageReference,
        /// Runtime selector; defaults to the effective container configuration.
        #[arg(long, value_enum, value_name = "RUNTIME")]
        runtime: Option<ContainerRuntimeArg>,
        /// Optional OCI platform: OS/ARCH[/VARIANT].
        #[arg(long, value_name = "PLATFORM")]
        platform: Option<osdk_core::container::OciPlatform>,
        /// Explicit containerd daemon address; must be paired with --namespace.
        #[arg(long, value_name = "ADDRESS", requires = "namespace")]
        address: Option<String>,
        /// Explicit containerd namespace; must be paired with --address.
        #[arg(long, value_name = "NAMESPACE", requires = "address")]
        namespace: Option<String>,
    },
    /// Preview or execute one narrowly scoped native prune.
    Prune {
        /// Native cache owner to target.
        #[arg(long, value_enum, value_name = "RUNTIME", required = true)]
        runtime: ContainerPruneRuntimeArg,
        /// Exact native state category to prune.
        #[arg(long, value_enum, value_name = "SCOPE", required = true)]
        scope: ContainerPruneScopeArg,
        /// Docker context to discover and bind into the preview.
        #[arg(long, value_name = "NAME")]
        context: Option<String>,
        /// Buildx builder to discover and bind into the preview.
        #[arg(long, value_name = "NAME")]
        builder: Option<osdk_core::container::BuildxBuilderSelector>,
        /// Execute the displayed preview after confirmation.
        #[arg(long, requires = "accept_preview")]
        execute: bool,
        /// Accept exactly this previously displayed preview identity.
        #[arg(long, value_name = "SHA256_ID", requires = "execute")]
        accept_preview: Option<String>,
    },
    /// Diagnose the selected native runtime and Buildx builder.
    Doctor {
        /// Runtime selector; defaults to the effective container configuration.
        #[arg(long, value_enum, value_name = "RUNTIME")]
        runtime: Option<ContainerRuntimeArg>,
        /// Buildx builder name; defaults to the effective container configuration.
        #[arg(long, value_name = "NAME")]
        builder: Option<osdk_core::container::BuildxBuilderSelector>,
        /// Emit deterministic, schema-versioned JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect caches owned by native container components.
    Cache {
        #[command(subcommand)]
        command: ContainerCacheCommand,
    },
    /// Test an OCI registry and its configured or built-in mirrors anonymously.
    Registry {
        #[command(subcommand)]
        command: ContainerRegistryCommand,
    },
    /// Benchmark, plan, and apply native mirror configuration changes.
    Mirrors {
        #[command(subcommand)]
        command: ContainerMirrorsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ContainerCacheCommand {
    /// Report aggregate native cache usage without scanning private stores.
    Status {
        /// Runtime/cache owner; defaults to the effective container configuration.
        #[arg(long, value_enum, value_name = "RUNTIME")]
        runtime: Option<ContainerCacheRuntimeArg>,
        /// Buildx builder name; defaults to the effective container configuration.
        #[arg(long, value_name = "NAME")]
        builder: Option<osdk_core::container::BuildxBuilderSelector>,
        /// Emit deterministic, schema-versioned JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ContainerRegistryCommand {
    /// Test registry API access, an optional image, and configured mirrors.
    Test {
        /// Upstream registry host with an optional port.
        #[arg(value_name = "REGISTRY")]
        registry: String,
        /// Optional OCI image reference for manifest and bounded blob checks.
        #[arg(long, value_name = "IMAGE")]
        image: Option<osdk_core::container::ImageReference>,
        /// Optional OCI platform: OS/ARCH[/VARIANT].
        #[arg(long, value_name = "PLATFORM")]
        platform: Option<osdk_core::container::OciPlatform>,
        /// Emit schema-versioned JSON; live timing fields vary between runs.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ContainerMirrorsCommand {
    /// Plan one registry's native mirror configuration without writing it.
    Plan {
        /// Registry whose configured mirror policy should be planned.
        #[arg(value_name = "REGISTRY")]
        registry: String,
        /// Native control plane to inspect and plan for.
        #[arg(long, value_enum, value_name = "RUNTIME", required = true)]
        runtime: ContainerMirrorRuntimeArg,
        /// Buildx builder name; defaults to the effective container configuration.
        #[arg(long, value_name = "NAME")]
        builder: Option<osdk_core::container::BuildxBuilderSelector>,
        /// Explicit Docker daemon JSON or BuildKit TOML input path.
        #[arg(long, value_name = "PATH")]
        native_config: Option<std::path::PathBuf>,
        /// Explicit main containerd TOML input path when config_path is absent.
        #[arg(long, value_name = "PATH")]
        containerd_main_config: Option<std::path::PathBuf>,
        /// Emit deterministic, schema-versioned JSON.
        #[arg(long)]
        json: bool,
    },
    /// Benchmark mirrors, confirm the resulting plan, and atomically write it.
    Apply {
        /// Registry whose configured or built-in mirror policy should be applied.
        #[arg(value_name = "REGISTRY")]
        registry: String,
        /// Native control plane to inspect and configure.
        #[arg(long, value_enum, value_name = "RUNTIME", required = true)]
        runtime: ContainerMirrorRuntimeArg,
        /// Buildx builder name; defaults to the effective container configuration.
        #[arg(long, value_name = "NAME")]
        builder: Option<osdk_core::container::BuildxBuilderSelector>,
        /// Exact Docker daemon JSON, containerd hosts.toml, or BuildKit TOML path.
        #[arg(long, value_name = "PATH", required = true)]
        native_config: std::path::PathBuf,
        /// Explicit main containerd TOML input path when config_path is absent.
        #[arg(long, value_name = "PATH")]
        containerd_main_config: Option<std::path::PathBuf>,
        /// OCI image used for manifest-equivalence and bounded layer benchmarking.
        #[arg(long, value_name = "IMAGE")]
        image: Option<osdk_core::container::ImageReference>,
        /// Optional OCI platform: OS/ARCH[/VARIANT].
        #[arg(long, value_name = "PLATFORM")]
        platform: Option<osdk_core::container::OciPlatform>,
        /// Required with --yes; must equal the plan generated in this invocation.
        #[arg(long, value_name = "SHA256", conflicts_with = "dry_run")]
        accept_plan: Option<String>,
        /// Benchmark and emit the resulting plan without prompting or writing.
        #[arg(long)]
        dry_run: bool,
        /// Emit schema-versioned JSON; live benchmark timing fields vary.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ContainerRuntimeArg {
    Auto,
    Docker,
    Containerd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ContainerCacheRuntimeArg {
    Auto,
    Docker,
    Containerd,
    Buildkit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ContainerMirrorRuntimeArg {
    Docker,
    Containerd,
    Buildkit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ContainerPruneRuntimeArg {
    Docker,
    Buildkit,
    Containerd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ContainerPruneScopeArg {
    Images,
    BuildCache,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_container_registry_test_arguments() {
        let cli = Cli::try_parse_from([
            "osdk",
            "container",
            "registry",
            "test",
            "docker.io",
            "--image",
            "ubuntu:24.04",
            "--platform",
            "linux/x86_64",
            "--json",
        ])
        .unwrap();
        let Command::Container {
            command:
                ContainerCommand::Registry {
                    command:
                        ContainerRegistryCommand::Test {
                            registry,
                            image,
                            platform,
                            json,
                        },
                },
        } = cli.command
        else {
            panic!("unexpected command");
        };
        assert_eq!(registry, "docker.io");
        assert_eq!(image.unwrap().to_string(), "docker.io/library/ubuntu:24.04");
        assert_eq!(platform.unwrap().to_string(), "linux/amd64");
        assert!(json);
    }

    #[test]
    fn parses_native_pull_and_narrow_prune_arguments() {
        let pull = Cli::try_parse_from([
            "osdk",
            "container",
            "pull",
            "ubuntu:24.04",
            "--runtime",
            "auto",
            "--platform",
            "Linux/X64",
        ])
        .unwrap();
        let Command::Container {
            command:
                ContainerCommand::Pull {
                    image,
                    runtime,
                    platform,
                    address,
                    namespace,
                },
        } = pull.command
        else {
            panic!("unexpected command");
        };
        assert_eq!(image.to_string(), "docker.io/library/ubuntu:24.04");
        assert_eq!(runtime, Some(ContainerRuntimeArg::Auto));
        assert_eq!(platform.unwrap().to_string(), "linux/amd64");
        assert!(address.is_none());
        assert!(namespace.is_none());

        let prune = Cli::try_parse_from([
            "osdk",
            "container",
            "prune",
            "--runtime",
            "docker",
            "--scope",
            "images",
            "--context",
            "team",
        ])
        .unwrap();
        let Command::Container {
            command:
                ContainerCommand::Prune {
                    runtime,
                    scope,
                    context,
                    execute,
                    accept_preview,
                    ..
                },
        } = prune.command
        else {
            panic!("unexpected command");
        };
        assert_eq!(runtime, ContainerPruneRuntimeArg::Docker);
        assert_eq!(scope, ContainerPruneScopeArg::Images);
        assert_eq!(context.as_deref(), Some("team"));
        assert!(!execute);
        assert!(accept_preview.is_none());
    }

    #[test]
    fn containerd_pull_selectors_are_paired() {
        for arguments in [
            vec![
                "osdk",
                "container",
                "pull",
                "alpine:3",
                "--runtime",
                "containerd",
                "--address",
                "unix:///run/containerd/containerd.sock",
            ],
            vec![
                "osdk",
                "container",
                "pull",
                "alpine:3",
                "--runtime",
                "containerd",
                "--namespace",
                "default",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn native_prune_execution_requires_an_accepted_preview_id() {
        let missing = Cli::try_parse_from([
            "osdk",
            "container",
            "prune",
            "--runtime",
            "docker",
            "--scope",
            "images",
            "--execute",
        ])
        .unwrap_err();
        assert!(missing.to_string().contains("--accept-preview"));

        let detached = Cli::try_parse_from([
            "osdk",
            "container",
            "prune",
            "--runtime",
            "buildkit",
            "--scope",
            "build-cache",
            "--accept-preview",
            "sha256:abc",
        ])
        .unwrap_err();
        assert!(detached.to_string().contains("--execute"));
    }

    #[test]
    fn mirror_plan_requires_an_explicit_runtime() {
        let error =
            Cli::try_parse_from(["osdk", "container", "mirrors", "plan", "docker.io"]).unwrap_err();
        assert!(error.to_string().contains("--runtime"));

        let cli = Cli::try_parse_from([
            "osdk",
            "container",
            "mirrors",
            "plan",
            "docker.io",
            "--runtime",
            "buildkit",
            "--builder",
            "team-builder",
            "--native-config",
            "buildkitd.toml",
            "--json",
        ])
        .unwrap();
        let Command::Container {
            command:
                ContainerCommand::Mirrors {
                    command:
                        ContainerMirrorsCommand::Plan {
                            registry,
                            runtime,
                            builder,
                            native_config,
                            json,
                            ..
                        },
                },
        } = cli.command
        else {
            panic!("unexpected command");
        };
        assert_eq!(registry, "docker.io");
        assert_eq!(runtime, ContainerMirrorRuntimeArg::Buildkit);
        assert_eq!(builder.unwrap().to_string(), "team-builder");
        assert_eq!(
            native_config.unwrap(),
            std::path::PathBuf::from("buildkitd.toml")
        );
        assert!(json);
    }

    #[test]
    fn parses_unattended_mirror_apply_arguments_and_allows_interactive_apply() {
        let cli = Cli::try_parse_from([
            "osdk",
            "--yes",
            "container",
            "mirrors",
            "apply",
            "docker.io",
            "--runtime",
            "docker",
            "--native-config",
            "daemon.json",
            "--accept-plan",
            "sha256:example",
            "--json",
        ])
        .unwrap();
        let Command::Container {
            command:
                ContainerCommand::Mirrors {
                    command:
                        ContainerMirrorsCommand::Apply {
                            registry,
                            runtime,
                            native_config,
                            accept_plan,
                            dry_run,
                            json,
                            ..
                        },
                },
        } = cli.command
        else {
            panic!("unexpected command");
        };
        assert_eq!(registry, "docker.io");
        assert_eq!(runtime, ContainerMirrorRuntimeArg::Docker);
        assert_eq!(native_config, std::path::PathBuf::from("daemon.json"));
        assert_eq!(accept_plan.as_deref(), Some("sha256:example"));
        assert!(!dry_run);
        assert!(json);

        let cli = Cli::try_parse_from([
            "osdk",
            "container",
            "mirrors",
            "apply",
            "docker.io",
            "--runtime",
            "docker",
            "--native-config",
            "daemon.json",
        ])
        .unwrap();
        assert!(!cli.global.yes);
        let Command::Container {
            command:
                ContainerCommand::Mirrors {
                    command: ContainerMirrorsCommand::Apply { accept_plan, .. },
                },
        } = cli.command
        else {
            panic!("unexpected command");
        };
        assert!(accept_plan.is_none());
    }
}
