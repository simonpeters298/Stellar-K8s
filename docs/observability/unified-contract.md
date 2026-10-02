# Unified Observability Contract

Issue #1481 defines a **versioned resource-attribute vocabulary** shared by
logs, metrics, and traces. Signals correlate from pod identity; operators do
not translate field names by hand.

## Schema

Published catalog: [`schemas/observability/resource-attributes.v1.json`](../../schemas/observability/resource-attributes.v1.json)

| Field | Required | Source |
|-------|----------|--------|
| `service.name` | yes | `OTEL_SERVICE_NAME` |
| `service.instance.id` | yes | `POD_UID` |
| `k8s.pod.name` | yes | `POD_NAME` |
| `k8s.namespace.name` | yes | `POD_NAMESPACE` |
| `k8s.node.name` | yes | `NODE_NAME` |
| `stellar.observability.contract.version` | yes | `1.0.0` |

`contractVersion`, `compatibleWith`, and `minCompatibleVersion` live on the
schema document. Compatible emitters keep working across a minor bump.

## Ingest

The bundled OpenTelemetry Collector (`charts/stellar-operator/templates/otel-collector.yaml`):

1. Stamps the contract version when it is absent.
2. Forwards **valid** signals to Tempo / Loki / backends.
3. Routes **violations** to `file/deadletter` (`traces/deadletter`,
   `metrics/deadletter`, `logs/deadletter`). Nothing is silently dropped.

The same rules run in-process via `stellar_k8s::observability_contract`.

## Generator

```bash
python3 scripts/generate-observability-instrumentation.py \
  --service stellar-golden-path \
  --out generated/observability_instrumentation.rs
```

## CI

`python3 scripts/ci/lint-observability-contract.py` fails the build when a
`KeyValue::new("…")` resource key is not in the schema.

## Grafana

Import `monitoring/grafana-log-trace-correlation.json`. Derived fields pivot
from Loki `trace_id` to Tempo using `k8s.pod.name` and `service.instance.id`.

## Pilot

The golden-path service (`emit_golden_path_signals`) emits one log, one
metric, and one span with the **same** resource identity.

## Validation

```bash
python3 scripts/ci/lint-observability-contract.py
python3 -m unittest scripts.tests.test_lint_observability_contract
cargo test --lib observability_contract -- --nocapture
cargo test --test observability_contract -- --nocapture
```
