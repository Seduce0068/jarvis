# Grafana dashboards

Import `dashboards/jarvis-overview.json` into Grafana Cloud:

1. Open **Dashboards → New → Import**.
2. Upload the JSON file.
3. Select the Grafana Cloud Prometheus data source when prompted.

The panels depend on the scrape jobs defined in
`infra/config/otelcol-config.yaml`:

| Panel | Source |
| --- | --- |
| HTTP request rate by service/status | sidecar `/metrics` |
| Total HTTP requests by service/status | sidecar `/metrics` |
| JARVIS services | otelcol Prometheus receiver |
| Dependencies | otelcol Prometheus receiver |

Queries (kept in sync with `jarvis-overview.json`):

```promql
sum by (service, status) (rate(jarvis_http_requests_total[5m]))
sum by (service, status) (jarvis_http_requests_total)
up{job=~"jarvis-.*"}
up{job=~"(qdrant|opencode|code-server|postgresql|node-exporter)"}
```

`jarvis_http_requests_total` carries two fixed labels emitted by each sidecar's
`/metrics`: `service` (e.g. `jarvis-policy`) and `status`. `status` has a
bounded vocabulary that includes `413` (body too large) and `415` (wrong
content type), so grouping by it keeps those rejections as separate series
instead of collapsing them into `other`. No request-path/endpoint label is
emitted, so the panels cannot be split by endpoint.

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
