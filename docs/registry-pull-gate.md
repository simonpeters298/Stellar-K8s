# Registry Pull-Path Vulnerability Gate

Epic: #1521 · Policy: `StellarRegistry.spec.admission.pullGate` · Module: `src/controller/registry_gate.rs`

The gate runs on the in-cluster registry's pull path rather than in an
admission webhook, so blocking still works when webhooks are degraded.

```yaml
apiVersion: stellar.org/v1alpha1
kind: StellarRegistry
spec:
  endpoint: registry.stellar.svc:5000
  scanning:
    enabled: true
    endpoint: http://trivy.security:4954
    maxCriticalCves: 0
  admission:
    pullGate: enforce   # off | audit | enforce
```

## Push

`PullGate::handle_push` scans the pushed digest (`<registry>/<repo>@<digest>`)
before the push is acknowledged, and stores a per-digest report. If the
scanner fails, no report is stored, so the digest stays unscanned. Reports can
be queried with `PullGate::report(digest)` and are served at
`<reportBaseUrl>/<digest>`.

## Pull

`PullGate::authorize_pull(digest)` decides whether a pull may proceed. Tag
pulls must be resolved to a digest first.

| Situation | `enforce` | `audit` | `off` |
|-----------|-----------|---------|-------|
| digest never scanned | deny | allow and record | allow |
| critical CVEs above `maxCriticalCves` | deny, with link to the report | allow and record | allow |
| clean | allow | allow | allow |

A denial is returned as an OCI distribution error, `403`:

```json
{"errors":[{"code":"DENIED","message":"sha256:… has 1 critical CVE(s) (max 0): CVE-2024-0001",
            "detail":{"digest":"sha256:…","reportUrl":"https://…/reports/sha256:…"}}]}
```

## Local serving

- `rewrite_to_local` rewrites every image reference to the pull-through
  layout of the local registry (`<local>/<upstream-host>/<path>`), so all
  in-cluster pulls are served locally.
- `CacheStats::hit_rate` tracks the steady-state cache hit rate.
