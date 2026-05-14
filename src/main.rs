use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliObfuscationLevel {
    Minimal,
    Low,
    Medium,
    High,
}

impl From<CliObfuscationLevel> for rbxl_obfuscate::ObfuscationLevel {
    fn from(level: CliObfuscationLevel) -> Self {
        match level {
            CliObfuscationLevel::Minimal => Self::Minimal,
            CliObfuscationLevel::Low => Self::Low,
            CliObfuscationLevel::Medium => Self::Medium,
            CliObfuscationLevel::High => Self::High,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Obfuscate Roblox RBXL script sources with Prometheus"
)]
struct Cli {
    /// Input .rbxl file.
    input: PathBuf,

    /// Output .rbxl file. Must not be the same path as input.
    output: PathBuf,

    /// Deprecated compatibility flag; Prometheus uses --level presets and ignores custom configs.
    #[arg(long)]
    darklua_config: Option<PathBuf>,

    /// Built-in obfuscation level to map to a Prometheus preset. Defaults to minimal.
    #[arg(long, value_enum)]
    level: Option<CliObfuscationLevel>,

    /// Report what would be processed without running Prometheus or writing output.
    #[arg(long)]
    dry_run: bool,

    /// Directory where original script sources should be written.
    #[arg(long)]
    backup_dir: Option<PathBuf>,

    /// Exact normalized Roblox instance path to skip, such as game.ServerScriptService.Main.
    #[arg(long)]
    skip_path: Vec<String>,

    /// Path to write a JSON processing manifest.
    #[arg(long)]
    manifest: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.darklua_config.is_some() && cli.level.is_some() {
        anyhow::bail!("--level cannot be combined with --darklua-config");
    }

    if cli.darklua_config.is_some() {
        eprintln!("Ignoring --darklua-config; the Prometheus backend uses --level presets.");
    }

    rbxl_obfuscate::run(rbxl_obfuscate::Options {
        input: cli.input,
        output: cli.output,
        obfuscation_level: cli.level.map(Into::into).unwrap_or_default(),
        dry_run: cli.dry_run,
        backup_dir: cli.backup_dir,
        skip_paths: cli.skip_path,
        manifest: cli.manifest,
    })
}
