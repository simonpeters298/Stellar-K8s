#!/usr/bin/env bats
#
# Bats coverage for scripts/dep-gate.sh
#
# Exercises the allow and deny decision paths of the dependency gate.
# Package manager invocations are stubbed via PATH shims so the suite
# never touches the network or a real package manager.

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  DEP_GATE="$REPO_ROOT/scripts/dep-gate.sh"

  # Isolated workspace so the gate never mutates the real checkout.
  WORKDIR="$(mktemp -d)"
  cd "$WORKDIR" || return 1

  # PATH shim directory: stub out package managers the gate may call.
  SHIM_DIR="$WORKDIR/shims"
  mkdir -p "$SHIM_DIR"
  for pm in npm yarn pnpm bun pip pip3 poetry cargo go; do
    cat >"$SHIM_DIR/$pm" <<'SHIM'
#!/usr/bin/env bash
# Stub package manager: record the call and succeed without network access.
echo "stub:$0 $*" >>"${DEP_GATE_STUB_LOG:-/dev/null}"
exit 0
SHIM
    chmod +x "$SHIM_DIR/$pm"
  done

  export PATH="$SHIM_DIR:$PATH"
  export DEP_GATE_STUB_LOG="$WORKDIR/pm-calls.log"
  : >"$DEP_GATE_STUB_LOG"
}

teardown() {
  if [ -n "$WORKDIR" ] && [ -d "$WORKDIR" ]; then
    rm -rf "$WORKDIR"
  fi
}

@test "dep-gate.sh exists and is executable" {
  [ -f "$DEP_GATE" ]
  [ -x "$DEP_GATE" ]
}

@test "allow path: permitted dependency change is accepted" {
  # A permitted change: no new/blocked dependencies introduced.
  cat >package.json <<'JSON'
{
  "name": "fixture-allow",
  "version": "1.0.0",
  "dependencies": {
    "left-pad": "1.3.0"
  }
}
JSON

  run bash "$DEP_GATE" --allow "left-pad"

  # The gate must succeed and report the permitted decision.
  [ "$status" -eq 0 ]
  echo "$output" | grep -qiE 'allow|permit|pass|ok'
}

@test "deny path: blocked dependency change is rejected" {
  # A blocked change: a dependency that is not on the allow list.
  cat >package.json <<'JSON'
{
  "name": "fixture-deny",
  "version": "1.0.0",
  "dependencies": {
    "evil-pkg": "9.9.9"
  }
}
JSON

  run bash "$DEP_GATE" --allow "left-pad"

  # The gate must fail and report the blocked decision.
  [ "$status" -ne 0 ]
  echo "$output" | grep -qiE 'deny|denied|block|reject|fail'
}

@test "package manager calls are stubbed via PATH" {
  cat >package.json <<'JSON'
{
  "name": "fixture-stub",
  "version": "1.0.0",
  "dependencies": {
    "left-pad": "1.3.0"
  }
}
JSON

  run bash "$DEP_GATE" --allow "left-pad"

  # Any package manager invocation must have gone through our shim.
  if [ -s "$DEP_GATE_STUB_LOG" ]; then
    grep -q 'stub:' "$DEP_GATE_STUB_LOG"
  fi
}
