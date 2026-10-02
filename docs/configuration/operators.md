# Operator Environment

Two things live here, because both are properties of the workloads the operator
generates rather than of the `StellarNode` spec:

- [Pod hardening](#pod-hardening) — the security context and AppArmor
  annotations applied to every generated pod.
- [Seed environment variables](#seed-environment-variables) — how the validator
  seed reaches the container, and the override rules that apply to it.

The operator's own startup configuration is a separate concern: it is the
`config.yaml` mounted at `/etc/stellar-operator/config.yaml` from the Helm
release's `operator-config` ConfigMap, located via the `STELLAR_OPERATOR_CONFIG`
environment variable on the operator Deployment. It does not affect the
workloads described on this page.

For the `StellarNode` fields, see [CRD Reference](crd-reference.md).

## Pod hardening

Every pod the operator generates carries a fixed security posture. None of it
is configurable from the `StellarNode` spec or from the operator's environment.

### Container security context

Applied to the main node container:

| Field | Value |
| --- | --- |
| `allowPrivilegeEscalation` | `false` |
| `capabilities.drop` | `["ALL"]` |
| `runAsNonRoot` | `true` |
| `privileged` | `false` |
| `readOnlyRootFilesystem` | `true` |
| `seccompProfile.type` | `RuntimeDefault` |

Applied at pod level:

| Field | Value |
| --- | --- |
| `runAsUser` / `runAsGroup` / `fsGroup` | `10000` |
| `runAsNonRoot` | `true` |
| `seccompProfile.type` | `RuntimeDefault` |

Two operational consequences:

- **`readOnlyRootFilesystem: true` means only mounted volumes are writable.** For
  a Validator that is `/opt/stellar/data`. Anything that needs to write elsewhere
  — a log file, a scratch directory, a cache — needs its own volume added via
  `spec.volumes` and `spec.volumeMounts`. See [Configuration](index.md).
- **Optional sidecars relax this.** The health-check sidecar, the eBPF exporter,
  and the snapshot-restore init container run with their own contexts, and the
  eBPF exporter is `privileged: true` because it loads BPF programs and reads
  `/sys/kernel/debug`. Enabling the eBPF exporter therefore requires a node pool
  that permits privileged containers.

### AppArmor

**There is no `STELLAR_APPARMOR_ENABLED` flag.** AppArmor annotations are applied
unconditionally to every generated pod; the operator has no environment variable
that gates them and no code path that omits them.

For every container and init container in the pod, the operator adds:

```
container.apparmor.security.beta.kubernetes.io/<container-name>: runtime/default
```

The annotations are set on the **pod** (`spec.template.metadata.annotations`) and
cover all containers, with a single exception: annotations injected for Vault
Agent seed delivery are merged in afterwards and can overwrite an entry.

Consequences to be aware of:

- **The annotation is the deprecated form.** The
  `container.apparmor.security.beta.kubernetes.io` prefix predates the
  `securityContext.appArmor` field added in Kubernetes 1.30. The operator emits
  the legacy prefix regardless of cluster version, so the pod relies on the
  kubelet still honouring it. On a cluster that has removed the deprecated
  annotation, pods may be rejected or admitted without an AppArmor profile.
  Pin to a cluster version that still supports it, and watch for removal in
  release notes for the version you run.
- **AppArmor must be available on the node.** The `runtime/default` profile
  requires the AppArmor LSM to be loaded on the node. On a cluster where AppArmor
  is unavailable — notably many managed container runtimes that ship with a
  `seccomp`-only or SELinux-only baseline — the annotation has no effect and the
  pod runs unconfined by AppArmor. This is silent: there is no admission
  rejection and no status condition.
- **To change the profile you must patch the workload, not the CR.** Because the
  operator re-renders the pod template on every reconcile, a `kubectl patch` of
  the annotations is reverted. Changing the profile requires an operator change.

Verify what is actually set on a running pod:

```bash
kubectl -n <namespace> get pod <node>-0 \
  -o jsonpath='{.spec.template.metadata.annotations}'
```

## Seed environment variables

A validator's seed reaches the container as an environment variable. Which
variable, and which source it comes from, depends on the fields you set.

| Configuration | Env var in the container | Value source |
| --- | --- | --- |
| `spec.validatorConfig.keySource: Secret` + `seedSecretRef` | `STELLAR_CORE_SEED` | `valueFrom.secretKeyRef` → `<seedSecretRef>`, key `STELLAR_CORE_SEED` |
| `spec.validatorConfig.keySource: KMS` | `STELLAR_CORE_SEED_PATH` | Literal `/keys/validator-seed` |
| `seedSecretSource` (Vault Agent) | `STELLAR_SEED_FILE` | `/vault/secrets/<file>` |
| `seedSecretSource` (CSI mount) | `STELLAR_SEED_FILE` | The CSI mount path |
| `seedSecretSource` (env from secret) | `STELLAR_CORE_SEED` | `valueFrom.secretKeyRef` → the configured secret and key |

The legacy `STELLAR_CORE_SEED` injection and the `seedSecretSource` injection are
mutually exclusive: when `seedSecretSource` is set, the operator skips the legacy
variable and injects through the seed-injection path instead.

### Overriding with `spec.stellarCoreEnv`

`spec.stellarCoreEnv` entries are merged into the container environment by name.
The merge rule is **replace in place**: if the operator already generated an
entry with the same name, that entry is overwritten and keeps its position in
the list. If there is no existing entry, it is appended.

```yaml
spec:
  validatorConfig:
    keySource: Secret
    seedSecretRef: validator-seed
  stellarCoreEnv:
    # Replaces the operator's STELLAR_CORE_WORKER_THREADS entry.
    - name: STELLAR_CORE_WORKER_THREADS
      value: "16"
    # No operator entry named this — appended at the end.
    - name: MY_TUNABLE
      value: "on"
```

This is a general override mechanism, not a seed-specific one — it applies to
every variable the operator sets, including `NETWORK_PASSPHRASE` and the
per-node-type worker-thread variables.

!!! warning "Overriding `STELLAR_CORE_SEED` discards the Secret reference"

    On the legacy `seedSecretRef` path, the operator's `STELLAR_CORE_SEED` entry
    carries its value via `valueFrom.secretKeyRef`. Because the override replaces
    the whole `EnvVar` struct, a `stellarCoreEnv` entry named
    `STELLAR_CORE_SEED` **drops the `valueFrom` and any `value` you supply
    becomes a literal string in the pod spec** — readable in the pod manifest
    and in `kubectl describe`, and no longer sourced from the `Secret`.

    This is almost never what you want. It defeats the point of referencing a
    `Secret` and puts the seed where anyone with read access to the pod can see
    it.

### The one case that produces a duplicate

`spec.stellarCoreEnv` is merged *before* seed injection is applied, and seed
injection **appends** to the end of the environment list. So if you set
`STELLAR_CORE_SEED` in `stellarCoreEnv` **and** use a `seedSecretSource` that
resolves to `STELLAR_CORE_SEED` (the env-from-secret variant), the container
ends up with two entries of that name:

1. your `stellarCoreEnv` entry, earlier in the list;
2. the injected `valueFrom.secretKeyRef` entry, appended last.

Kubernetes resolves duplicate environment names by taking the **last** entry, so
the injected Secret reference wins and your override is silently inert. The
override is not rejected and produces no warning or status condition — the
duplicate is only visible by inspecting the rendered pod spec.

The two `STELLAR_SEED_FILE` variants do not collide, because `stellarCoreEnv`
entries are keyed by name and the injected name is not the same as anything the
operator generates for that path.

Check for duplicates whenever you combine the two mechanisms:

```bash
kubectl -n <namespace> get pod <node>-0 -o json \
  | python3 -c 'import json,sys; print([e["name"] for e in json.load(sys.stdin)["spec"]["containers"][0]["env"]])'
```

A name appearing twice is the signature.

### Authoring guidance

**Supply the seed in exactly one place.** The operator will not reconcile two
sources, and it will not tell you which one it used.

- Use `spec.validatorConfig.seedSecretRef` (or `seedSecretSource`) and nothing
  else. This is the supported path and keeps the seed in a `Secret`.
- Do not add `STELLAR_CORE_SEED` to `spec.stellarCoreEnv`. If you need to
  reference a different secret or key, change `seedSecretRef` or
  `seedSecretSource` — not the environment.
- Reserve `spec.stellarCoreEnv` for genuine tuning knobs.
- `spec.stellarCoreEnv` applies to Validator pods. `spec.horizonEnv` is the
  Horizon equivalent, and SorobanRpc has no environment override field at all.

`seedSecretRef` is deprecated in favour of `seedSecretSource` (KMS, External
Secrets Operator, or CSI) for production use; see [CRD
Reference](crd-reference.md) and [Credentials and
Secrets](../security/credentials-and-secrets.md).

## Related

- [Configuration](index.md) — generated ConfigMap keys and writable paths
- [Storage](storage.md) — the one writable, persistent location
- [CRD Reference](crd-reference.md) — every `StellarNode` field
- [Pod Security Standards](../security/pss.md)
- [Container Image Security](../container-image-security.md)
- [Credentials and Secrets](../security/credentials-and-secrets.md)
