# Contributing to Stellar-K8s

Thank you for contributing to Stellar-K8s! This guide explains how to work with the project, keep your pull requests ready for review, and follow our commit and merge conventions.

## Troubleshooting Quick Links

If you run into issues, jump to the relevant section below:
- [Setup Issues](#setup-issues)
- [Build Failures](#build-failures)
- [Cargo Issues](#cargo-issues)
- [Docker Issues](#docker-issues)
- [Kubernetes Issues](#kubernetes-issues)
- [CI Failures](#ci-failures)

## 1. Fork and Pull Request Workflow

We use a fork-and-pull-request model. The basic flow is:

1. **Fork** the repository on GitHub.
2. **Clone** your fork locally:
   ```bash
   git clone https://github.com/YOUR_USERNAME/stellar-k8s.git
   cd stellar-k8s
   ```
3. **Add the upstream remote**:
   ```bash
   git remote add upstream https://github.com/OtowoOrg/Stellar-K8s.git
   ```
4. **Sync from upstream** before creating a branch:
   ```bash
   git fetch upstream
   git checkout main
   git merge upstream/main
   ```
5. **Create a new branch** for your work.
6. **Make focused commits**.
7. **Run local checks** before pushing.
8. **Push your branch** to your fork.
9. **Open a Pull Request** against the upstream `main` branch.

## 2. Branch Naming and Strategy

Use clear, descriptive branch names. Recommended prefixes:

- `feat/` for new features (e.g. `feat/auto-mtls`)
- `fix/` for bug fixes (e.g. `fix/panic-on-startup`)
- `docs/` for documentation updates (e.g. `docs/update-architecture`)
- `chore/` for maintenance or dependency changes (e.g. `chore/bump-kube-rs`)
- `test/` for test-related work (e.g. `test/e2e-service-mesh`)

### Branching Rules

- Always branch from the latest `main`.
- Do not work directly on `main`.
- Keep each branch scoped to a single feature, bug fix, or documentation item.
- Rebase or merge `main` into your branch before opening a PR if `main` has advanced.

### Merge Strategy

We prefer a clean history. When your PR is approved, maintainers will typically merge it using:

- **Squash and merge** for feature and fix branches
- **Rebase and merge** only when preserving a linear history is important

If your PR contains multiple logical changes, split it into separate branches and PRs.

## 3. PR Checklist

Before opening a PR, confirm the following:

- [ ] The code or documentation change is complete and focused.
- [ ] The PR targets the `main` branch.
- [ ] Your branch is up to date with `main`.
- [ ] You have run tests locally.
- [ ] You have run formatting and lint checks.
- [ ] You have added or updated documentation, if needed.
- [ ] Commit messages are clear, accurate, and follow our conventions.
- [ ] Every commit includes a DCO sign-off.
- [ ] The PR description is filled out completely using the template.
- [ ] The PR includes links to any related issues or design discussions.

### Required checks

Before submitting, run the contributor health gates via `make` (not raw
`cargo` — Make targets set the workspace feature flags so results match CI):

```bash
make health        # Format + lint + tests + docs
make ci-local      # Full local CI gate (includes audit + link-check)
```

The full checklist, command rationale, and per-step details live in the
[Canonical Repository Health Checklist](docs/development/repo-health-checklist.md).
If your change adds shell scripts, also run `make shellcheck`.

You can also run spell checking locally using `codespell`:
```bash
pip install codespell
codespell --ignore-words=.codespellignore --skip="Cargo.lock,package-lock.json,target,node_modules,vendor,*-vendor" docs/ src/ *.md
```

## 4. Commit Message Examples

We follow [Conventional Commits](https://www.conventionalcommits.org/).

Correct examples:

```text
feat(cli): add support for --dry-run mode
fix(webhook): handle nil admission review objects
docs(contributing): clarify PR checklist and branch strategy
test(integration): add end-to-end service mesh coverage
chore(deps): bump kube-rs to 0.1.0
```

When to use each type:

- `feat:` new functionality
- `fix:` bug fixes
- `docs:` documentation-only changes
- `chore:` maintenance tasks and dependency updates
- `refactor:` code changes that do not add features or fix bugs
- `test:` adding or updating tests

Example with body and footer:

```text
fix(metrics): avoid panic when metrics registry is empty

This change adds a guard around metric registration so operator startup
continues even if no collector is present.

Signed-off-by: Alice Doe <alice@example.com>
```

## 5. Developer Certificate of Origin (DCO)

All commits must include a `Signed-off-by` line.

Add this automatically with:

```bash
git commit -s -m "fix: your fix description"
```

The sign-off must match the commit author. Unsigned commits may fail CI and block merge.

## 6. Pull Request Template

A PR template is provided in `.github/PULL_REQUEST_TEMPLATE.md` and will populate the PR description when you open a PR.

Fill out every section fully. Do not leave the template blank or remove required checklist items.

The template ensures your change includes:

- tests and validation
- documentation updates when required
- formatting and linting checks
- DCO sign-off

## 7. Development Environment

### Prerequisites

- Rust 1.92+ (CI-enforced minimum — see `scripts/lib/versions.sh`)
- Kubernetes local cluster (`kind`, `minikube`, etc.)
- Docker
- `cargo-audit`
- `pre-commit` hooks

### Setup

Docker, kind, kubectl, Helm, and `gh` must currently be installed manually
for your OS (automated installers for these are tracked separately — see
[DEVELOPMENT.md § Prerequisites](DEVELOPMENT.md#prerequisites) for the
per-tool install links). Then run:

```bash
make dev-setup
```

`make dev-setup` installs the Rust toolchain/components, `cargo-audit` and
`cargo-watch`, and the `pre-commit` hooks, then runs
`stellar-bootstrap-verify` as a final step and prints a pass/fail report of
every required tool and version pin — see
[DEVELOPMENT.md § Troubleshooting](DEVELOPMENT.md#missing-or-outdated-tools)
if it reports anything missing or outdated.

### Local checks — Canonical Workflow

Always drive the local pipeline through `make` targets so results match CI:

```bash
make health        # Contributor health gate
make ci-local      # Full CI pipeline (fmt-check + lint + docs-lint + audit + test + build + link-check)
```

See the [Canonical Repository Health Checklist](docs/development/repo-health-checklist.md)
for the full command set and per-step expectations.

### Script tests — bats harness

Shell scripts under `scripts/` are covered by a [bats](https://github.com/bats-core/bats-core)
test harness in `scripts/tests/`. CI runs these suites on every PR that touches
`scripts/`, so add or extend a suite whenever you change a script.

**Prerequisites** — install bats (and its helper libraries) locally:

```bash
# macOS
brew install bats-core

# Debian/Ubuntu
sudo apt-get install -y bats

# Any platform via npm
npm install -g bats
```

**Run the suites** — the same invocation CI uses:

```bash
bats scripts/tests/
```

To run a single suite while iterating:

```bash
bats scripts/tests/preflight.bats
```

**Adding a new suite** — create `scripts/tests/<script-name>.bats` next to the
script it exercises, then:

1. Start the file with `#!/usr/bin/env bats` and load shared helpers with
   `load 'test_helper'` if the suite needs common fixtures.
2. Add one `@test "<description>"` block per behavior you want to lock in.
3. Use `run <command>` and assert on `$status` / `$output` so failures are
   reported per test case.
4. Keep each suite self-contained: create any temp files under `$BATS_TEST_TMPDIR`
   and clean up after the test.
5. Verify locally with `bats scripts/tests/<script-name>.bats` before pushing.

**Reference suite** — [`scripts/tests/preflight.bats`](scripts/tests/preflight.bats)
is the canonical example: it shows the expected file layout, helper loading, and
assertion style to follow when adding new suites.

### Operational scripts index

[`scripts/README.md`](scripts/README.md) indexes every operational script in the
repository: what it does, the canonical invocation (and the `make` target that
wraps it, where one exists), and the suite in `scripts/tests/` that covers it. It
also separates the CI-only helpers in `scripts/ci/` from the scripts you are
expected to run locally.

To lint the GitHub issue templates locally — the same check CI runs when
`.github/ISSUE_TEMPLATE/` changes:

```bash
python3 scripts/issue_template_lint.py
```

The command exits non-zero and lists every offending template when an Issue Form
is missing required keys (`name`, `description`, `body`), uses an unsupported
`body` field type, or has a malformed `config.yml`. It is also part of
`make health`.

## 8. Coding Standards

- Format Rust code with `make fmt`.
- Lint with `make lint` (clippy with the project's feature flags).
- Run tests with `make test`.
- Document behavior changes in code comments and docs.
- Keep PRs small and easy to review.

### Rust code conventions

- Module names use `snake_case`.
- Public types and functions require doc comments (`///`).
- Do not add `#[allow(dead_code)]` without a comment explaining why the code must stay.
- Unused imports must be removed before merging.
- Feature-gated code that is no longer used should be deleted, not suppressed.

### Container environment variable conventions

- **Seed injection deduplication**: A `StellarNode` may configure its validator seed via the legacy `spec.validatorConfig.seedSecretRef` (a plain Kubernetes Secret reference) or the newer `spec.validatorConfig.seedSecretSource` (KMS/ESO/CSI/Vault-backed). Both paths inject an environment variable named `STELLAR_CORE_SEED` into the pod spec. To prevent the API server from rejecting the pod due to duplicate environment variable names, the pod builder merges env vars **by name** using `merge_env_overrides` (see `src/controller/resources.rs`) instead of appending. The last writer wins, which gives `seedSecretSource` precedence over `seedSecretRef` — matching the precedence in `ValidatorConfig::resolve_seed_source()`. If both fields are set, only one `STELLAR_CORE_SEED` entry appears in the rendered pod spec, sourced from `seedSecretSource`.
- **No hard rejection**: The operator does **not** reject a CR that sets both `seedSecretRef` and `seedSecretSource`; it silently deduplicates. This preserves backward compatibility with existing clusters that may have both fields populated during migration.
- **Auditing other env vars**: The same `merge_env_overrides` mechanism is used for `stellarCoreEnv` (Validator), `horizonEnv` (Horizon), and any custom env vars injected via CSI/Vault. Contributors adding new env var injection paths **must** route them through `merge_env_overrides` (or `build_container` for the legacy `seedSecretRef` path) rather than using `Vec::extend` on the container's `env` list. A property-based test in `src/controller/seed_env_dedupe_test.rs` asserts uniqueness of all env var names across all three node types.

### Documentation conventions

- Documentation files use `kebab-case.md` (e.g., `disk-scaling.md`).
- Files that belong to a topic area go in the matching `docs/<topic>/` subdirectory.
- Root-level docs (`README.md`, `DEVELOPMENT.md`, `CONTRIBUTING.md`) are entry points only — detailed content belongs in `docs/`.
- New doc files must be added to `mkdocs.yml` under the appropriate section.

### Script conventions

- Scripts use `kebab-case.sh` (e.g., `cleanup.sh`).
- Every script must pass `shellcheck -S error`.
- Do not add one-off archive or batch scripts under `scripts/`. Use
  `scripts/cleanup.sh` (`make cleanup`) as the single cleanup entrypoint, or
  remove obsolete helpers entirely.
- Historical or one-off scripts should not be committed to the repository; keep only operational scripts under `scripts/`.

### Manifest and config conventions

- CRD YAML files follow the `stellar{feature}-crd.yaml` naming pattern under `config/crd/`.
- Example manifests in `examples/` use descriptive, feature-based names — not issue numbers.
- Generated manifests (CRDs, API reference, bundle) must be regenerated from their source before merg

/* … truncated 3674 chars — edit only what you need near the top … */
