use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
};

use clap::{Parser, ValueEnum};

const INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/Thyssenkrupp234/roblox-obfuscator/main/install.sh";

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
    about = "Obfuscate Roblox RBXL/RBXM script sources with Prometheus",
    after_help = "Commands:\n  update    Update rbx-obfuscator and managed dependencies"
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

#[derive(Debug, Parser)]
#[command(
    name = "rbx-obfuscator update",
    about = "Update rbx-obfuscator and managed dependencies"
)]
struct UpdateCli {
    /// Show installer command output.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    if let Some(update_cli) = parse_update_command(std::env::args_os()) {
        return run_update(update_cli);
    }

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

fn parse_update_command<I>(args: I) -> Option<UpdateCli>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    args.next()?;
    let first_arg = args.next()?;
    if first_arg != "update" {
        return None;
    }

    let update_args = std::iter::once(OsString::from("rbx-obfuscator update")).chain(args);
    Some(UpdateCli::parse_from(update_args))
}

fn run_update(update_cli: UpdateCli) -> anyhow::Result<()> {
    let mut installer = Command::new("sh");
    installer.arg("-c");
    if update_cli.verbose {
        installer.arg(format!(
            "curl -fsSL '{INSTALL_SCRIPT_URL}' | sh -s -- --verbose"
        ));
    } else {
        installer.arg(format!("curl -fsSL '{INSTALL_SCRIPT_URL}' | sh"));
    }
    installer
        .stdin(Stdio::null())
        .status()
        .map_err(anyhow::Error::from)
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                anyhow::bail!("update installer exited with status {status}")
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_is_required() {
        assert!(Cli::try_parse_from(["rbx-obfuscator", "input.rbxl"]).is_err());
    }

    #[test]
    fn output_is_optional() {
        let cli =
            Cli::try_parse_from(["rbx-obfuscator", "input.rbxm", "--level", "medium"]).unwrap();

        assert_eq!(cli.input, PathBuf::from("input.rbxm"));
        assert_eq!(cli.level, CliObfuscationLevel::Medium);
        assert_eq!(cli.output, None);
    }

    #[test]
    fn output_flag_is_supported() {
        let cli = Cli::try_parse_from([
            "rbx-obfuscator",
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
            "rbx-obfuscator",
            "input.rbxl",
            "--level",
            "minimal",
            "--strip-types",
        ])
        .unwrap();

        assert!(cli.strip_types);
    }

    #[test]
    fn update_command_is_detected() {
        let cli = parse_update_command(["rbx-obfuscator", "update"].map(OsString::from)).unwrap();

        assert!(!cli.verbose);
    }

    #[test]
    fn update_command_accepts_verbose() {
        let cli =
            parse_update_command(["rbx-obfuscator", "update", "--verbose"].map(OsString::from))
                .unwrap();

        assert!(cli.verbose);
    }

    #[test]
    fn non_update_command_uses_obfuscation_cli() {
        assert!(parse_update_command(
            ["rbx-obfuscator", "input.rbxl", "--level", "minimal"].map(OsString::from)
        )
        .is_none());
    }
}
