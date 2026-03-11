# RUDE 🦀 - Rust Unified Download Engine

A high-performance async download engine in Rust, combining the best ideas from
**aria2** (protocol breadth, RPC API, session persistence, chunk splitting) and
**Surge** (work stealing, adaptive workers, sequential/streaming mode, mirror failover).

---

## Architecture

```
┌───────────────────────────────────────────────────────────┐
│                    rude daemon                            │
│                                                           │
│  ┌──────────────────┐     ┌────────────────────────────┐  │
│  │  DownloadManager │────▶│  axum RPC server           │  │
│  │  (DashMap tasks) │     │  REST + WebSocket          │  │
│  └────────┬─────────┘     │  :16800                    │  │
│           │               └────────────────────────────┘  │
│           ▼                                               │
│  ┌──────────────────┐   per task                          │
│  │    Scheduler     │──────────────────────────────┐      │
│  │  probe → split   │                              │      │
│  │  → work loop     │                              ▼      │
│  └────────┬─────────┘                    ┌──────────────┐ │
│           │                              │  WorkQueue   │ │
│           ▼                              │  (ChunkMap + │ │
│  ┌──────────────────┐                    │  work steal) │ │
│  │   MirrorPool     │                    └──────┬───────┘ │
│  │  weighted RR +   │                           │         │
│  │  failure tracking│          ┌────────────────┘         │
│  └──────────────────┘          ▼                          │
│                      ┌─────────────────┐                  │
│                      │  Worker tasks   │                  │
│                      │  (tokio::spawn) │                  │
│                      │  async stream   │                  │
│                      │  + throttle     │                  │
│                      └─────────────────┘                  │
└───────────────────────────────────────────────────────────┘
```

---

## Features

| Feature | Source inspiration |
|---|---|
| Multi-connection parallel chunked download | aria2 `split` + `max-connection-per-server` |
| Work stealing (fast workers steal from slow) | Surge |
| Adaptive worker restart on speed drop | Surge |
| Sequential (streaming) mode for media | Surge |
| Mirror pool with weighted round-robin + failover | aria2 + Surge |
| Pre-file-allocation (`fallocate`/`set_len`) | aria2 |
| Session persistence / crash recovery | aria2 `save-session` |
| JSON REST API + WebSocket live progress | aria2 JSON-RPC style |
| Per-task speed throttle (token bucket) | aria2 `max-download-limit` |
| Checksum verification (SHA256/SHA1/MD5) | aria2 `checksum=` |
| Global bandwidth cap | aria2 `max-overall-download-limit` |

---

## REST API

### Add a download

```http
POST /api/addUri
Content-Type: application/json

{
  "uris": ["https://example.com/file.iso", "https://mirror.example.com/file.iso"],
  "options": {
    "split": 8,
    "max_connection_per_server": 8,
    "dir": "/tmp",
    "out": "myfile.iso",
    "sequential_download": false,
    "checksum": "sha256=abc123..."
  }
}
```

Response: `{ "gid": "550e8400-e29b-41d4-a716-446655440000" }`

### Check status

```http
GET /api/tellStatus/:gid
GET /api/tellActive
GET /api/tellWaiting
GET /api/tellStopped
```

### Control

```http
PUT    /api/pause/:gid
PUT    /api/resume/:gid
DELETE /api/remove/:gid
GET    /api/getGlobalStat
```

### WebSocket live progress

```
ws://localhost:16800/ws/progress/:gid   — DownloadProgress JSON every 500ms
ws://localhost:16800/ws/global          — aggregate stats every 1s
```

---

## CLI

```bash
# Start daemon
rude daemon

# Add a download (runs inline, shows progress bar)
rude add https://example.com/bigfile.iso

# With mirrors and options
rude add \
  https://primary.example.com/file.tar.gz \
  https://mirror1.example.com/file.tar.gz \
  --split 16 \
  --connections 8 \
  --max-speed 10000000 \
  --checksum sha256=deadbeef...

# Sequential mode (for video preview while downloading)
rude add https://example.com/movie.mkv --sequential

# Daemon management (via HTTP to the running daemon)
rude pause  <gid>
rude resume <gid>
rude cancel <gid> [--remove-file]
rude list
rude status <gid>

# Show effective config
rude config
```

---

## Configuration

Default path: `~/.config/rude/config.toml`

```toml
max_concurrent_downloads    = 5
max_connections_per_server  = 8
max_total_connections       = 32
split                       = 8
min_split_size              = 20971520   # 20 MiB
chunk_size                  = 4194304    # 4 MiB
adaptive_workers            = true
slow_worker_threshold       = 0.3        # restart if <30% of median speed
work_stealing               = true
max_retries                 = 5
retry_delay_ms              = 1000
download_dir                = "~/Downloads"
file_allocation             = "prealloc"
save_session_interval       = 60         # seconds
max_download_speed          = 0          # 0 = unlimited

[rpc]
enabled = true
host    = "127.0.0.1"
port    = 16800
secret  = "your-token"  # optional
```

---

## NixOS / Home Manager

```nix
# flake.nix inputs
inputs.rude.url = "github:me-osano/rude";

# home.nix
{ inputs, pkgs, ... }:
{
  imports = [ ./rude.nix ];

  services.rude = {
    enable = true;
    package = inputs.rude.packages.${pkgs.system}.default;
    settings = {
      max_concurrent_downloads    = 5;
      max_connections_per_server  = 8;
      adaptive_workers            = true;
      work_stealing               = true;
      download_dir                = "/home/enosh/Downloads";
      rpc.port                    = 16800;
    };
  };
}
```

---

## Building

```bash
cargo build --release
# binary at ./target/release/rude

# Run tests
cargo test
```

---

## Module map

```
src/
├── main.rs                  Entry point, tokio runtime
├── lib.rs                   Public API re-exports
├── core/
│   ├── config.rs            EngineConfig (TOML-serialisable)
│   ├── task.rs              DownloadTask state machine + types
│   ├── chunk.rs             ChunkMap + WorkQueue (work stealing)
│   ├── worker.rs            Async per-chunk downloader + throttle
│   ├── scheduler.rs         Task lifecycle: probe → split → download → verify
│   ├── mirror.rs            MirrorPool: weighted selection + failover
│   ├── speed.rs             SpeedSampler (rolling window) + ETA
│   ├── manager.rs           DownloadManager: concurrency, task registry
│   └── session.rs           Crash-safe session persist/restore
├── proto/
│   └── types.rs             JSON wire types (request/response)
├── rpc/
│   └── server.rs            axum REST + WebSocket server
└── cli/
    └── mod.rs               clap CLI (daemon / add / status / ...)
```

## 📄 License

MIT License - see [LICENSE](./LICENSE) for details.