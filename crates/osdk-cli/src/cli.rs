//! Clap command tree for the `osdk` binary.

use clap::{Parser, Subcommand};

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

    /// Set the active version (installs if needed) and write a pin.
    #[command(alias = "u")]
    Use {
        /// e.g. `node@20`.
        tool: String,
        /// Write the pin to the user global config instead of the project.
        #[arg(short, long)]
        global: bool,
        /// Backend-specific option as key=value (repeatable). See `install`.
        #[arg(short = 'o', long = "opt", value_name = "KEY=VALUE")]
        opts: Vec<String>,
    },

    /// Uninstall a tool version.
    #[command(alias = "rm")]
    Uninstall {
        /// e.g. `node@20.11.1`.
        tool: String,
    },

    /// Show the active version of each tool for the current directory.
    Current { tool: Option<String> },

    /// Print the install directory of a tool version.
    Where {
        /// e.g. `node` or `node@20.11.1`.
        tool: String,
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
