use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
};

use anyhow::Result;
use clap::{error::ErrorKind, CommandFactory, Parser, Subcommand, ValueEnum};

mod tui;

const INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/Thyssenkrupp234/rbx-obfuscator/main/install.sh";

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
    about = "Obfuscate Roblox files or extract readable project components",
    long_about = "Run without a command to open the interactive wizard.\n\nUse `rbx-obfuscator obfuscate` for Prometheus obfuscation.\nUse `rbx-obfuscator extract` for component extraction.",
    after_help = "Required Obfuscation Flags:\n  --level <minimal|low|medium|high>    Required for `obfuscate`; values are case-insensitive\n\nExamples:\n  rbx-obfuscator\n  rbx-obfuscator obfuscate /path/to/game.rbxl --level high --output ~/game-obfuscated.rbxl\n  rbx-obfuscator extract /path/to/game.rbxl --output ./game-components\n  rbx-obfuscator update"
)]
struct Cli {
    /// Output path for obfuscate or extract. Can also be passed after those commands.
    #[arg(short, long, value_name = "OUTPUT")]
    output: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Obfuscate Script, LocalScript, and ModuleScript sources with Prometheus.
    Obfuscate(ObfuscateCli),

    /// Extract scripts, GUI JSON, instance hierarchy, and content references.
    Extract(ExtractCli),

    /// Update the CLI and managed dependencies.
    Update(UpdateCli),
}

#[derive(Debug, Parser)]
#[command(
    name = "rbx-obfuscator obfuscate",
    about = "Obfuscate Roblox script sources with Prometheus",
    after_help = "Example:\n  rbx-obfuscator obfuscate /Users/lincolnmuller/Documents/train\\ game.rbxl --level high --output ~/train_game_obfuscated_high.rbxl"
)]
struct ObfuscateCli {
    /// Input .rbxl, .rbxm, .rbxlx, or .rbxmx file to obfuscate.
    #[arg(value_name = "INPUT")]
    input: PathBuf,

    /// Output file. Defaults to <input-stem>-obfuscated_<Level>.<extension>.
    #[arg(short, long, value_name = "OUTPUT")]
    output: Option<PathBuf>,

    /// Required obfuscation level. Values are case-insensitive: minimal, low, medium, high.
    #[arg(
        long,
        value_enum,
        ignore_case = true,
        value_name = "LEVEL",
        help_heading = "Required Obfuscation Flags"
    )]
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
    about = "Extract RBXL/RBXM components without obfuscating",
    after_help = "Examples:\n  rbx-obfuscator extract /Users/lincolnmuller/Documents/train\\ game.rbxl\n  rbx-obfuscator extract /Users/lincolnmuller/Documents/train\\ game.rbxl --output ~/train_game_components\n\nWhen --output is omitted, extraction creates a folder next to INPUT using the input file name without its extension."
)]
struct ExtractCli {
    /// Input .rbxl, .rbxm, .rbxlx, or .rbxmx file to deconstruct.
    #[arg(value_name = "INPUT")]
    input: PathBuf,

    /// Output folder for extracted components. Defaults to a sibling folder named after INPUT.
    #[arg(value_name = "OUTPUT_FOLDER")]
    output_folder: Option<PathBuf>,

    /// Output folder for extracted components. Defaults to a sibling folder named after INPUT.
    #[arg(short, long, value_name = "OUTPUT")]
    output: Option<PathBuf>,

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
    Obfuscate(ObfuscateCli, Option<PathBuf>),
    Extract(ExtractCli, Option<PathBuf>),
    Update(UpdateCli),
}

fn main() -> Result<()> {
    let mode = match parse_app_mode(std::env::args_os()) {
        Ok(mode) => mode,
        Err(error) => error.exit(),
    };

    match mode {
        AppMode::Wizard => tui::run_wizard(),
        AppMode::Obfuscate(cli, output) => tui::run_obfuscation(cli.into_options(output)?),
        AppMode::Extract(cli, output) => tui::run_extraction(cli.into_options(output)?),
        AppMode::Update(update_cli) => run_update(update_cli),
    }
}

fn parse_app_mode<I>(args: I) -> std::result::Result<AppMode, clap::Error>
where
    I: IntoIterator<Item = OsString>,
{
    let cli = Cli::try_parse_from(args)?;
    match cli.command {
        Some(CliCommand::Obfuscate(mut command)) => {
            let output = resolve_output(cli.output, command.output.take())?;
            Ok(AppMode::Obfuscate(command, output))
        }
        Some(CliCommand::Extract(mut command)) => {
            let output = resolve_output(cli.output, command.output.take())?;
            Ok(AppMode::Extract(command, output))
        }
        Some(CliCommand::Update(command)) => {
            if cli.output.is_some() {
                Err(Cli::command().error(
                    ErrorKind::ArgumentConflict,
                    "--output cannot be used with update",
                ))
            } else {
                Ok(AppMode::Update(command))
            }
        }
        None => {
            if cli.output.is_some() {
                Err(Cli::command().error(
                    ErrorKind::ArgumentConflict,
                    "--output requires `obfuscate` or `extract`",
                ))
            } else {
                Ok(AppMode::Wizard)
            }
        }
    }
}

fn resolve_output(
    root_output: Option<PathBuf>,
    command_output: Option<PathBuf>,
) -> std::result::Result<Option<PathBuf>, clap::Error> {
    match (root_output, command_output) {
        (Some(_), Some(_)) => Err(Cli::command().error(
            ErrorKind::ArgumentConflict,
            "use --output either before or after the command, not both",
        )),
        (Some(output), None) | (None, Some(output)) => Ok(Some(output)),
        (None, None) => Ok(None),
    }
}

impl ExtractCli {
    fn into_options(
        self,
        output: Option<PathBuf>,
    ) -> Result<rbxl_obfuscate::extract::ExtractOptions> {
        let output_folder = match (self.output_folder, output) {
            (Some(_), Some(_)) => {
                anyhow::bail!("use either positional OUTPUT_FOLDER or --output, not both")
            }
            (Some(output_folder), None) | (None, Some(output_folder)) => output_folder,
            (None, None) => rbxl_obfuscate::extract::default_output_folder(&self.input)?,
        };

        Ok(rbxl_obfuscate::extract::ExtractOptions {
            input: self.input,
            output_folder,
            verbose: self.verbose,
        })
    }
}

impl ObfuscateCli {
    fn into_options(self, output: Option<PathBuf>) -> Result<rbxl_obfuscate::Options> {
        Ok(rbxl_obfuscate::Options {
            input: self.input,
            output,
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
    fn direct_input_without_command_is_rejected() {
        assert!(parse_app_mode(["rbx-obfuscator", "input.rbxl"].map(OsString::from)).is_err());
    }

    #[test]
    fn obfuscate_requires_level() {
        assert!(
            parse_app_mode(["rbx-obfuscator", "obfuscate", "input.rbxl"].map(OsString::from))
                .is_err()
        );
    }

    #[test]
    fn obfuscate_output_is_optional() {
        let AppMode::Obfuscate(cli, output) = parse_app_mode(
            [
                "rbx-obfuscator",
                "obfuscate",
                "input.rbxm",
                "--level",
                "medium",
            ]
            .map(OsString::from),
        )
        .unwrap() else {
            panic!("expected obfuscation mode");
        };

        assert_eq!(cli.input, PathBuf::from("input.rbxm"));
        assert_eq!(cli.level, CliObfuscationLevel::Medium);
        assert_eq!(output, None);
    }

    #[test]
    fn obfuscate_output_flag_is_supported() {
        let AppMode::Obfuscate(_, output) = parse_app_mode(
            [
                "rbx-obfuscator",
                "obfuscate",
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

        assert_eq!(output, Some(PathBuf::from("output.rbxl")));
    }

    #[test]
    fn output_flag_can_be_passed_before_command() {
        let AppMode::Obfuscate(_, output) = parse_app_mode(
            [
                "rbx-obfuscator",
                "--output",
                "output.rbxl",
                "obfuscate",
                "input.rbxl",
                "--level",
                "high",
            ]
            .map(OsString::from),
        )
        .unwrap() else {
            panic!("expected obfuscation mode");
        };

        assert_eq!(output, Some(PathBuf::from("output.rbxl")));
    }

    #[test]
    fn obfuscate_level_accepts_mixed_case_values() {
        let AppMode::Obfuscate(high_cli, _) = parse_app_mode(
            [
                "rbx-obfuscator",
                "obfuscate",
                "input.rbxl",
                "--level",
                "High",
            ]
            .map(OsString::from),
        )
        .unwrap() else {
            panic!("expected obfuscation mode");
        };
        let AppMode::Obfuscate(medium_cli, _) = parse_app_mode(
            [
                "rbx-obfuscator",
                "obfuscate",
                "input.rbxl",
                "--level",
                "Medium",
            ]
            .map(OsString::from),
        )
        .unwrap() else {
            panic!("expected obfuscation mode");
        };

        assert_eq!(high_cli.level, CliObfuscationLevel::High);
        assert_eq!(medium_cli.level, CliObfuscationLevel::Medium);
    }

    #[test]
    fn obfuscate_positional_output_is_rejected() {
        assert!(parse_app_mode(
            [
                "rbx-obfuscator",
                "obfuscate",
                "input.rbxl",
                "output.rbxl",
                "--level",
                "high",
            ]
            .map(OsString::from),
        )
        .is_err());
    }

    #[test]
    fn explicit_obfuscate_subcommand_is_supported() {
        let AppMode::Obfuscate(cli, output) = parse_app_mode(
            [
                "rbx-obfuscator",
                "obfuscate",
                "input.rbxl",
                "--level",
                "minimal",
                "--output",
                "output.rbxl",
                "--dry-run",
            ]
            .map(OsString::from),
        )
        .unwrap() else {
            panic!("expected obfuscation mode");
        };

        assert_eq!(output, Some(PathBuf::from("output.rbxl")));
        assert!(cli.dry_run);
    }

    #[test]
    fn extract_command_is_supported() {
        let AppMode::Extract(cli, output) = parse_app_mode(
            ["rbx-obfuscator", "extract", "input.rbxl", "output-folder"].map(OsString::from),
        )
        .unwrap() else {
            panic!("expected extract mode");
        };

        assert_eq!(cli.input, PathBuf::from("input.rbxl"));
        assert_eq!(cli.output_folder, Some(PathBuf::from("output-folder")));
        assert_eq!(output, None);
    }

    #[test]
    fn extract_output_flag_is_supported() {
        let AppMode::Extract(cli, output) = parse_app_mode(
            [
                "rbx-obfuscator",
                "extract",
                "input.rbxl",
                "--output",
                "output-folder",
            ]
            .map(OsString::from),
        )
        .unwrap() else {
            panic!("expected extract mode");
        };

        assert_eq!(cli.input, PathBuf::from("input.rbxl"));
        assert_eq!(cli.output_folder, None);
        assert_eq!(output, Some(PathBuf::from("output-folder")));

        let options = cli.into_options(output).unwrap();
        assert_eq!(options.output_folder, PathBuf::from("output-folder"));
    }

    #[test]
    fn extract_output_folder_defaults_to_input_stem() {
        let AppMode::Extract(cli, output) = parse_app_mode(
            ["rbx-obfuscator", "extract", "projects/train game.rbxl"].map(OsString::from),
        )
        .unwrap() else {
            panic!("expected extract mode");
        };

        assert_eq!(cli.output_folder, None);
        assert_eq!(output, None);
        let options = cli.into_options(output).unwrap();
        assert_eq!(options.output_folder, PathBuf::from("projects/train game"));
    }

    #[test]
    fn strip_types_and_verbose_flags_are_supported() {
        let AppMode::Obfuscate(cli, _) = parse_app_mode(
            [
                "rbx-obfuscator",
                "obfuscate",
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
