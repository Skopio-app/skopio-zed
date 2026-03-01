# skopio-ls

## Overview

`skopio-ls` is a Language Server Protocol (LSP) server that:

- Receives file activity events from Zed
- Tracks file-based coding sessions
- Applies idle + switching heuristics
- Emits session events to [`skopio-cli`](https://github.com/Skopio-app/skopio/tree/main/apps/cli)
- Periodically triggers `skopio-cli` sync

## Architecture

```text
Zed
  ↓ (LSP notifications)
skopio-ls
  ↓ (subprocess)
skopio-cli event
  ↓
Local DB
  ↓
skopio-cli sync
  ↓
Server
```

## What It Tracks

[Activity Signals](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_synchronization)

- did_open
- did_change
- did_save
- did_close
