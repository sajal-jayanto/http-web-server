# http-web-server

A small HTTP reverse proxy and load balancer built on [Pingora](https://github.com/cloudflare/pingora).
You describe everything in a single YAML file: which ports to listen on, which path goes to which
servers, how to spread traffic across them, and optional rate limits and health checks.

```
              ┌──────────────────────────── :8080 ──────────────────────────────┐
 client ───▶  │  /api/*  ──▶ rate limit ──▶ round robin ──▶ 127.0.0.1:3000      │
              │                                         └─▶ 127.0.0.1:3001      │
              │  /*      ──────────────────▶ round robin ──▶ 127.0.0.1:3005     │
              │                                         └─▶ 127.0.0.1:3006      │
              └─────────────────────────────────────────────────────────────────┘
```

## Contents

- [Features](#features)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [How requests are handled](#how-requests-are-handled)
- [Testing](#testing)
- [Project layout](#project-layout)
- [Contributing](#contributing)
- [Known limitations](#known-limitations)

## Features

- **Multiple listeners**: run any number of clusters, each on its own port or address.
- **Optional TLS (HTTPS)** per listener, from a certificate and key file.
- **Optional HTTP/2**: HTTP/1.1 by default; turn on HTTP/2 per listener (ALPN over TLS,
  or cleartext h2c).
- **Path-prefix routing**: the longest matching prefix wins; anything unmatched gets `404`.
- **Four load-balancing algorithms**: `round_robin`, `random`, `hash` (sticky by client IP) and
  `consistent` (sticky, with less reshuffling when upstreams change).
- **Per-route rate limiting** by client IP, answering `429 Too Many Requests` when exceeded.
- **Active health checks** (TCP connect or HTTP `GET` returning `200`); unhealthy upstreams
  stop receiving traffic until they recover.
- **Pingora server settings** (`threads`, `pid_file`, …) live in the same file.

## Requirements

- Rust **1.88 or newer** (the crate uses the 2024 edition and let-chains). Install with [rustup](https://rustup.rs).
- A C compiler and **CMake**, which some of Pingora's native dependencies need at build time.
  - Debian/Ubuntu: `sudo apt install build-essential cmake`
  - Fedora: `sudo dnf install gcc gcc-c++ cmake`
  - macOS: `xcode-select --install && brew install cmake`
- Linux or macOS. Pingora does not support Windows.

## Quick start

```bash
# 1. Build
cargo build --release

# 2. Start two throwaway upstream servers (any HTTP server works)
python3 -m http.server 3005 &
python3 -m http.server 3006 &

# 3. Write a minimal config
cat > config/local.yaml <<'EOF'
version: 1
threads: 2
clusters:
  - name: demo
    listen: 8080
    routes:
      - path: /
        upstreams:
          - 127.0.0.1:3005
          - 127.0.0.1:3006
EOF

# 4. Run the proxy with that config
CONFIG=config/local.yaml cargo run --release

# 5. In another terminal, send a request
curl -i http://localhost:8080/
```

Each request is forwarded to `3005` and `3006` in turn; the access logs of the two Python servers
show it alternating.

Without `CONFIG`, the server reads [config/web-conf.yaml](config/web-conf.yaml) relative to the
current working directory, so run it from the project root or set `CONFIG` to an absolute path.

## Configuration

The config file has two parts: Pingora's own server settings at the top level, and the
`clusters` list that this project adds.

### Server settings

| Key            | Default                               | Description                          |
| -------------- | ------------------------------------- | ------------------------------------ |
| `version`      | —                                     | Pingora config version, use `1`.     |
| `threads`      | Pingora default                       | Worker threads per service.          |
| `pid_file`     | `/tmp/http-web-server.pid`            | Where the PID is written.            |
| `upgrade_sock` | `/tmp/http-web-server_upgrade.sock`   | Socket used for graceful upgrades.   |

Any other [Pingora server option](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/conf.md)
can be added at the top level too.

### Clusters

| Key      | Required | Description                                                                                      |
| -------- | -------- | ------------------------------------------------------------------------------------------------ |
| `name`   | yes      | Label used in health-check service names and error messages.                                     |
| `listen` | yes      | A port (`8080`, binds `0.0.0.0`) or a full `host:port` (`127.0.0.1:8080` for local-only access). |
| `tls`    | no       | Serve HTTPS. See [TLS and HTTP/2](#tls-and-http2). Without it the listener is plain HTTP.        |
| `http2`  | no       | `true` to also accept HTTP/2. Default `false` (HTTP/1.1 only).                                   |
| `routes` | yes      | List of routes, see below.                                                                       |

Two clusters may use the same `listen` address; their routes are merged into one listener. They
must then use the same `tls` and `http2` settings. A path may appear only once per listen address,
otherwise the server refuses to start.

### TLS and HTTP/2

```yaml
clusters:
  - name: web
    listen: 8443
    http2: true                 # optional, default false
    tls:                        # optional, default plain HTTP
      cert: certs/server.crt    # PEM certificate (chain)
      key: certs/server.key     # PEM private key
    routes:
      - path: /
        upstreams: [127.0.0.1:3005]
```

| `tls` | `http2` | What clients can use                                                        |
| ----- | ------- | --------------------------------------------------------------------------- |
| no    | `false` | HTTP/1.1 (the default)                                                      |
| no    | `true`  | HTTP/1.1, or cleartext HTTP/2 with prior knowledge (h2c)                    |
| yes   | `false` | HTTPS with HTTP/1.1                                                         |
| yes   | `true`  | HTTPS with HTTP/2 or HTTP/1.1, picked via ALPN                              |

TLS uses [rustls](https://github.com/rustls/rustls), so no system OpenSSL is needed. The `cert` and
`key` paths are relative to the current working directory. A missing or invalid certificate or key
stops the server at startup. TLS ends at the proxy: traffic to the
upstreams is still plain HTTP/1.1.

To try it locally with a self-signed certificate (the `certs/` directory is git-ignored, so keys
never get committed):

```bash
mkdir -p certs
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=localhost" \
  -keyout certs/server.key -out certs/server.crt
curl -k --http2 https://localhost:8443/     # -k: accept the self-signed cert
curl --http2-prior-knowledge http://localhost:8080/   # h2c, on a listener with http2: true
```

### Routes

| Key            | Default       | Description                                                                      |
| -------------- | ------------- | -------------------------------------------------------------------------------- |
| `path`         | `/`           | Path prefix. The request path is forwarded unchanged (`/api/users` stays as is). |
| `upstreams`    | — (required)  | List of `host:port` backends to balance across.                                  |
| `algorithm`    | `round_robin` | One of `round_robin`, `random`, `hash`, `consistent`.                            |
| `rate_limit`   | none          | See [Rate limiting](#rate-limiting).                                             |
| `health_check` | none          | See [Health checks](#health-checks). Without it, all upstreams count as healthy. |

#### Load-balancing algorithms

| Algorithm     | Behaviour                                                                                |
| ------------- | ---------------------------------------------------------------------------------------- |
| `round_robin` | Each request goes to the next upstream in turn.                                          |
| `random`      | A random upstream per request.                                                           |
| `hash`        | The same client IP always lands on the same upstream.                                    |
| `consistent`  | Like `hash`, but adding or removing an upstream moves fewer clients (ketama hashing).    |

#### Rate limiting

```yaml
rate_limit:
  requests: 100     # max requests per client IP per window
  window_secs: 60   # window length in seconds (default: 1)
```

Limits are counted separately for each route and each client IP. Requests over the limit get
`429 Too Many Requests` and never reach an upstream.

#### Health checks

```yaml
health_check:
  type: http          # tcp (default): can connect / http: GET <path> returns 200
  path: /health       # used by http checks (default: /)
  interval_secs: 5    # seconds between checks (default: 5)
```

HTTP checks send `GET <path>` with `Host: localhost`. An upstream that fails is skipped until it
passes again. If every upstream on a route is unhealthy, the request fails with an error response.

### Full example

See [config/web-conf.yaml](config/web-conf.yaml), which is commented and covers every option:
a rate-limited, health-checked `/api` route plus a catch-all `/` route on port `8080`.

## How requests are handled

1. The listener for the request's port picks the route with the **longest prefix** that matches
   the path. Prefixes match on whole segments: `/api` matches `/api` and `/api/users`, but not
   `/apiary`. No match returns `404`.
2. If the route has a rate limit and the client IP is over it, the proxy returns `429`.
3. The route's balancer picks a healthy upstream (using the client IP as the key for `hash` and
   `consistent`).
4. The request is forwarded over plain HTTP with its original path, and the response is streamed
   back.

## Testing

```bash
cargo test
```

The suite in [src/tests.rs](src/tests.rs) has two kinds of tests:

- **Unit tests** for config parsing and defaults, path matching, and each balancing algorithm.
- **End-to-end tests** (`e2e_*`) that start the real proxy and mock upstreams on free local ports
  and check routing, round robin, stickiness, rate limiting and health checks over real sockets.
  They need nothing but localhost, though the health-check and rate-limit-window tests take a few
  seconds each.

Run a subset by name:

```bash
cargo test config_        # only config tests
cargo test e2e_           # only end-to-end tests
```

### Continuous integration

[.github/workflows/ci.yml](.github/workflows/ci.yml) runs on every push to `master`, on every pull
request, and on demand (`workflow_dispatch`). It builds and checks the code but does **not** deploy.

| Job              | What it checks                                                              |
| ---------------- | --------------------------------------------------------------------------- |
| Format           | `cargo fmt --check`                                                         |
| Clippy           | `cargo clippy --all-targets`, with warnings treated as errors               |
| Test             | `cargo test` on Ubuntu and macOS                                            |
| Minimum Rust     | The crate still compiles with Rust 1.88 (`rust-version` in Cargo.toml)      |
| Release build    | `cargo build --release`, uploads the Linux binary as a 7-day artifact       |
| Security audit   | Dependencies checked against the RustSec advisory database                  |

To make CI block merges, enable branch protection on `master` in the GitHub repo settings and mark
these jobs as required checks.

## Project layout

```
.
├── .github/workflows/
│   └── ci.yml            # CI: fmt, clippy, test, MSRV, release build, audit
├── .githooks/
│   └── pre-push          # runs cargo test, blocks the push on failure
├── Cargo.toml
├── rustfmt.toml          # formatting rules (max width 100)
├── config/
│   └── web-conf.yaml     # default, fully commented config
├── certs/                # local TLS cert/key (git-ignored, create it yourself)
└── src/
    ├── main.rs           # config types, balancers, router, server setup
    └── tests.rs          # unit and end-to-end tests
```

## Contributing

0. **Enable the git hooks** (once per clone): `git config core.hooksPath .githooks`. The
   [pre-push hook](.githooks/pre-push) runs `cargo test` and blocks the push if any test fails.
1. **Branch** off `master`: `git checkout -b feature/short-description`.
2. **Keep it formatted**: run `cargo fmt` before committing. The rules are in
   [rustfmt.toml](rustfmt.toml).
3. **Lint it**: run `cargo clippy --all-targets -- -D warnings`; CI fails on any warning.
4. **Test your change**: add or update tests in [src/tests.rs](src/tests.rs), and make sure
   `cargo test` passes. New config options need a parsing test; new request behaviour needs an
   `e2e_*` test.
5. **Document it**: if you add or change a config option, update both the comment block in
   [config/web-conf.yaml](config/web-conf.yaml) and the tables in this README.
6. **Commit** with a short, imperative summary line (for example `Add per-route timeout option`),
   then open a pull request describing what changed and why.

Code style notes:

- Bad configuration should fail fast at startup with a message naming the cluster and route,
  not at request time.
- Keep defaults in small `default_*` functions next to the config structs so they are easy to find.

## Known limitations

- The proxy talks to upstreams over plain HTTP/1.1 only (no upstream TLS or HTTP/2).
- No automatic HTTP → HTTPS redirect; a listener is either plain HTTP or HTTPS.
- Configuration is read once at startup; restart the server to apply changes.
- Rate limiting and `hash`/`consistent` balancing key on the connecting IP. Behind another proxy
  or load balancer, every client shares that proxy's IP.
- Rate-limit counters live in memory and are not shared between multiple instances.
