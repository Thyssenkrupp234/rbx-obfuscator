use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
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
    about = "Obfuscate Roblox RBXL/RBXM script sources with Prometheus"
)]
struct Cli {
    /// Input .rbxl or .rbxm file.
    input: PathBuf,

    /// Output file. Defaults to <input-stem>-obfuscated_<Level>.<extension>.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Built-in obfuscation level to map to a Prometheus preset.
    #[arg(long, value_enum)]
    level: CliObfuscationLevel,

    /// Report what would be processed without running Prometheus or writing output.
    #[arg(long)]
    dry_run: bool,

    /// Strip Luau type annotations before every Prometheus run.
    #[arg(long)]
    strip_types: bool,

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

    rbxl_obfuscate::run(rbxl_obfuscate::Options {
        input: cli.input,
        output: cli.output,
        obfuscation_level: cli.level.into(),
        dry_run: cli.dry_run,
        strip_types: cli.strip_types,
        backup_dir: cli.backup_dir,
        skip_paths: cli.skip_path,
        manifest: cli.manifest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_is_required() {
        assert!(Cli::try_parse_from(["rbxl-obfuscate", "input.rbxl"]).is_err());
    }

    #[test]
    fn output_is_optional() {
        let cli =
            Cli::try_parse_from(["rbxl-obfuscate", "input.rbxm", "--level", "medium"]).unwrap();

        assert_eq!(cli.input, PathBuf::from("input.rbxm"));
        assert_eq!(cli.level, CliObfuscationLevel::Medium);
        assert_eq!(cli.output, None);
    }

    #[test]
    fn output_flag_is_supported() {
        let cli = Cli::try_parse_from([
            "rbxl-obfuscate",
            "input.rbxl",
            "--level",
            "high",
            "--output",
            "output.rbxl",
        ])
        .unwrap();

        assert_eq!(cli.output, Some(PathBuf::from("output.rbxl")));
    }

    #[test]
    fn strip_types_flag_is_supported() {
        let cli = Cli::try_parse_from([
            "rbxl-obfuscate",
            "input.rbxl",
            "--level",
            "minimal",
            "--strip-types",
        ])
        .unwrap();

        assert!(cli.strip_types);
    }
}
