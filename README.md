# Remote Agent Daemon

A lightweight Rust MCP (Model Context Protocol) stdio server that runs on remote hosts, enabling AI agents to edit files, search code, and execute commands on remote machines with the same tool interface as local development.

## Features

- **MCP stdio protocol** — JSON-RPC over SSH stdin/stdout, no network ports
- **Hashline patch engine** — Full port of `@oh-my-pi/hashline` with SWAP/DEL/INS/BLK operations, snapshot store, and stale-tag recovery
- **Pi-compatible output** — `[path#TAG]` headers, numbered lines, hashline tags match the local pi tool format exactly
- **6 tools**: `remote_read`, `remote_edit`, `remote_search`, `remote_bash`, `remote_find`, `remote_write`
- **Tiny footprint** — 2.4MB binary, <5MB RSS, <1% CPU on ARM boards
- **Cross-platform** — Fully static musl binaries for x86_64/aarch64 Linux (no glibc dependency), plus macOS and Windows

## Quick Start

### Install

Download the latest binary from [Releases](../../releases), or build from source:

```bash
cargo build --release
```

### Use

The daemon communicates over MCP stdio. Launch it on a remote host via SSH:

```bash
# Direct SSH
ssh user@host /path/to/remote-agentd

# Multi-hop SSH
ssh -J jump-host user@board /path/to/remote-agentd

# From an MCP client (e.g. Claude Code)
claude mcp add --transport stdio remote-board -- ssh user@host /path/to/remote-agentd
```

### Tools

| Tool | Description |
|------|-------------|
| `remote_read` | Read files/directories with line selectors (`:50-100`, `:raw`) and hashline tags |
| `remote_edit` | Apply hashline patch edits (`SWAP`/`DEL`/`INS`/`SWAP.BLK`) |
| `remote_search` | Regex search with gitignore support |
| `remote_bash` | Shell command execution with streaming output |
| `remote_find` | Glob file matching with mtime sorting |
| `remote_write` | Create/overwrite files with auto-mkdir |

## Architecture

```
┌─────────────────────┐     SSH stdio      ┌──────────────────────┐
│   Local Agent (pi)  │ ◄════════════════► │  Remote Agent Daemon │
│                     │   JSON-RPC         │                      │
│  MCP client         │   over SSH tunnel  │  MCP stdio server    │
│  (Claude/omp/etc)   │                     │                      │
└─────────────────────┘                    │  ┌─ hashline engine  │
                                           │  ├─ grep (regex)    │
                                           │  ├─ file I/O        │
                                           │  └─ shell exec      │
                                           └──────────────────────┘
```

## Hashline Patch Language

The daemon implements the hashline patch language for surgical file edits:

```
[path/to/file#TAG]
SWAP 4.=6:
+new line 1
+new line 2
DEL 8
INS.POST 10:
+inserted after line 10
INS.TAIL:
+appended at end
```

- `TAG` is a 4-hex content hash from `remote_read` output
- Edits validate against the current file hash — stale tags are rejected
- Stale-tag recovery attempts 3-way merge / line-shift remap

## Building from Source

```bash
# Native build
cargo build --release

# Cross-compile for aarch64 Linux (fully static musl)
cargo build --release --target aarch64-unknown-linux-musl

# Cross-compile for x86_64 Linux (fully static musl)
cargo build --release --target x86_64-unknown-linux-musl
```

> Linux release binaries are built with musl for fully static linking — drop them on any Linux host (glibc or musl) with no runtime dependency.

## Testing

```bash
# Run all 107 tests
cargo test

# Run hashline engine tests only
cargo test -p hashline
```

## License

MIT
