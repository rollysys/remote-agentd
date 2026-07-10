# Remote Agent Daemon

A lightweight Rust MCP (Model Context Protocol) stdio server that runs on remote hosts, enabling AI agents to edit files, search code, and execute commands on remote machines with the same tool interface as local development.

## Features

- **MCP stdio protocol** — JSON-RPC over SSH stdin/stdout, no network ports
- **Hashline patch engine** — Full port of `@oh-my-pi/hashline` with SWAP/DEL/INS/BLK operations, snapshot store, and stale-tag recovery
- **Pi-compatible output** — `[path#TAG]` headers, numbered lines, hashline tags match the local pi tool format exactly
- **8 tools**: `remote_read`, `remote_edit`, `remote_search`, `remote_bash`, `remote_find`, `remote_write`, `remote_fetch`, `remote_put`
- **Tiny footprint** — 2.4MB binary, <5MB RSS, <1% CPU on ARM boards
- **Cross-platform** — Fully static musl binaries for x86_64/aarch64 Linux (no glibc dependency), plus macOS and Windows
- **Sudo support** — Every file tool accepts `sudo: true` for root-owned files (`sudo -n`, requires NOPASSWD)
- **Sideband transfer** — `remote_fetch`/`remote_put` keep large files off the MCP/LLM context via separate `scp`/`rsync` channels

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
| `remote_read` | Read files/directories with line selectors (`:50-100`, `:raw`) and hashline tags. `sudo:true` for root-owned files |
| `remote_edit` | Apply hashline patch edits (`SWAP`/`DEL`/`INS`/`SWAP.BLK`). `sudo:true` reads via `sudo cat`, restores mode/owner |
| `remote_search` | Regex search with gitignore support |
| `remote_bash` | Shell command execution. `sudo:true` wraps in `sudo -n sh -c` |
| `remote_find` | Glob file matching with mtime sorting |
| `remote_write` | Create/overwrite files with auto-mkdir. `sudo:true`, `mode`/`owner` params |
| `remote_fetch` | Large-file download: returns path+checksum for sideband `scp`/`rsync` (bypasses LLM context) |
| `remote_put` | Large-file upload: two-phase commit via sideband `scp`/`rsync` (bypasses LLM context) |

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

## Large File Transfer (sideband)

MCP stdio carries JSON-RPC — every tool result enters the LLM context window. For large files this is a hard bottleneck: a 10MB log file would blow the context. `remote_fetch` and `remote_put` solve this by keeping file bytes **off the MCP channel entirely**.

### remote_fetch (download)

```jsonc
// 1. Agent calls remote_fetch — daemon returns metadata, NOT file bytes
{"path": "/var/log/syslog", "sudo": true}
// → {abs_path: "/var/log/syslog", size: 4718592, sha256: "a1b2…", mode: "640", owner: "syslog"}

// 2. Client opens a SEPARATE scp/rsync connection to pull the file
//    (the MCP client, not the LLM, does this — bytes never touch the context)
scp user@host:/var/log/syslog ./local-copy
```

### remote_put (upload, two-phase commit)

```jsonc
// Phase 1: daemon creates a staging path under /tmp
{"path": "/opt/app/config.yaml", "sudo": true}
// → {staging_path: "/tmp/remote-agentd-staging/config.yaml.1234.5678"}

// Client uploads to staging via a separate scp/rsync connection
scp ./local-config user@host:/tmp/remote-agentd-staging/config.yaml.1234.5678

// Phase 2: daemon atomically renames staged file into place, applies mode/owner
{"path": "/opt/app/config.yaml", "commit": true, "staging_path": "/tmp/…", "sudo": true, "mode": "644", "owner": "app:app"}
```

## Sudo / Permissions

On boards where the daemon runs as a normal user but target files are root-owned (common with `sudo`-managed configs), pass `sudo: true` to any file tool. The daemon shells out to `sudo -n` (non-interactive — fails fast if NOPASSWD is not configured, instead of hanging on a password prompt).

```jsonc
// Read a root-owned file
{"path": "/etc/shadow", "sudo": true}

// Edit a root-owned file (mode/owner preserved automatically)
{"input": "[/etc/hosts#TAG]\nDEL 5", "sudo": true}

// Run a privileged command
{"command": "systemctl restart nginx", "sudo": true}

// Write with explicit permissions
{"path": "/etc/myapp.conf", "content": "…", "sudo": true, "mode": "640", "owner": "root:root"}
```

> **Requires:** `NOPASSWD` sudoers entry for the daemon user. Without it, `sudo -n` returns immediately with an error (no hang).

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
