# OpenTofu (legacy, unused)

**OCI は使用しない（ADR-0004, 2026-11-15）。**

このディレクトリの OpenTofu モジュールと `cloud-init.yaml` は、以前の OCI
Always Free（Ampere A1）運用のためのレガシーであり、現在は使用していない。
現行の実行基盤は自宅 Kubuntu PC 上の systemd（`infra/systemd/`）である。

過去に作成した OCI リソースの削除が必要な場合のみ、
`infra/scripts/decommission-oci.sh` を使う。
