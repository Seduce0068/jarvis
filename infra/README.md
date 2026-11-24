# JARVIS Infrastructure

> **2026-11-15 決定（[ADR-0004](../adr/0004-local-only-no-oci.md)）: OCI は使用しない。**
> 現在の実行基盤は自宅 Kubuntu PC 上の systemd（`systemd/`）で、配置・検証は
> `scripts/` のローカル向けスクリプトで行う。`opentofu/` と `cloud-init.yaml`
> は未使用のレガシーであり、以下の「Deployment」以降は当時の OCI 手順である。

This directory contains the systemd units, deploy/verify scripts, and config
templates for the **local Kubuntu host**. The OpenTofu/cloud-init assets under
`opentofu/` are legacy (OCI is not used; ADR-0004).

## Structure

```text
infra/
├── opentofu/                    # 未使用レガシー: OpenTofu modules (OCI)
│   ├── environments/prod/       # Production environment
│   └── modules/
│       ├── compute/             # OCI Compute (A1 Flex)
│       ├── network/             # VCN, Subnet, IGW, NAT
│       ├── security/            # NSG, Security Rules
│       ├── storage/             # Block Volume, Object Storage
│       └── observability/       # Logging, Monitoring, Alarms
├── systemd/                     # Systemd service units
│   ├── jarvis-policy.service
│   ├── jarvis-mcp-gw.service
│   ├── jarvis-event.service
│   ├── jarvis-context.service
│   ├── qdrant.service
│   ├── opencode.service
│   ├── code-server.service
│   ├── otelcol.service
│   ├── jarvis-backup.service
│   └── jarvis-backup.timer
├── scripts/                     # Operational scripts
│   ├── backup-local.sh         # Local-only backup (PostgreSQL/Qdrant/config)
│   ├── backup-preflight.sh     # Read-only backup preflight (make backup-preflight)
│   ├── backup.sh               # Launcher -> rust/scripts/backup.rs
│   ├── build-selfhost.sh       # Build/deploy Rust sidecars (self-hosted)
│   ├── check-deploy-drift.sh   # Read-only drift/runtime check (jarvis-mcp-gw)
│   ├── decommission-oci.sh     # Legacy OCI decommission (dry-run default)
│   ├── deploy-mcp-gw-safe.sh   # Safe jarvis-mcp-gw deploy (dry-run default)
│   ├── deploy.sh               # Launcher -> rust/scripts/deploy.rs (legacy)
│   ├── install-backup-timer.sh # Install/refresh local backup timer (dry-run default)
│   ├── install-mcp-gateway.sh  # Install jarvis-mcp-gw binary and restart
│   ├── restore-local.sh        # Local-only restore dispatcher (dry-run default)
│   ├── restore.sh              # Launcher -> rust/scripts/restore.rs
│   ├── setup-selfhost.sh       # Self-hosted host setup (Ubuntu/Debian)
│   ├── smoke-mcp-gw.sh         # Pre-deploy smoke test (jarvis-mcp-gw)
│   ├── test-restore-local.sh   # Synthetic-fixture restore regression (no production I/O)
│   ├── verify-backup.sh        # Read-only backup integrity verification
│   ├── lib/                    # Shared shell helpers
│   │   └── payload-descriptors.sh
│   └── restore-local.d/        # Local restore payload handlers
│       ├── configs.sh
│       ├── git.sh
│       ├── pg.sh
│       └── qdrant.sh
├── config/                      # Configuration files
│   ├── pii_patterns.toml       # PII detection patterns
│   ├── mcp_allowlist.toml      # MCP tool allowlist
│   ├── policy/                 # Cedar/Casbin policies
│   │   ├── cedar_schema.cedarschema
│   │   ├── casbin_model.conf
│   │   └── casbin_policy.csv
│   ├── opencode/               # OpenCode V2 config
│   │   └── opencode.jsonc
│   ├── otelcol-config.yaml     # OpenTelemetry Collector
│   ├── policy.env              # Policy Engine env テンプレート (op:// は値なし参照、実秘密は bws run)
│   ├── mcp_gateway.env         # MCP GW env テンプレート (op:// 実参照なし・プレースホルダのみ) -> /etc/jarvis/jarvis-mcp-gw.env
│   ├── event.env               # Event Normalizer env テンプレート (op:// は値なし参照、実秘密は bws run)
│   ├── context.env             # Context Builder env テンプレート (op:// は値なし参照、実秘密は bws run)
│   ├── otel.env                # OTel Collector env (legacy template, op://; 起動経路では未使用)
│   └── code_server.env         # code-server env テンプレート (op:// 実参照なし、実秘密は bws run)
├── grafana/                     # Importable Grafana dashboards
│   ├── README.md
│   └── dashboards/jarvis-overview.json
└── cloud-init.yaml             # 未使用レガシー: OCI instance initialization
```

> `config/*.env` は参照用テンプレートです。現行の秘密情報注入は 1Password CLI
> (`op run`) ではなく、Bitwarden Secrets Manager の `bws run` +
> `/etc/jarvis/bws.env` 経由です（`mcp_gateway.env` は
> `deploy-mcp-gw-safe.sh` の配置元という例外があります。詳細は後述
> 「Secret Management」および「MCP Gateway の環境ファイル（I50-A）」）。

## Self-host setup note

[`scripts/setup-selfhost.sh`](scripts/setup-selfhost.sh) は PostgreSQL の
`jarvis` role を作成する際、ハードコードされたパスワードを使わなくなりました
（P2-11）。新規ホストでは `JARVIS_DB_PASSWORD` を環境変数に設定して実行するか、
peer 認証などで別途 role を準備してください。未設定の場合は role 作成を
スキップし、既存 environment を壊しません。code-server の生成パスワードも
標準出力に出さず、`/home/<user>/.config/code-server/config.yaml`（0600）に
保存します。

## Deployment (legacy: OCI, 現行非使用)

> **2026-11-15 注記（ADR-0004）**: OCI は使用しないため、以下の OCI/OpenTofu/cloud-init 手順は現行のデプロイ手順ではありません。
> 現在の実行基盤はローカル Kubuntu + systemd です。現行の配置/検証は `systemd/` と `scripts/`（例: [scripts/deploy-mcp-gw-safe.sh](scripts/deploy-mcp-gw-safe.sh), [scripts/check-deploy-drift.sh](scripts/check-deploy-drift.sh), [scripts/smoke-mcp-gw.sh](scripts/smoke-mcp-gw.sh)）を参照してください。
> レガシー資産の扱い・残タスクは [adr/0004-local-only-no-oci.md](../adr/0004-local-only-no-oci.md) と [docs/integration/remaining-tasks.md](../docs/integration/remaining-tasks.md)（BK-3 等）を参照。

### Prerequisites (legacy)

> **注記**: OCI/OpenTofu/旧ホスト前提。現行ローカル Kubuntu + systemd 運用ではそのまま必要条件・手順にならない。

1. OCI Account with Always Free eligibility
2. OpenTofu installed (`tofu version`)
3. OCI CLI configured (`oci setup config`)
4. 1Password CLI installed and authenticated (`op signin`), plus a service
   account token for unattended `op run` on the host
5. Tailscale account (for secure access)
6. SSH key pair

### Quick Start (legacy)

> **注記**: 以下は OCI 環境向けの旧手順。現行ローカル運用では実行しないこと。OCI 関連の実行・削除は [docs/integration/remaining-tasks.md](../docs/integration/remaining-tasks.md)（BK-3）と [adr/0004-local-only-no-oci.md](../adr/0004-local-only-no-oci.md) を確認し、前提確認・承認・資格情報が揃う場合にのみ別判断で行う。

```bash
# 1. Navigate to production environment
cd infra/opentofu/environments/prod

# 2. Create terraform.tfvars with your values
cat > terraform.tfvars <<EOF
compartment_id     = "ocid1.compartment.oc1..xxxxx"
ssh_public_key     = "ssh-ed25519 AAAA..."
tailscale_auth_key = "tskey-auth-xxxxx"
# Optional overrides
# availability_domain = "Uocm:AP-TOKYO-1-AD-1"
# allowed_ssh_cidrs   = ["<tailnet-ip>/10"]  # Tailscale CIDR
EOF

# 3. Initialize and plan
tofu init
tofu plan

# 4. Apply
tofu apply
```

### Post-Deployment (legacy, cloud-init)

> **注記**: OCI インスタンス + cloud-init 前提の旧手順。現行ローカル Kubuntu + systemd ホストでは、パッケージ導入・ユーザー/配置・サービス構成・確認コマンドが異なる場合があるため、そのまま実行しないこと。

After the instance is provisioned, cloud-init would:

1. Install packages (Rust, OpenCode, code-server, PostgreSQL, Docker, Tailscale)
2. Create `jarvis` user and directories
3. Configure PostgreSQL
4. Install Qdrant (arm64 release binary; no deb is published for arm64)
5. Join Tailscale network
6. Install systemd units and service configs from this repository
7. Generate code-server password
8. Enable all services

Wait 5-10 minutes for cloud-init to complete, then (legacy example):

```bash
# Check service status via Tailscale SSH (legacy OCI host example)
tailscale ssh jarvis@jarvis-host

# Verify services (adjust unit names/host as needed)
sudo systemctl status jarvis-* opencode code-server otelcol

# Check logs
sudo journalctl -u jarvis-policy -f
```

### Idle Compute Recovery (legacy: OCI, 現行非使用)

> **注記**: OCI/OpenTofu 前提の設計メモ。現行ローカル運用とは無関係だが、`infra/opentofu/` 資産の扱い（BK-3）や将来の整理判断の文脈として残す。

Automatic `tofu apply` recovery is intentionally not enabled. The current
production configuration uses a local `terraform.tfstate` backend, and that
state is not stored in this repository. A timer on the OCI host cannot recover
the host after OCI reclaims it; an external runner without the authoritative
state could instead plan duplicate resources or incur charges.

Before enabling unattended recovery, provide all of the following:

- An external runner that remains available when the OCI host is absent.
- A durable, access-controlled, backed-up state backend with locking, plus a
  verified migration from the current state.
- A least-privilege OCI identity available only to that runner.
- A health check that distinguishes instance loss from transient network,
  DNS, or OCI API failures.
- Bounded retries, alerting, and a plan guard that permits only reviewed
  recovery actions; ambiguous plans must stop for operator review.

Until those prerequisites are satisfied, recover manually from the workstation:
inspect `tofu plan` and the selected backup before applying or restoring. Do
not automate `tofu apply` from a host-local timer or an ephemeral CI runner.

### Access

> Note: Hostnames, paths, ports, and exposure depend on the installed units/configuration on the current host (services typically bind to `127.0.0.1`; remote access, if any, is environment-dependent). Verify against the actual service config and network setup.

- **code-server (IDE)**: example `http://jarvis-host.tailnet.ts.net:8080` (password path may differ; verify on the host, e.g. `/etc/jarvis/code-server-password` in legacy setups)
- **OpenCode Web**: example `http://jarvis-host.tailnet.ts.net:49374`
- **Metrics**: Policy `:8081`, MCP GW `:8082`, Event `:8083`, Context `:8084` (verify bind address/availability on the host)

### MCP Gateway

`jarvis-mcp-gw` is the execution path for MCP tools: every call is
policy-checked and must have an entry in `/etc/jarvis/mcp_allowlist.toml`
(a missing or malformed allowlist aborts startup).

- **Execute**: HTTP `POST /v1/execute` on `:8082` with
  `{"tool": "...", "args": {...}, "principal": "..."}`, or gRPC
  `jarvis.mcp.gateway.v1.McpGatewayService/ExecuteTool` on `:50052`.
- **MCP server endpoint** (Streamable HTTP): `POST /mcp` on `:8082`,
  served by the same process. OpenCode connects as a `remote` MCP server
  (`oauth: false`): `tools/list` exposes the allowlist entries granted to
  `JARVIS_MCP_GW_MCP_PRINCIPAL` (default `opencode`) and every
  `tools/call` passes the same policy + allowlist checks as
  `/v1/execute`. The principal is fixed server-side; clients cannot
  choose it. Entries marked `requires_approval = true` are still listed in
  `tools/list` (the schema is advertised to clients); each `tools/call` waits
  for human approval instead of being hidden. The managed OpenCode
  config in `infra/config/opencode/opencode.jsonc` wires the host up.
- **Register an upstream MCP server** (gRPC only): the gateway spawns the
  command over stdio, discovers its tools via `tools/list`, and routes
  matching calls to it ahead of the sandbox fallback. Discovered tools are
  not allowlisted automatically; registrations live in memory only, so
  re-register after a gateway restart.

```bash
# Example only (legacy/cloud-init note). Verify grpcurl availability, target service, command path, and impact before use.
# Registering changes in-memory gateway state until restart and may affect running behavior; do not run without explicit need/approval.
# grpcurl -plaintext \
#   -d '{"name": "filesystem",
#        "command": ["/usr/local/bin/mcp-server-filesystem"]}' \
#   localhost:50052 jarvis.mcp.gateway.v1.McpGatewayService/RegisterMcpServer
```

### MCP Gateway の環境ファイル（I50-A）

`jarvis-mcp-gw.service` はホスト固有の上書きを
`EnvironmentFile=-/etc/jarvis/jarvis-mcp-gw.env` から読みます。unit の
`Environment=` 値はリテラル既定値であり、この EnvironmentFile の値が優先されます。
repo 側の配置元は `infra/config/mcp_gateway.env` の 1 ファイルで、
[scripts/deploy-mcp-gw-safe.sh](scripts/deploy-mcp-gw-safe.sh) が
`root:root` / `0600`・`.bak-$ts` 退避付きで原子的に上記パスへ配置します。
テンプレートはリネームしないでください。

配置先の env には次の 2 キーが必須です。

- `JARVIS_MCP_GW_BASE_DIR`: 読み取り系ハンドラ（`filesystem.*` / `git.*`）の
  workspace root であり、パスエスケープ境界。
- `JARVIS_MCP_GW_EGRESS_ALLOWLIST`: `web.fetch` の egress 許可ホスト。
  空値は genuine deny-all（fail-closed）。

[scripts/check-deploy-drift.sh](scripts/check-deploy-drift.sh) が存在と必須キーを
fail-closed で検査します（値は出力しません）。env は root:root / 0600 で配置される
ため、単独で drift チェックを実行する場合は root（または読み取り権限）が必要で、
読めない場合は `E_ENV_UNREADABLE` として DANGER になります（deploy スクリプトの
事後確認は root で実行されるため問題ありません）。未配置だと unit のリテラル既定値
（開発用 BASE_DIR と `egress=example.com`）が実効になり、`filesystem.*` の失敗と
「deny-all のつもりが example.com 許可」の fail-open を招きます。

### Kill Switch

`jarvis-policy` owns the kill switch and persists it to
`/var/lib/jarvis/kill-switch.json`
(`JARVIS_POLICY_KILL_SWITCH_STATE_PATH`). Sidecars authorize through the
policy gRPC endpoint (`Check`), so activation reaches every service on the
next call without a restart.

- A missing state file means "not activated" (first start).
- An unreadable or corrupt state file is treated as activated; the service
  fails closed instead of silently disarming the switch.
- Activation and deactivation both require the admin token
  (`JARVIS_POLICY_KILL_SWITCH_ADMIN_TOKEN`); a local process cannot stop the
  system unilaterally.
- The state directory was described as cloud-init-created in the legacy setup; on the current local host, ensure the directory and permissions exist as required by the installed unit (see `ReadWritePaths=/var/lib/jarvis` where applicable).

### Secret Management

Secrets are stored in **Bitwarden Secrets Manager** and injected by
`bws run`:

- The `jarvis` project holds the `JARVIS_*` keys as secrets whose names
  match the environment variables the services read.
- The systemd units run the service via `bws run`. The project ID is **not**
  duplicated in the units: every `jarvis-*.service` expands
  `${JARVIS_BWS_PROJECT_ID}` from `/etc/jarvis/bws.env` (see
  `infra/config/bws.env.example`) into `bws run --project-id`.
  `bws` reads `BWS_ACCESS_TOKEN` from the same `/etc/jarvis/bws.env` (commonly
  root-owned; verify path, ownership, mode, and `EnvironmentFile` in the
  installed unit). 2026-11-15 の read-only 確認では、ローカルの
  `jarvis-policy` / `jarvis-mcp-gw` / `jarvis-event` / `jarvis-context` の
  installed units も `bws run` + `/opt/jarvis/bin/<service>` を使用していたが、
  実行ユーザー/グループや project ID 等は host/unit ごとに異なりうるため、
  実際の `ExecStart`・バイナリ配置・認証設定を必ず確認すること。
- The access token is a machine-account token scoped to the corresponding
  Bitwarden Secrets Manager project (project/name/ID は実機 unit を確認)。

Provision/update the access token as needed (example; 実行前に対象パス・所有者・権限・内容の扱い・影響範囲を確認し、必要な権限/承認のもとで実施すること。再起動は稼働中サービスへ影響する):

```bash
# Example only - verify path/ownership/mode/unit scope before use; do not paste real secrets into shared logs/history
# sudo sh -c 'umask 077; echo "BWS_ACCESS_TOKEN=<token>" > /etc/jarvis/bws.env'
# sudo systemctl restart jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context
```

The repo `otelcol.service` now matches the installed unit and runs
`otelcol-contrib` directly (no `op run`, SEC-1 2026-11-16). The Grafana
credential supply path remains unidentified; do not assume `otel.env` is used.

`infra/config/*.env` files remain reference templates that retain legacy
`op://` references; real secrets are injected by `bws run` (see above), not by
these files. 例外は `infra/config/mcp_gateway.env` で、現行の
[scripts/deploy-mcp-gw-safe.sh](scripts/deploy-mcp-gw-safe.sh) が
`/etc/jarvis/jarvis-mcp-gw.env` へ配置する配置元です（詳細は
「MCP Gateway の環境ファイル（I50-A）」を参照）。
旧デプロイ資産（`deploy.rs`）は `/etc/jarvis/` への配送を想定する記述/実装があるが、
現行ローカル運用での適用可否・実際の配置は host/deploy 手順により異なるため確認が必要。

### Backup & DR（ADR-0004 準拠・ローカル完結）

[ADR-0004](../adr/0004-local-only-no-oci.md) により OCI は使用しない。
現行状態（2026-11-19）: `jarvis-backup.timer` は実機へ導入済みで
バックアップジョブは稼働中。OCI を使用せず、ローカル完結の
backup / verify / restore パスへ移行済み。

現行のローカルバックアップ運用（ADR-0004 準拠、OCI 不使用）:

- 取得: [backup-local.sh](scripts/backup-local.sh)
- 整合性検証: [verify-backup.sh](scripts/verify-backup.sh)
- 復旧: [restore-local.sh](scripts/restore-local.sh)
- 設計: [local-restore-design.md](../docs/integration/local-restore-design.md)
- 定期実行: `jarvis-backup.timer`（2026-11-19 実機導入、稼働中）

履歴（2026-11-15 時点の旧 OCI 前提メモ。現行運用では使用しない）:

- 当時は `jarvis-backup.timer` は未導入、バックアップジョブは未稼働だった。
- 旧構成では PostgreSQL dump / Qdrant snapshot / Config files / Git bundles を
  OCI Object Storage へ保存していた（Retention: 7 days local, 30 days remote）。
- 現状の論点・再設計メモ:
  [docs/integration/backup-redesign-notes.md](../docs/integration/backup-redesign-notes.md)
- 関連タスク:
  [docs/integration/remaining-tasks.md](../docs/integration/remaining-tasks.md)（BK-1/BK-2/SEC-2）

To test disaster recovery（履歴: 旧 OCI 構成の例。現行ローカル環境ではそのまま実行しない）:

```bash
# Legacy example only (OCI/rclone/op/env, /opt/jarvis 配置, 旧ホスト前提)。現行ホストでは実行しないこと。
# 実施する場合も BK-1/BK-2/SEC-2 の再設計・保存先/認証/復旧手順の確定・隔離先での検証・必要な承認後に別途判断する。
# rust-script /opt/jarvis/scripts/restore.rs latest --dry-run
# After checking the selected backup, execute the destructive restore:
# rust-script /opt/jarvis/scripts/restore.rs latest --confirm
```

### Updating Services（レガシー/参照: 旧リモートデプロイ前提）

> **2026-11-15 注記（ADR-0004）**: 現行の実行基盤はローカル Kubuntu + systemd です。以下の `deploy.rs` + `jarvis-host`（aarch64 ビルド/リモート配送）手順は旧構成前提のため、現行運用のそのままの手順ではありません。  
> 現行のローカル配置・検証は `systemd/` と `scripts/` を参照（例: jarvis-mcp-gw は [scripts/deploy-mcp-gw-safe.sh](scripts/deploy-mcp-gw-safe.sh), [scripts/check-deploy-drift.sh](scripts/check-deploy-drift.sh), [scripts/smoke-mcp-gw.sh](scripts/smoke-mcp-gw.sh)。他サービスの現行更新手順は未整理の可能性あり）。

Deploy from the workstation. `deploy.rs` builds for aarch64, ships binaries,
units and configs, restarts services, and runs health checks（旧リモートデプロイ前提の例。現行ローカル運用ではそのまま実行しないこと）:

```bash
# Legacy example only (remote aarch64 deploy target `jarvis-host`; do not run on current local-only setup)
# rust-script rust/scripts/deploy.rs --rust-dir rust jarvis-host deploy

# Verify the remote command shapes without touching the host (legacy example; still executes the script locally)
# rust-script rust/scripts/deploy.rs --rust-dir rust jarvis-host deploy --dry-run
```

### Monitoring

> Note: The following reflects intended/repo configuration and may vary by host. Verify installed `otelcol`/service configs, endpoints, credentials, and exporters before assuming active telemetry.

- **Grafana Cloud Free**: Traces, metrics, logs (if configured)
- **Prometheus scraping**: `otelcol` scrapes `/metrics` on policy `:8081`,
  MCP GW `:8082`, event `:8083`, and context `:8084` (verify actual targets/config on the host). Each service installs
  a Prometheus recorder at startup and exposes `jarvis_http_requests_total`
  alongside exporter-provided metrics.
- **OTLP traces**: services can export spans to `otelcol` at
  `http://127.0.0.1:4317` (`OTEL_EXPORTER_OTLP_ENDPOINT`); the collector
  forwards them to Grafana Cloud when configured.
- **OCI Monitoring**: レガシー（OCI 非使用。現行はローカル運用。ADR-0004 参照）
- **Tailscale**: Network connectivity (environment-dependent)

### Troubleshooting（環境依存・一部レガシー例を含む）

> **注記**: `cloud-init` や `/opt/jarvis`、`jarvis-backup` の手動起動は旧 OCI/旧パス前提の例を含みます（`jarvis-backup.timer` は 2026-11-19 に導入済み）。現行ローカル systemd 環境では、対象サービス・ログ先・データ配置・Tailscale/UFW の有無に応じて読み替えてください（実機状態は read-only で確認）。

```bash
# View cloud-init logs (legacy; local systemd host may not use cloud-init)
sudo journalctl -u cloud-init -f

# Check specific service
sudo journalctl -u jarvis-policy -n 100

# Verify network (if applicable)
tailscale status
sudo ufw status

# Check disk space (path depends on host layout; /opt/jarvis is a legacy example)
df -h /

# Legacy manual start (jarvis-backup.timer is installed and running as of 2026-11-19; use the local backup scripts instead)
# sudo systemctl start jarvis-backup
```

## Architecture Reference

See `../adr/0001-unified-architecture.md` for the unified architecture decision.
