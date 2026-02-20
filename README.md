# skopio-zed

## Overview

`skopio-zed` is a Zed editor extension that:

- Installs and manages the `skopio-ls` language server
- Installs and manages the [`skopio-cli`](https://github.com/Skopio-app/skopio/tree/main/apps/cli)
- Launches `skopio-ls` with the correct runtime arguments
- Passes configuration to the LSP via CLI flags
- Handles binary downloads, updates, and verification

## Architecture

```text
Zed
 ↓
skopio-zed (extension)
 ↓ (spawns process)
skopio-ls (LSP)
 ↓ (spawns subprocess)
skopio-cli event / sync
```

## Development

### Configure Zed to use dev binaries

In Zed settings:

```json
{
  "lsp": {
    "skopio": {
      "binary": {
        "path": "/absolute/path/to/target/debug/skopio-ls"
      },
      "settings": {
        "cli_path": "/absolute/path/to/target/debug/skopio-cli",
        "idle_secs": 60,
        "switch_grace_secs": 60,
        "min_session_secs": 2,
        "sync_secs": 180
      }
    }
  }
}
```

