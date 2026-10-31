# Grafana dashboards

Import `dashboards/jarvis-overview.json` into Grafana Cloud:

1. Open **Dashboards → New → Import**.
2. Upload the JSON file.
3. Select the Grafana Cloud Prometheus data source when prompted.

The panels depend on the scrape jobs defined in
`infra/config/otelcol-config.yaml`:

| Panel | Query | Source |
| --- | --- | --- |
| HTTP request rate | `sum(rate(jarvis_http_requests_total[5m]))` | sidecar `/metrics` |
| Total HTTP requests | `sum(jarvis_http_requests_total)` | sidecar `/metrics` |
| JARVIS services | `up{job=~"jarvis-.*"}` | otelcol Prometheus receiver |
| Dependencies | `up{job=~"postgresql\|qdrant\|opencode\|code-server\|node-exporter"}` | otelcol Prometheus receiver |

`jarvis_http_requests_total` has no labels yet; the dashboard aggregates it
across all sidecars. Add request-path labels before splitting the panel by
endpoint.

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
