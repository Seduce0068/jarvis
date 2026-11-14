# ADR-0004: OCI 非使用・ローカルPC運用

- Status: Accepted
- Date: 2026-11-15

## 文脈

ADR-0001 は OCI Always Free（Ampere A1）上の単一 VM に Rust sidecar と
code-server / OpenCode を集約する構成を前提としていた。しかし 2026-11-15
時点で OCI の `jarvis-host` は Tailscale 上 offline が継続しており、実際に
稼働しているのは自宅 Kubuntu PC 上の systemd サービス
（`jarvis-policy` / `jarvis-mcp-gw` / `jarvis-event` / `jarvis-context`）である。

ユーザー判断により **OCI は使用しない**こととする。

## 決定

- 実行基盤は自宅 Kubuntu PC 上の systemd（system unit）とする。
- OCI は使用しない。`infra/opentofu/`（OpenTofu モジュール・cloud-init）は
  未使用のレガシーとして凍結し、削除は別途判断する。
- `infra/scripts/decommission-oci.sh` は過去に作成した OCI リソースの
  後始末専用レガシーとして残す（ローカル state は `.terraform/` にあり
  `.gitignore` 済み）。
- サービスは引き続き `127.0.0.1` bind、外部アクセスは Tailscale 経由とする。
- 2026-11-15 の実機確認で、OCI に依存する active unit / timer / cron は無い
  （稼働中は jarvis 4サービス、qdrant、opencode、code-server、otelcol）。

## 影響

- **デプロイ**: リモート SSH 前提の `rust/scripts/deploy.rs` /
  `infra/scripts/deploy.sh` はローカル運用では使わない。ローカル配置は
  `infra/systemd/` と `infra/scripts/deploy-mcp-gw-safe.sh` 等で行う。
- **バックアップ**: `rust/scripts/backup.rs` は OCI Object Storage（rclone）前提。
  2026-11-15 の実機確認では `jarvis-backup.timer` は未導入、rclone 設定
  （`~/.config/rclone/rclone.conf`）も無く、バックアップジョブは稼働していない。
  保存先の再決定が必要（未解決）。
- **監視**: OCI Monitoring / Alarms 前提の記述は無効。Grafana Cloud と
  `otelcol` による OTLP は継続する。
- **ADR-0001 との関係**: インフラ選定（OCI）部分は本 ADR で置き換える。
  アーキテクチャ境界（OpenCode V2 = Agent Runtime、Rust sidecar の責務）は
  継続する。

## 代替案

- **OCI Always Free の継続**: Idle Compute 回収リスクと運用負担のため却下。
- **他クラウドへの移行**: 現時点で要件なし。必要になった時点で再評価する。

## 関連

- `adr/0001-unified-architecture.md`（インフラ部分を置き換え）
- `adr/0003-native-web-fetch.md`
