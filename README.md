# rbxl-obfuscate

`rbxl-obfuscate` is an RBXL-native release tool for Roblox place files. It reads a binary `.rbxl`, finds every `Script`, `LocalScript`, and `ModuleScript`, runs each Luau `Source` property through Prometheus, replaces the source in the DOM, and writes a new `.rbxl`.

It does not use Rojo and does not require Roblox Studio for the normal workflow.

## Setup

Install Rust stable and make sure `cargo` is available:

```bash
rustup install stable
```

Prometheus is installed automatically into `.tools/prometheus-lua/` on the first real run. Dry-run mode does not download or install anything. The `darklua` executable is no longer required on `PATH`.

To use a preinstalled or custom Prometheus executable, set:

```bash
export RBXL_OBFUSCATE_PROMETHEUS=/path/to/prometheus-lua
```

Build the CLI:

```bash
cargo build --release
```

## Usage

```bash
rbxl-obfuscate input.rbxl output.rbxl \
  --level high \
  --backup-dir backups \
  --manifest manifest.json
```

Options:

- `--dry-run`: reports scripts that would be processed and the Prometheus preset that would be used, without running Prometheus, installing Prometheus, or writing an output `.rbxl`.
- `--level <minimal|low|medium|high>`: maps the existing level name to a Prometheus preset. Defaults to `minimal`.
- `--darklua-config <path>`: retained for CLI compatibility, but ignored by the Prometheus backend. Cannot be combined with `--level`.
- `--backup-dir <dir>`: writes original script sources as `.luau` files before replacement.
- `--skip-path <path>`: skips an exact normalized Roblox instance path, such as `game.ServerScriptService.Main`. Can be passed more than once.
- `--manifest <path>`: writes a JSON report of processed, skipped, failed, or dry-run scripts.

The tool refuses to write the output path when it resolves to the same file as the input.

If Prometheus fails to process a script, that script is left unchanged in the output `.rbxl`. The run continues, and all failed script paths are printed at the end.

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

Recommended smoke test:

```bash
rbxl-obfuscate game.rbxl game.release.rbxl \
  --backup-dir backups \
  --manifest manifest.json
```

Then open `manifest.json` and confirm the expected scripts were processed or skipped. Keep `backups/` for comparing original sources when validating a release build.
