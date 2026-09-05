# HAProxy SPOA IP Reputation (FireHol)

A high-performance HAProxy external agent (SPOA) that blocks malicious IPs in real-time using FireHol blocklists.

## Table of Contents

- [Features](#features)
- [Architecture](#architecture)
- [How It Works](#how-it-works)
  - [Request Flow](#http-request-flow)
  - [Auto-Update Flow](#auto-update-flow)
- [Requirements](#requirements)
- [Test](#test)
- [Configuration](#configuration)
  - [Environment Variables](#environment-variables)
  - [HAProxy Configuration](#haproxy-configuration)
- [License](#license)


## Features

- 🛡️ **Real-time IP Blocking**: Blocks malicious IPs at HAProxy level before reaching backend
- 🌎 **FireHol Integration**: Auto-updates from [FireHol blocklist-ipsets](https://github.com/firehol/blocklist-ipsets)
- 🗃️ **Category-based Filtering**: Block IPs by category (abuse, spam, attacks, etc.)
- 📃 **File Name-based Filtering**: Block IPs by file name (cybercrime.ipset, cidr_report_bogons.netset, etc.)
- 🔥 **Hot Reload**: MMDB updates without service interruption
- 🚀 **High Performance**: Rust-based SPOA with parallel processing
- ♻️ **Cron Auto-Update**: Configurable update schedule (default: hourly)


## Architecture

```
                              ┌─────────────────────────────────────────────────────────────┐
                              │                     HAProxy Load Balancer                   │
Internet ──────► Request ────►│  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐      │
(Malicious or Clean IP)       │  │   TCP       │    │   HTTP      │    │   SPOE      │      │
                              │  │   Frontend  │───►│   Frontend  │───►│   Agent     │      │
                              │  └─────────────┘    └─────────────┘    └──────┬──────┘      │
                              │                                               │             │
                              └───────────────────────────────────────────────┼─────────────┘
                                                                              │
                                                                              │ SPOE Protocol
                                                                              │ (port 9000)
                                                                              ▼
                              ┌─────────────────────────────────────────────────────────────┐
                              │              HAProxy SPOA IP Reputation FireHol             │
                              │  ┌─────────────────┐    ┌─────────────────────────────┐     │
                              │  │  SPOA Server    │    │   IP Reputation Engine      │     │
                              │  │  (port 9000)    │───►│                             │     │
                              │  └─────────────────┘    │  ┌─────────────────────┐    │     │
                              │                         │  │  MMDB (FireHol DB)  │    │     │
                              │                         │  │  - Malicious IPs    │    │     │
                              │                         │  │  - Categories       │    │     │
                              │                         │  │  - Metadata         │    │     │
                              │                         │  └─────────────────────┘    │     │
                              │                         └─────────────────────────────┘     │
                              └─────────────────────────────────────────────────────────────┘
                                                                              │
                                                                              │ Auto-update
                                                                              ▼
                              ┌─────────────────────────────────────────────────────────────┐
                              │              FireHol Blocklist Repository                   │
                              │     https://github.com/firehol/blocklist-ipsets.git         │
                              │  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐      │
                              │  │   Clone     │───►│   Parse     │───►│   Build     │      │
                              │  │   Repo      │    │   .ipset    │    │   MMDB file │      │
                              │  │             │    │   .netset   │    │             │      │
                              │  └─────────────┘    └─────────────┘    └─────────────┘      │
                              └─────────────────────────────────────────────────────────────┘
```

## How It Works

### HTTP Request Flow

```
┌──────────┐     ┌──────────┐     ┌──────────┐     ┌────────────┐     ┌─────────────┐
│  Client  │     │ HAProxy  │     │   SPOA   │     │  MMDB file │     │ Stick Table │
│          │     │          │     │  Server  │     │  Lookup    │     │    Haproxy  │
└──────────┘     └──────────┘     └──────────┘     └────────────┘     └─────────────┘
     │                 │                │                │                │
     │  1. Request     │                │                │                │
     │────────────────►│                │                │                │
     │                 │                │                │                │
     │                 │  2. Check IP   │                │                │
     │                 │  in stick      │                │                │
     │                 │  table         │                │                │
     │                 │─────────────────────────────────────────────────►│
     │                 │                │                │                │
     │                 │◄─────────────────────────────────────────────────│
     │                 │  IP: bad, good │                │                │
     │                 │    or unknown  │                │                │
     │                 │                │                │                │
     │                 │  3. SPOE Event │                │                │
     │                 │  (src_ip)      │                │                │
     │                 │  (if unknown)  │                │                │
     │                 │───────────────►│                │                │
     │                 │                │                │                │
     │                 │                │  4. Lookup IP  │                │
     │                 │                │───────────────►│                │
     │                 │                │                │                │
     │                 │                │  5. Result     │                │
     │                 │                │◄───────────────│                │
     │                 │                │   (blocked/    │                │
     │                 │                │    allowed)    │                │
     │                 │                │                │                │
     │                 │  6. Decision   │                │                │
     │                 │◄───────────────│                │                │
     │                 │                │                │                │
     │                 │ 7. Store bad or│                │                │
     │                 │  good IP in    │                │                │ 
     │                 │  stick table   │                │                │
     │                 │─────────────────────────────────────────────────►│
     │                 │                │                │                │
     │8. Allow or Block│                │                │                │
     │◄────────────────│                │                │                │
     │                 │                │                │                │
```

### Auto-Update Flow

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           Startup / Cron Trigger                            │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  1. Clone/Update FireHol Repository (git fetch + hard reset)                │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  2. Collect .ipset and .netset files (parallel processing)                  │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  3. Parse IP/CIDR entries with metadata (category, maintainer, URLs)        │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  4. Build MMDB file (MaxMind DB format with deep merge)                     │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  5. Hot-reload MMDB (atomic swap, zero downtime)                            │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  6. File Watcher detects change → SPOA uses new rules immediately           │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Requirements

- Rust 1.98.0+
- Docker & Docker Compose (for containerized deployment)


## Test

```shell
cat > haproxy/haproxy-spoa-ip-reputation-firehol.cfg <<'EOF'
[ip-reputation]
spoe-agent ip-reputation
    groups check-ip
    option var-prefix iprep
    option pipelining
    timeout hello 2s
    timeout idle 30s
    timeout processing 100ms
    use-backend spoe-backend

spoe-message check-client-ip
    args ip=urlp(ip) # <-- get ip by params

spoe-group check-ip
    messages check-client-ip

EOF

make docker-compose-up
```

In another terminal
```shell
curl -v "http://127.0.0.1:8080?ip=1.24.16.32"
```

## Configuration

### Environment Variables

| Variable                    | Default                                          | Description                         |
|-----------------------------|--------------------------------------------------|-------------------------------------|
| `LOG_LEVEL`                 | `info`                                           | Log level                           |
| `MMDB_PATH`                 | `firehol.mmdb`                                   | Path to MMDB database file          |
| `SPOA_LISTEN_ADRESS`        | `0.0.0.0:9000`                                   | SPOA server listen address          |
| `DROP_CATEGORY`             | `abuse`                                          | Comma-separated categories to block |
| `DROP_FILE_NAMES`           | `abuseipdb_1d.ipset`                             | Comma-separated file names to block |
| `FIREHOL_REPO_PATH`         | `firehol-blocklist-ipsets`                       | Local git repo path                 |
| `FIREHOL_REPO_URL`          | `https://github.com/firehol/blocklist-ipsets.git`| Git repository URL                  |
| `FIREHOL_IGNIORE_COUNTRY`   | `true`                                           | Ignore country-specific blocklists  |
| `FIREHOL_UPDATE_CRON_JOB`   | `@hourly`                                        | Cron schedule for auto-updates      |
| `FIREHOL_REPO_BRANCH`       | `master`                                         | Git branch to use                   |

### HAProxy Configuration

```haproxy
# haproxy/haproxy-spoa-ip-reputation-firehol.cfg

[ip-reputation]
spoe-agent ip-reputation
    groups check-ip
    option var-prefix iprep
    option pipelining
    timeout hello 2s
    timeout idle 30s
    timeout processing 100ms
    use-backend spoe-backend

spoe-message check-client-ip
    args ip=src
    ; args ip=urlp(ip)

spoe-group check-ip
    messages check-client-ip
```

```haproxy
# haproxy/haproxy.cfg

global
    log stdout format raw daemon info

defaults
    mode http
    option httplog
    log global

    timeout connect 1s
    timeout client 30s
    timeout server 30s
    timeout tarpit 15s

frontend http-in
    bind :8080

    stick-table type ip size 1m expire 10s store gpt0

    http-request track-sc0 src

    acl ip_good sc_get_gpt0(0) -m int eq 1
    acl ip_bad sc_get_gpt0(0) -m int eq 2
    acl ip_unknown sc_get_gpt0(0) -m int eq 0

    filter spoe engine ip-reputation config /etc/haproxy/haproxy-spoa-ip-reputation-firehol.cfg

    http-request send-spoe-group ip-reputation check-ip if ip_unknown

    http-request sc-set-gpt0(0) 1 if ip_unknown { var(sess.iprep.ip_bad) -m int eq 0 }
    http-request sc-set-gpt0(0) 2 if ip_unknown { var(sess.iprep.ip_bad) -m int eq 1 }

    #http-request tarpit if ip_bad
    http-request silent-drop if ip_bad

    default_backend web

backend web
    http-request return status 200 content-type "text/plain" string "Hello World\n"

backend spoe-backend
    mode tcp
    option spop-check
    server spoa1 spoa:9000 check

```


## License

MIT License
