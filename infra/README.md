# JARVIS Infrastructure

This directory contains all IaC and deployment config for JARVIS personal AI system.

## Structure

```text
infra/
├── opentofu/                    # OpenTofu (Terraform) modules
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
│   ├── backup.sh               # Compatibility launcher to Rust script
│   ├── deploy.sh               # Compatibility launcher to Rust script
│   └── restore.sh              # Compatibility launcher to Rust script
├── config/                      # Configuration files
│   ├── pii_patterns.toml       # PII detection patterns
│   ├── mcp_allowlist.toml      # MCP tool allowlist
│   ├── policy/                 # Cedar/Casbin policies
│   │   ├── cedar_schema.cedarschema
│   │   ├── casbin_model.conf
│   │   └── casbin_policy.csv
│   ├── opencode/               # OpenCode V2 config
│   │   └── config.toml
│   ├── otelcol-config.yaml     # OpenTelemetry Collector
│   ├── policy.env              # Policy Engine env (1Password)
│   ├── mcp_gateway.env         # MCP Gateway env (1Password)
│   ├── event.env               # Event Normalizer env (1Password)
│   ├── context.env             # Context Builder env (1Password)
│   ├── otel.env                # OTel Collector env (1Password)
│   └── code_server.env         # code-server env (1Password)
└── cloud-init.yaml             # OCI instance initialization
```

## Deployment

### Prerequisites

1. OCI Account with Always Free eligibility
2. OpenTofu installed (`tofu version`)
3. OCI CLI configured (`oci setup config`)
4. 1Password CLI installed and authenticated (`op signin`), plus a service
   account token for unattended `op run` on the host
5. Tailscale account (for secure access)
6. SSH key pair

### Quick Start

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
# allowed_ssh_cidrs   = ["100.64.0.0/10"]  # Tailscale CIDR
EOF

# 3. Initialize and plan
tofu init
tofu plan

# 4. Apply
tofu apply
```

### Post-Deployment

After the instance is provisioned, cloud-init will:

1. Install packages (Rust, OpenCode, code-server, PostgreSQL, Docker, Tailscale)
2. Create `jarvis` user and directories
3. Configure PostgreSQL
4. Install Qdrant (arm64 release binary; no deb is published for arm64)
5. Join Tailscale network
6. Install systemd units and service configs from this repository
7. Generate code-server password
8. Enable all services

Wait 5-10 minutes for cloud-init to complete, then:

```bash
# Check service status via Tailscale SSH
tailscale ssh jarvis@jarvis-host

# Verify services
sudo systemctl status jarvis-* opencode code-server otelcol

# Check logs
sudo journalctl -u jarvis-policy -f
```

### Access

- **code-server (IDE)**: `http://jarvis-host.tailnet.ts.net:8080` (pw in `/etc/jarvis/code-server-password`)
- **OpenCode Web**: `http://jarvis-host.tailnet.ts.net:49374`
- **Metrics**: Policy `:8081`, MCP GW `:8082`, Event `:8083`, Context `:8084`

### Secret Management

Secrets use **1Password CLI** references (`op://`):

- `.env` files under `infra/config/` hold only `op://` refs (no plaintext
  secrets) and are shipped to `/etc/jarvis/` by `rust-script
  rust/scripts/deploy.rs`.
- Services resolve them at start via `op run --env-file=...` (see the systemd
  units). This requires the `OP_SERVICE_ACCOUNT_TOKEN` environment variable,
  which systemd reads from the root-owned `/etc/jarvis/op.env`.

Provision the service account token once after the host is created:

```bash
sudo sh -c 'umask 077; echo "OP_SERVICE_ACCOUNT_TOKEN=ops_..." > /etc/jarvis/op.env'
sudo systemctl restart jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context otelcol
```

Required 1Password vault items:

- `JARVIS/Policy/admin_token`
- `JARVIS/PostgreSQL/connection_string`
- `JARVIS/Home Assistant/long_lived_token`
- `JARVIS/Gmail/client_id`, `client_secret`
- `JARVIS/Google Calendar/client_id`, `client_secret`
- `JARVIS/Grafana/instance`, `token`
- `JARVIS/OCI/access_key_id`, `secret_access_key`, `endpoint`

### Backup & DR

Daily backups run via `jarvis-backup.timer`:

- PostgreSQL dump → OCI Object Storage
- Qdrant snapshot → OCI Object Storage
- Config files → OCI Object Storage
- Git bundles → OCI Object Storage

Retention: 7 days local, 30 days remote.

To test disaster recovery:

```bash
# On fresh instance after tofu apply
rust-script /opt/jarvis/scripts/restore.rs latest --dry-run
# After checking the selected backup, execute the destructive restore:
rust-script /opt/jarvis/scripts/restore.rs latest --confirm
```

### Updating Services

Deploy from the workstation. `deploy.rs` builds for aarch64, ships binaries,
units and configs, restarts services, and runs health checks:

```bash
# Full deploy
rust-script rust/scripts/deploy.rs --rust-dir rust jarvis-host deploy

# Verify the remote command shapes without touching the host
rust-script rust/scripts/deploy.rs --rust-dir rust jarvis-host deploy --dry-run
```

### Monitoring

- **Grafana Cloud Free**: Traces, metrics, logs
- **OCI Monitoring**: Instance health, CPU, memory alarms
- **Tailscale**: Network connectivity

### Troubleshooting

```bash
# View cloud-init logs
sudo journalctl -u cloud-init -f

# Check specific service
sudo journalctl -u jarvis-policy -n 100

# Verify network
tailscale status
sudo ufw status

# Check disk space
df -h /opt/jarvis

# Manual backup
sudo systemctl start jarvis-backup
```

## Architecture Reference

See `../adr/0001-unified-architecture.md` for the unified architecture decision.
