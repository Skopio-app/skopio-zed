# Skopio for Zed

## Overview

Skopio for Zed is a macOS coding-activity tracker. It records coding sessions in
Zed, buffers them locally through `skopio-cli`, and syncs them to the local
[Skopio Desktop](https://github.com/Skopio-app/skopio) app.

The extension:

- Installs and manages the `skopio-ls` language server
- Installs and manages the [`skopio-cli`](https://github.com/Skopio-app/skopio/tree/main/apps/cli)
- Launches `skopio-ls` with the correct runtime arguments
- Passes Zed configuration to the language server via CLI flags
- Verifies downloaded CLI archives before extracting them

## Requirements

- macOS on Apple Silicon or Intel
- Skopio Desktop installed and running when events are synced

No separate online account is required. If Skopio Desktop is not running,
activity remains buffered in `~/.skopio/cli.db` and a later sync can retry it.

## Data and privacy

For each coding session, Skopio records:

- The absolute file path and project path
- The project's current Git branch, when available
- Session start and end timestamps and duration
- The app (`Zed`), category (`Coding`), entity type (`File`), and source
  (`skopio-zed`)

Zed sends document-change notifications to `skopio-ls` so it can detect
activity. `skopio-ls` discards the changed text: source-code contents are not
stored in the CLI database or sent to Skopio Desktop.

Disable or uninstall the extension in Zed to stop collecting new coding
activity.

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

## Configuration

The extension works without custom settings. These defaults can be overridden
in Zed's settings:

```json
{
  "lsp": {
    "skopio": {
      "settings": {
        "idle_secs": 60,
        "switch_grace_secs": 60,
        "min_session_secs": 2,
        "sync_secs": 180,
        "category": "Coding",
        "app": "Zed",
        "entity_type": "File",
        "source": "skopio-zed"
      }
    }
  }
}
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
        "sync_secs": 180,
      }
    }
  }
}
```
