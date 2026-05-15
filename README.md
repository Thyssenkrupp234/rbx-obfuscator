# rbxl-obfuscate

`rbxl-obfuscate` is a Roblox binary release tool for place and model files. It reads `.rbxl` place files and `.rbxm` model files, finds every `Script`, `LocalScript`, and `ModuleScript`, runs each Luau `Source` property through Prometheus, replaces the source in the DOM, and writes a new Roblox binary file.

It does not use Rojo and does not require Roblox Studio for the normal workflow.

## Setup

Install the latest version from GitHub:

```bash
curl -fsSL https://raw.githubusercontent.com/Thyssenkrupp234/roblox-obfuscator/main/install.sh | sh
```

The installer:

- downloads or updates this repository from GitHub under `~/.rbxl-obfuscate/source`
- installs Rust with `rustup` if `cargo` is missing
- installs or updates Prometheus
- builds the release binary
- installs `rbxl-obfuscate` to `~/.local/bin`
- adds `~/.local/bin` to `PATH` in your shell profile

Because this repository is private, the one-line installer requires GitHub access to the raw script URL. If you already have a checkout, you can also run:

```bash
./install.sh
```

Installer settings can be overridden with environment variables:

```bash
RBXL_OBFUSCATE_REPO_URL=https://github.com/Thyssenkrupp234/roblox-obfuscator.git
RBXL_OBFUSCATE_BRANCH=main
RBXL_OBFUSCATE_INSTALL_ROOT="$HOME/.rbxl-obfuscate"
RBXL_OBFUSCATE_BIN_DIR="$HOME/.local/bin"
```

Manual build:

```bash
cargo build --release
```

Prometheus is a direct runtime dependency. The CLI looks for `prometheus-lua` on `PATH`. If it is missing during a real run, the CLI installs it with the official Prometheus installer.

The tool also records the last successful Prometheus install or update under the user's state directory. If that timestamp is older than one week and an internet connection is available, it runs `prometheus-lua update`; if that fails, it retries the installer command. If there is no internet connection, the update is skipped and the existing Prometheus installation is used.

Dry-run mode does not install, update, or launch Prometheus.

## Usage

Obfuscate a place file and write the default output next to the input:

```bash
rbxl-obfuscate game.rbxl --level medium
```

For `game.rbxl`, that writes:

```text
game-obfuscated_Medium.rbxl
```

Obfuscate a model file with an explicit output path:

```bash
rbxl-obfuscate input.rbxm --level high --output output.rbxm
```

Options:

- `--level <minimal|low|medium|high>`: required obfuscation complexity. Maps to a Prometheus preset.
- `--output <path>`, `-o <path>`: output `.rbxl` or `.rbxm` path. Defaults to `<input-stem>-obfuscated_<Level>.<extension>`.
- `--dry-run`: reports scripts that would be processed and the Prometheus preset that would be used, without running Prometheus or writing output.
- `--backup-dir <dir>`: writes original script sources as `.luau` files before replacement.
- `--skip-path <path>`: skips an exact normalized Roblox instance path, such as `game.ServerScriptService.Main`. Can be passed more than once.
- `--manifest <path>`: writes a JSON report of processed, skipped, failed, or dry-run scripts.

The tool accepts only `.rbxl` and `.rbxm` inputs and refuses to write the output path when it resolves to the same file as the input.

If Prometheus fails to process a script, that script is left unchanged in the output file. The run continues, and all failed script paths are printed at the end.

Prometheus preset mapping:

- `minimal`: `Weak`
- `low`: `Weak`
- `medium`: `Medium`
- `high`: `Strong`

## Test Strategy

Run unit tests:

```bash
cargo test
```

Recommended local validation:

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
rbxl-obfuscate game.rbxl --level medium --dry-run --manifest manifest.json
```

Then open `manifest.json` and confirm the expected scripts were processed or skipped. Keep `backups/` for comparing original sources when validating a release build.
