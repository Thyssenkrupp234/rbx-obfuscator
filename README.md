# rbx-obfuscator

`rbx-obfuscator` is a Roblox place/model utility for obfuscation and component extraction. It reads `.rbxl`, `.rbxm`, `.rbxlx`, and `.rbxmx` files, finds every `Script`, `LocalScript`, and `ModuleScript`, can run Luau `Source` properties through Prometheus, and writes the result back in the same Roblox file format.

It does not use Rojo and does not require Roblox Studio for the normal workflow.

## Setup

Install the latest version from GitHub:

```bash
curl -fsSL https://raw.githubusercontent.com/Thyssenkrupp234/rbx-obfuscator/main/install.sh | sh
```

The installer keeps normal output quiet and only shows one line per install step. To see the underlying `git`, Prometheus, and Cargo output, run:

```bash
curl -fsSL https://raw.githubusercontent.com/Thyssenkrupp234/rbx-obfuscator/main/install.sh | sh -s -- --verbose
```

The installer:

- downloads or updates this repository from GitHub under `~/.rbx-obfuscator/source`
- installs Rust with `rustup` if `cargo` is missing
- installs or updates Prometheus
- builds the release binary
- installs `rbx-obfuscator` to `~/.local/bin`
- adds `~/.local/bin` to `PATH` in your shell profile

The one-line installer fetches `install.sh` from GitHub. If you already have a checkout, you can also run:

```bash
./install.sh
```

Installer settings can be overridden with environment variables:

```bash
RBX_OBFUSCATOR_REPO_URL=https://github.com/Thyssenkrupp234/rbx-obfuscator.git
RBX_OBFUSCATOR_BRANCH=main
RBX_OBFUSCATOR_INSTALL_ROOT="$HOME/.rbx-obfuscator"
RBX_OBFUSCATOR_BIN_DIR="$HOME/.local/bin"
```

Manual build:

```bash
cargo build --release
```

Prometheus is a direct runtime dependency. The CLI looks for `prometheus-lua` on `PATH`. If it is missing during a real run, the CLI installs it with the official Prometheus installer.

The tool also records the last successful Prometheus install or update under the user's state directory. If that timestamp is older than one week and an internet connection is available, it runs `prometheus-lua update`; if that fails, it retries the installer command. If there is no internet connection, the update is skipped and the existing Prometheus installation is used.

Dry-run mode does not install, update, or launch Prometheus.

## Usage

There are three direct commands:

- `rbx-obfuscator obfuscate`: obfuscates scripts inside a Roblox file.
- `rbx-obfuscator extract`: exports readable components from a Roblox file.
- `rbx-obfuscator update`: updates the installed CLI and managed dependencies.

Running `rbx-obfuscator` with no command always opens the interactive terminal wizard:

```bash
rbx-obfuscator
```

The wizard lets you choose full obfuscation or component extraction, paste paths, pick the Prometheus level, watch clean progress, and see the equivalent direct command on the completion screen. Direct commands skip the setup wizard and go straight to the progress screen.

### Obfuscate

Obfuscation must be run through the `obfuscate` command. The input file and `--level` are required.

Obfuscate a place file and write the default output next to the input:

```bash
rbx-obfuscator obfuscate game.rbxl --level medium
```

For `game.rbxl`, that writes:

```text
game-obfuscated_Medium.rbxl
```

Obfuscate a file with an explicit output path:

```bash
rbx-obfuscator obfuscate /Users/lincolnmuller/Documents/train\ game.rbxl --level high --output ~/train_game_obfuscated_high.rbxl
```

Obfuscation syntax:

```bash
rbx-obfuscator obfuscate <input.rbxl|input.rbxm|input.rbxlx|input.rbxmx> --level <minimal|low|medium|high> [--output <output-file>]
```

Obfuscation options:

- `--level <minimal|low|medium|high>`: required obfuscation complexity. Maps to a Prometheus preset.
- `--output <path>`, `-o <path>`: optional output `.rbxl`, `.rbxm`, `.rbxlx`, or `.rbxmx` path. Defaults to `<input-stem>-obfuscated_<Level>.<extension>`.
- `--dry-run`: reports scripts that would be processed and the Prometheus preset that would be used, without running Prometheus or writing output.
- `--strip-types`: removes Luau type annotations and lowers known Prometheus-incompatible Luau syntax before every Prometheus run.
- `--backup-dir <dir>`: writes original script sources as `.luau` files before replacement.
- `--skip-path <path>`: skips an exact normalized Roblox instance path, such as `game.ServerScriptService.Main`. Can be passed more than once.
- `--manifest <path>`: writes a JSON report of processed, skipped, failed, or dry-run scripts, including whether Luau compatibility preprocessing was applied.
- `--verbose`, `-v`: shows detailed per-script and Prometheus logs. Normal output stays compact.

### Extract

Extraction must be run through the `extract` command. The only required argument is the input Roblox file.

```bash
rbx-obfuscator extract /Users/lincolnmuller/Documents/train\ game.rbxl
```

When no output folder is given, extraction creates a sibling folder using the input file name without its extension. The command above writes to:

```text
/Users/lincolnmuller/Documents/train game/
```

Use an explicit output folder when you want the extracted project somewhere else:

```bash
rbx-obfuscator extract /Users/lincolnmuller/Documents/train\ game.rbxl ~/train_game_components
```

Extraction syntax:

```bash
rbx-obfuscator extract <input.rbxl|input.rbxm|input.rbxlx|input.rbxmx> [output-folder]
```

Extraction exports scripts, GUI JSON, the instance tree, content references, and `manifest.json`.

### Update

Update the CLI and managed dependencies:

```bash
rbx-obfuscator update
```

`update` downloads the latest installer from GitHub, updates the source checkout, rebuilds the CLI, reinstalls it, and updates Prometheus. Pass `--verbose` after `update` to show installer output.

The tool accepts `.rbxl`, `.rbxm`, `.rbxlx`, and `.rbxmx` inputs. It refuses to write the obfuscation output path when it resolves to the same file as the input, and extraction refuses to use the input file as the output folder.

## Extraction

`rbx-obfuscator extract` deconstructs a Roblox place/model file into a readable project breakdown:

```text
Ro-TransLink-extracted/
  scripts/
  guis/
  instances.json
  content_refs.json
  manifest.json
```

Extraction exports:

- script sources that exist in the file as `.luau`
- GUI roots such as `ScreenGui`, `BillboardGui`, and `SurfaceGui` as JSON
- a readable `instances.json` hierarchy with safe serializable properties
- asset/content references such as `rbxassetid://`, `rbxasset://`, Roblox asset URLs, texture IDs, mesh IDs, sound IDs, and animation IDs
- a manifest with counts, warnings, unsupported property notes, and exported script metadata

Extraction does not decompile bytecode, recover source that is not stored in the file, download external assets, or claim raw assets were recovered. It lists references that are available in serializable properties.

Prometheus can fail on some Luau type syntax, such as `local Bus:ObjectValue = script.Bus` or `for _, Car: Model in pairs(cars) do`, and on some newer Luau syntax, such as if-expressions and backtick string interpolation. By default, the tool only applies this Luau compatibility preprocessing after Prometheus fails for a script, then retries that one script once. Use `--strip-types` to preprocess every script before Prometheus runs.

If Prometheus still fails to process a script, that script is left unchanged in the output file. The run continues, and all failed script paths are printed at the end.

Long-running scripts are guarded so one huge ModuleScript does not stall the whole run forever. After a script has been running for 10 seconds in the interactive wizard, the footer shows controls: press Enter to skip that script, `m` to restart that script with the Prometheus `Minify` preset, or `k`/`s` to force the current preset to keep running. If the script reaches 30 seconds without a bypass, the tool automatically switches that script to `Minify`; if `Minify` also runs for 30 seconds without a bypass, the script is skipped unchanged. Direct CLI mode uses the same automatic Minify-then-skip behavior without keyboard prompts.

Prometheus preset mapping:

- `minimal`: `Weak`
- `low`: `Weak`
- `medium`: `Medium`
- `high`: `Strong`

## Local Development

To test the working checkout without installing or updating the released CLI, run the binary through Cargo:

```bash
cargo run -- --help
cargo run -- obfuscate /Users/lincolnmuller/Documents/train\ game.rbxl --level high --output /tmp/train_game_obfuscated_high.rbxl
cargo run -- extract /Users/lincolnmuller/Documents/train\ game.rbxl /tmp/train_game_components
```

The `--` after `cargo run` separates Cargo's flags from `rbx-obfuscator`'s flags. This always uses the code in the current checkout.

You can also build once and run the local debug binary directly:

```bash
cargo build
./target/debug/rbx-obfuscator --help
./target/debug/rbx-obfuscator obfuscate game.rbxl --level medium --dry-run
```

Use `cargo build --release` and `./target/release/rbx-obfuscator ...` when you want to test the optimized binary before installing it.

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
cargo run -- obfuscate game.rbxl --level medium --dry-run --manifest manifest.json
```

Then open `manifest.json` and confirm the expected scripts were processed or skipped. Keep `backups/` for comparing original sources when validating a release build.
