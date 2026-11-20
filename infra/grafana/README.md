# Grafana dashboards

Import `dashboards/jarvis-overview.json` into Grafana Cloud:

1. Open **Dashboards → New → Import**.
2. Upload the JSON file.
3. Select the Grafana Cloud Prometheus data source when prompted.

The panels depend on the scrape jobs defined in
`infra/config/otelcol-config.yaml`: `jarvis-policy`, `jarvis-mcp-gw`,
`jarvis-event`, `jarvis-context`, `qdrant` (5 jobs total). Jobs that are not
scraped (for example `opencode`, `code-server`, `postgresql`, `node-exporter`)
must not appear in a panel expression.

| Panel | Source |
| --- | --- |
| HTTP request rate by service/status | sidecar `/metrics` |
| Total HTTP requests by service/status | sidecar `/metrics` |
| JARVIS services | otelcol Prometheus receiver (`jarvis-.*`, 4 jobs) |
| Dependencies | otelcol Prometheus receiver (all 5 scrape jobs) |
| Dead-lettered embeddings rate | jarvis-context `/metrics` |
| Dead-letter persistence failures rate | jarvis-context `/metrics` |
| Pending embeddings outbox | jarvis-context `/metrics` |
| Exhausted embeddings outbox | jarvis-context `/metrics` |

Queries (kept in sync with `jarvis-overview.json`):

```promql
sum by (service, status) (rate(jarvis_http_requests_total[5m]))
sum by (service, status) (jarvis_http_requests_total)
up{job=~"jarvis-.*"}
up{job=~"jarvis-policy|jarvis-mcp-gw|jarvis-event|jarvis-context|qdrant"}
sum by (service, error_kind) (rate(jarvis_dead_lettered_embeddings_total[5m]))
sum by (service, error_kind) (rate(jarvis_dead_letter_persistence_failures_total[5m]))
sum by (service) (jarvis_pending_embeddings)
sum by (service) (jarvis_exhausted_embeddings)
```

`jarvis_http_requests_total` carries two fixed labels emitted by each sidecar's
`/metrics`: `service` (e.g. `jarvis-policy`) and `status`. `status` has a
bounded vocabulary that includes `413` (body too large) and `415` (wrong
content type), so grouping by it keeps those rejections as separate series
instead of collapsing them into `other`. No request-path/endpoint label is
emitted, so the panels cannot be split by endpoint.

The dead-letter/outbox metrics are exported by `jarvis-context` with fixed
labels only: the two counters carry `service` (= `jarvis-context`) and
`error_kind` (`embedding` / `persistence`), and the two gauges carry `service`
only. The panels aggregate with `sum by (...)` over exactly those labels, so no
new label or high-cardinality dimension is introduced (no row id, payload,
vector value or raw error text).

## Provisioning

To provision the dashboard automatically, point Grafana's dashboard
provisioning at this directory and mount it read-only:

```yaml
apiVersion: 1
providers:
  - name: jarvis
    folder: JARVIS
    type: file
    options:
      path: /var/lib/grafana/dashboards/jarvis
```

Grafana Cloud Free does not expose file provisioning, so import the JSON
manually there.
