use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
};

use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};

mod tui;

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
    name = "rbx-obfuscator",
    author,
    version,
    about = "Obfuscate Roblox RBXL/RBXM script sources with Prometheus",
    after_help = "Commands:\n  rbx-obfuscator                         Open the interactive wizard\n  rbx-obfuscator obfuscate <in> <out>    Explicit obfuscation mode\n  rbx-obfuscator extract <in> <folder>   Extract scripts, GUI JSON, instances, and content refs\n  rbx-obfuscator update                  Update the CLI and managed dependencies"
)]
struct ObfuscateCli {
    /// Input .rbxl, .rbxm, .rbxlx, or .rbxmx file.
    input: PathBuf,

    /// Optional positional output file for compatibility with older usage.
    output_positional: Option<PathBuf>,

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

    /// Show detailed script and Prometheus logs.
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "rbx-obfuscator extract",
    about = "Extract RBXL/RBXM components without obfuscating"
)]
struct ExtractCli {
    /// Input .rbxl, .rbxm, .rbxlx, or .rbxmx file.
    input: PathBuf,

    /// Output folder for extracted components.
    output_folder: PathBuf,

    /// Show detailed extraction logs.
    #[arg(short, long)]
    verbose: bool,
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

#[derive(Debug)]
enum AppMode {
    Wizard,
    Obfuscate(ObfuscateCli),
    Extract(ExtractCli),
    Update(UpdateCli),
}

fn main() -> Result<()> {
    let mode = match parse_app_mode(std::env::args_os()) {
        Ok(mode) => mode,
        Err(error) => error.exit(),
    };

    match mode {
        AppMode::Wizard => tui::run_wizard(),
        AppMode::Obfuscate(cli) => rbxl_obfuscate::run(cli.into_options()?),
        AppMode::Extract(cli) => {
            rbxl_obfuscate::extract::run(rbxl_obfuscate::extract::ExtractOptions {
                input: cli.input,
                output_folder: cli.output_folder,
                verbose: cli.verbose,
            })
            .map(|_| ())
        }
        AppMode::Update(update_cli) => run_update(update_cli),
    }
}

fn parse_app_mode<I>(args: I) -> std::result::Result<AppMode, clap::Error>
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    if args.len() == 1 {
        return Ok(AppMode::Wizard);
    }

    let first_arg = args.get(1).and_then(|arg| arg.to_str()).unwrap_or_default();

    match first_arg {
        "update" => {
            let update_args = std::iter::once(OsString::from("rbx-obfuscator update"))
                .chain(args.into_iter().skip(2));
            UpdateCli::try_parse_from(update_args).map(AppMode::Update)
        }
        "extract" => {
            let extract_args = std::iter::once(OsString::from("rbx-obfuscator extract"))
                .chain(args.into_iter().skip(2));
            ExtractCli::try_parse_from(extract_args).map(AppMode::Extract)
        }
        "obfuscate" => {
            let obfuscate_args = std::iter::once(OsString::from("rbx-obfuscator obfuscate"))
                .chain(args.into_iter().skip(2));
            ObfuscateCli::try_parse_from(obfuscate_args).map(AppMode::Obfuscate)
        }
        _ => ObfuscateCli::try_parse_from(args).map(AppMode::Obfuscate),
    }
}

impl ObfuscateCli {
    fn into_options(self) -> Result<rbxl_obfuscate::Options> {
        if self.output_positional.is_some() && self.output.is_some() {
            bail!("provide either positional output or --output, not both");
        }

        Ok(rbxl_obfuscate::Options {
            input: self.input,
            output: self.output.or(self.output_positional),
            obfuscation_level: self.level.into(),
            dry_run: self.dry_run,
            strip_types: self.strip_types,
            verbose: self.verbose,
            backup_dir: self.backup_dir,
            skip_paths: self.skip_path,
            manifest: self.manifest,
        })
    }
}

fn run_update(update_cli: UpdateCli) -> Result<()> {
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
    fn no_args_selects_wizard() {
        assert!(matches!(
            parse_app_mode(["rbx-obfuscator"].map(OsString::from)).unwrap(),
            AppMode::Wizard
        ));
    }

    #[test]
    fn level_is_required() {
        assert!(parse_app_mode(["rbx-obfuscator", "input.rbxl"].map(OsString::from)).is_err());
    }

    #[test]
    fn output_is_optional() {
        let AppMode::Obfuscate(cli) = parse_app_mode(
            ["rbx-obfuscator", "input.rbxm", "--level", "medium"].map(OsString::from),
        )
        .unwrap() else {
            panic!("expected obfuscation mode");
        };

        assert_eq!(cli.input, PathBuf::from("input.rbxm"));
        assert_eq!(cli.level, CliObfuscationLevel::Medium);
        assert_eq!(cli.output, None);
        assert_eq!(cli.output_positional, None);
    }

    #[test]
    fn output_flag_is_supported() {
        let AppMode::Obfuscate(cli) = parse_app_mode(
            [
                "rbx-obfuscator",
                "input.rbxl",
                "--level",
                "high",
                "--output",
                "output.rbxl",
            ]
            .map(OsString::from),
        )
        .unwrap() else {
            panic!("expected obfuscation mode");
        };

        assert_eq!(cli.output, Some(PathBuf::from("output.rbxl")));
    }

    #[test]
    fn positional_output_is_supported() {
        let AppMode::Obfuscate(cli) = parse_app_mode(
            [
                "rbx-obfuscator",
                "input.rbxl",
                "output.rbxl",
                "--level",
                "high",
            ]
            .map(OsString::from),
        )
        .unwrap() else {
            panic!("expected obfuscation mode");
        };

        assert_eq!(cli.output_positional, Some(PathBuf::from("output.rbxl")));
    }

    #[test]
    fn explicit_obfuscate_subcommand_is_supported() {
        let AppMode::Obfuscate(cli) = parse_app_mode(
            [
                "rbx-obfuscator",
                "obfuscate",
                "input.rbxl",
                "output.rbxl",
                "--level",
                "minimal",
                "--dry-run",
            ]
            .map(OsString::from),
        )
        .unwrap() else {
            panic!("expected obfuscation mode");
        };

        assert_eq!(cli.output_positional, Some(PathBuf::from("output.rbxl")));
        assert!(cli.dry_run);
    }

    #[test]
    fn extract_command_is_supported() {
        let AppMode::Extract(cli) = parse_app_mode(
            ["rbx-obfuscator", "extract", "input.rbxl", "output-folder"].map(OsString::from),
        )
        .unwrap() else {
            panic!("expected extract mode");
        };

        assert_eq!(cli.input, PathBuf::from("input.rbxl"));
        assert_eq!(cli.output_folder, PathBuf::from("output-folder"));
    }

    #[test]
    fn strip_types_and_verbose_flags_are_supported() {
        let AppMode::Obfuscate(cli) = parse_app_mode(
            [
                "rbx-obfuscator",
                "input.rbxl",
                "--level",
                "minimal",
                "--strip-types",
                "--verbose",
            ]
            .map(OsString::from),
        )
        .unwrap() else {
            panic!("expected obfuscation mode");
        };

        assert!(cli.strip_types);
        assert!(cli.verbose);
    }

    #[test]
    fn update_command_is_detected() {
        let AppMode::Update(cli) =
            parse_app_mode(["rbx-obfuscator", "update"].map(OsString::from)).unwrap()
        else {
            panic!("expected update mode");
        };

        assert!(!cli.verbose);
    }

    #[test]
    fn update_command_accepts_verbose() {
        let AppMode::Update(cli) =
            parse_app_mode(["rbx-obfuscator", "update", "--verbose"].map(OsString::from)).unwrap()
        else {
            panic!("expected update mode");
        };

        assert!(cli.verbose);
    }
}
