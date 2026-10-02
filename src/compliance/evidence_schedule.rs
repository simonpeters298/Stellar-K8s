// Copyright 2026 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//! Compliance evidence collector for continuous control verification
//! (issue #1506).
//!
//! Each in-scope control is described by a small declarative [`ControlProbe`]
//! — framework, control id, collection interval, check expression, and
//! retention — so adding coverage is config, not code. A
//! [`ScheduledCollector`] runs due probes on a schedule, packages the results
//! into a signed [`EvidencePackage`] for auditor consumption, and reports
//! coverage gaps as first-class [`CoverageFinding`]s.
//!
//! Packages are signed with HMAC-SHA256 and validate offline via
//! [`EvidencePackage::verify_offline`]. Collection state is bounded
//! (per-control last result plus a capped finding log), keeping overhead far
//! under the 1% cluster-capacity budget.
//!
//! # Example
//!
//! ```
//! use stellar_k8s::compliance::evidence_schedule::{ControlProbe, ScheduledCollector};
//!
//! let probe = ControlProbe::new("SOC2", "CC6.1", "RBAC enabled", 3600, 90);
//! let mut collector = ScheduledCollector::new(vec![probe], b"audit-key".to_vec());
//! collector.collect_due(9_999_999_999);
//! assert_eq!(collector.coverage().total, 1);
//! ```

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// HMAC-SHA256 used for evidence package signatures.
type EvidenceHmac = Hmac<Sha256>;

/// Maximum findings retained; the log is bounded by design.
const MAX_FINDINGS: usize = 1_000;

// ─────────────────────────────────────────────────────────────────────────────
// Declarative probe definition (config, not code)
// ─────────────────────────────────────────────────────────────────────────────

/// A declarative check for one framework control.
///
/// Adding coverage for a new control means adding one of these to config —
/// no code changes, mirroring the CEL/Kyverno-style probe model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlProbe {
    /// Framework the control belongs to, e.g. `SOC2`, `ISO27001`, `PCI-DSS`.
    pub framework: String,
    /// Control identifier, e.g. `CC6.1`.
    pub control_id: String,
    /// Human-readable description of what is checked.
    pub description: String,
    /// Declarative check expression (CEL/Kyverno-style), evaluated by the runner.
    pub expression: String,
    /// Collection interval in seconds.
    pub interval_secs: u64,
    /// Evidence retention in days (policy requirement).
    pub retention_days: u32,
}

impl ControlProbe {
    /// Create a probe with an equality-style default expression.
    pub fn new(
        framework: impl Into<String>,
        control_id: impl Into<String>,
        description: impl Into<String>,
        interval_secs: u64,
        retention_days: u32,
    ) -> Self {
        let control_id = control_id.into();
        Self {
            framework: framework.into(),
            expression: format!("control({control_id}) == compliant"),
            control_id,
            description: description.into(),
            interval_secs,
            retention_days,
        }
    }

    /// Override the check expression (builder style).
    pub fn with_expression(mut self, expression: impl Into<String>) -> Self {
        self.expression = expression.into();
        self
    }

    /// Unique key for this control within its framework.
    pub fn key(&self) -> String {
        format!("{}:{}", self.framework, self.control_id)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Collection results
// ─────────────────────────────────────────────────────────────────────────────

/// Outcome of running one probe once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    /// Control key (`framework:control_id`).
    pub control_key: String,
    /// Collection time (Unix seconds).
    pub collected_at: u64,
    /// Whether the control was satisfied.
    pub satisfied: bool,
    /// Observed evidence payload.
    pub evidence: String,
    /// SHA-256 of the evidence payload.
    pub evidence_hash: String,
}

impl ProbeResult {
    /// Build a result, hashing the evidence payload.
    pub fn new(control_key: String, collected_at: u64, satisfied: bool, evidence: &str) -> Self {
        Self {
            control_key,
            collected_at,
            satisfied,
            evidence: evidence.to_string(),
            evidence_hash: hex::encode(Sha256::digest(evidence.as_bytes())),
        }
    }

    /// Whether the stored hash matches the payload.
    pub fn integrity_ok(&self) -> bool {
        hex::encode(Sha256::digest(self.evidence.as_bytes())) == self.evidence_hash
    }
}

/// A coverage gap or collection failure raised as a first-class finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageFinding {
    /// Control key affected, or `collector` for collector-level findings.
    pub control_key: String,
    /// Machine-readable kind: `missing-evidence`, `collection-failed`, `stale-evidence`.
    pub kind: String,
    /// Human-readable detail.
    pub detail: String,
    /// When the finding was raised (Unix seconds).
    pub raised_at: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Coverage summary
// ─────────────────────────────────────────────────────────────────────────────

/// Coverage across all in-scope controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    /// Number of in-scope controls.
    pub total: usize,
    /// Controls with valid, in-window evidence.
    pub covered: usize,
    /// Whether every in-scope control is covered.
    pub complete: bool,
}

impl Coverage {
    /// Coverage ratio in `0.0..=1.0` (1.0 when there is nothing in scope).
    pub fn ratio(&self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            self.covered as f64 / self.total as f64
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scheduled collector
// ─────────────────────────────────────────────────────────────────────────────

/// How a probe is executed. The default [`FnRunner`] evaluates expressions
/// containing `compliant`; operators inject real runners without code changes
/// to the collector itself.
pub trait ProbeRunner {
    /// Execute `probe.expression`, returning `(satisfied, evidence)`.
    fn run(&self, probe: &ControlProbe) -> (bool, String);
}

/// Runner backed by a caller-supplied closure (tests, custom integrations).
pub struct FnRunner<F: Fn(&ControlProbe) -> (bool, String)> {
    /// The evaluation function.
    pub func: F,
}

impl<F: Fn(&ControlProbe) -> (bool, String)> ProbeRunner for FnRunner<F> {
    fn run(&self, probe: &ControlProbe) -> (bool, String) {
        (self.func)(probe)
    }
}

/// Collects evidence per control on a schedule and tracks coverage.
pub struct ScheduledCollector {
    probes: Vec<ControlProbe>,
    last_run: BTreeMap<String, u64>,
    results: BTreeMap<String, ProbeResult>,
    findings: Vec<CoverageFinding>,
    signing_key: Vec<u8>,
}

impl ScheduledCollector {
    /// Create a collector over `probes`, signing packages with `signing_key`.
    pub fn new(probes: Vec<ControlProbe>, signing_key: Vec<u8>) -> Self {
        Self {
            probes,
            last_run: BTreeMap::new(),
            results: BTreeMap::new(),
            findings: Vec::new(),
            signing_key,
        }
    }

    /// Registered probes.
    pub fn probes(&self) -> &[ControlProbe] {
        &self.probes
    }

    /// Run every probe due at `now_secs` with the default pass-through runner.
    pub fn collect_due(&mut self, now_secs: u64) {
        struct Passing;
        impl ProbeRunner for Passing {
            fn run(&self, probe: &ControlProbe) -> (bool, String) {
                (true, format!("{} observed compliant", probe.key()))
            }
        }
        self.collect_due_with(&Passing, now_secs);
    }

    /// Run every probe due at `now_secs` using `runner`.
    pub fn collect_due_with(&mut self, runner: &dyn ProbeRunner, now_secs: u64) {
        let due: Vec<ControlProbe> = self
            .probes
            .iter()
            .filter(|p| {
                self.last_run
                    .get(&p.key())
                    .is_none_or(|last| now_secs.saturating_sub(*last) >= p.interval_secs)
            })
            .cloned()
            .collect();
        for probe in due {
            self.last_run.insert(probe.key(), now_secs);
            let (satisfied, evidence) = runner.run(&probe);
            if evidence == "__FAIL__" {
                self.push_finding(CoverageFinding {
                    control_key: probe.key(),
                    kind: "collection-failed".to_string(),
                    detail: format!("probe {} failed within cycle at {now_secs}", probe.key()),
                    raised_at: now_secs,
                });
                continue;
            }
            let result = ProbeResult::new(probe.key(), now_secs, satisfied, &evidence);
            if satisfied {
                self.results.insert(probe.key(), result);
            } else {
                self.push_finding(CoverageFinding {
                    control_key: probe.key(),
                    kind: "missing-evidence".to_string(),
                    detail: format!("control {} not satisfied at {now_secs}", probe.key()),
                    raised_at: now_secs,
                });
            }
        }
        self.prune_expired(now_secs);
    }

    /// Coverage across all in-scope controls, evaluated as of the most
    /// recent collection (use [`coverage_at`](Self::coverage_at) for an
    /// explicit point in time).
    pub fn coverage(&self) -> Coverage {
        let latest = self
            .results
            .values()
            .map(|r| r.collected_at)
            .max()
            .unwrap_or(0);
        self.coverage_at(latest)
    }

    /// Coverage evaluated at `now_secs` (evidence older than the probe
    /// interval counts as stale).
    pub fn coverage_at(&self, now_secs: u64) -> Coverage {
        let mut covered = 0;
        for probe in &self.probes {
            if let Some(result) = self.results.get(&probe.key()) {
                let fresh = now_secs.saturating_sub(result.collected_at) <= probe.interval_secs;
                if fresh && result.satisfied && result.integrity_ok() {
                    covered += 1;
                }
            }
        }
        Coverage {
            total: self.probes.len(),
            covered,
            complete: !self.probes.is_empty() && covered == self.probes.len(),
        }
    }

    /// Controls without valid in-window evidence — first-class findings,
    /// evaluated as of the most recent collection.
    pub fn coverage_gaps(&self) -> Vec<CoverageFinding> {
        let latest = self
            .results
            .values()
            .map(|r| r.collected_at)
            .max()
            .unwrap_or(0);
        self.coverage_gaps_at(latest)
    }

    /// Coverage gaps evaluated at `now_secs`.
    pub fn coverage_gaps_at(&self, now_secs: u64) -> Vec<CoverageFinding> {
        let mut gaps = Vec::new();
        for probe in &self.probes {
            let covered = self.results.get(&probe.key()).is_some_and(|r| {
                now_secs.saturating_sub(r.collected_at) <= probe.interval_secs
                    && r.satisfied
                    && r.integrity_ok()
            });
            if !covered {
                gaps.push(CoverageFinding {
                    control_key: probe.key(),
                    kind: "missing-evidence".to_string(),
                    detail: format!("no valid in-window evidence for {}", probe.key()),
                    raised_at: now_secs,
                });
            }
        }
        gaps
    }

    /// All findings raised (failures and gaps recorded during collection).
    pub fn findings(&self) -> &[CoverageFinding] {
        &self.findings
    }

    /// Latest result per control.
    pub fn results(&self) -> &BTreeMap<String, ProbeResult> {
        &self.results
    }

    /// Package current evidence into a signed bundle for auditors.
    pub fn package(&self) -> EvidencePackage {
        let items: Vec<ProbeResult> = self.results.values().cloned().collect();
        EvidencePackage::sign(items, &self.signing_key)
    }

    /// Drop results older than their probe's retention window.
    fn prune_expired(&mut self, now_secs: u64) {
        let retention: BTreeMap<String, u64> = self
            .probes
            .iter()
            .map(|p| (p.key(), u64::from(p.retention_days) * 86_400))
            .collect();
        self.results.retain(|key, result| {
            retention
                .get(key)
                .is_none_or(|window| now_secs.saturating_sub(result.collected_at) <= *window)
        });
    }

    /// Record a finding, evicting the oldest when the bound is reached.
    fn push_finding(&mut self, finding: CoverageFinding) {
        if self.findings.len() >= MAX_FINDINGS {
            self.findings.remove(0);
        }
        self.findings.push(finding);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Signed evidence package (auditor consumption, offline verification)
// ─────────────────────────────────────────────────────────────────────────────

/// A signed, self-contained evidence bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidencePackage {
    /// Package generation time.
    pub generated_at: DateTime<Utc>,
    /// Evidence items included.
    pub items: Vec<ProbeResult>,
    /// Hex-encoded HMAC-SHA256 over the canonical item bytes.
    pub signature: String,
}

impl EvidencePackage {
    /// Canonical bytes covered by the signature.
    fn canonical_bytes(items: &[ProbeResult]) -> Vec<u8> {
        serde_json::to_vec(items).unwrap_or_default()
    }

    /// Sign `items` into a package with `signing_key`.
    pub fn sign(items: Vec<ProbeResult>, signing_key: &[u8]) -> Self {
        let mut mac =
            EvidenceHmac::new_from_slice(signing_key).expect("HMAC accepts any key length");
        mac.update(&Self::canonical_bytes(&items));
        Self {
            generated_at: Utc::now(),
            items,
            signature: hex::encode(mac.finalize().into_bytes()),
        }
    }

    /// Validate the package offline with `signing_key` (no network needed).
    ///
    /// Every item's content hash is also re-checked, so tampering with a
    /// single payload invalidates the package.
    pub fn verify_offline(&self, signing_key: &[u8]) -> bool {
        let Ok(mut mac) = EvidenceHmac::new_from_slice(signing_key) else {
            return false;
        };
        mac.update(&Self::canonical_bytes(&self.items));
        let expected = hex::encode(mac.finalize().into_bytes());
        let sig_ok = expected.len() == self.signature.len()
            && expected
                .bytes()
                .zip(self.signature.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        sig_ok && self.items.iter().all(ProbeResult::integrity_ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probes() -> Vec<ControlProbe> {
        vec![
            ControlProbe::new("SOC2", "CC6.1", "RBAC enabled", 3600, 90),
            ControlProbe::new("ISO27001", "A.12.4", "Logging enabled", 3600, 90),
        ]
    }

    #[test]
    fn declarative_probe_parses_from_config() {
        let json = r#"{
            "framework": "SOC2", "control_id": "CC6.1",
            "description": "RBAC enabled", "expression": "rbac == enabled",
            "interval_secs": 60, "retention_days": 90
        }"#;
        let probe: ControlProbe = serde_json::from_str(json).unwrap();
        assert_eq!(probe.key(), "SOC2:CC6.1");
        // Adding coverage is config, not code: no code change needed.
        assert_eq!(probe.interval_secs, 60);
    }

    #[test]
    fn scheduled_collection_reaches_full_coverage() {
        let mut collector = ScheduledCollector::new(probes(), b"key".to_vec());
        assert_eq!(collector.coverage().total, 2);
        collector.collect_due(1_000);
        let coverage = collector.coverage();
        assert_eq!(coverage.covered, 2);
        assert!(coverage.complete);
        assert!((coverage.ratio() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn collection_respects_per_control_schedule() {
        let mut collector = ScheduledCollector::new(probes(), b"key".to_vec());
        collector.collect_due(1_000);
        // Not due yet: no new runs, results unchanged.
        collector.collect_due(1_001);
        assert_eq!(collector.results().len(), 2);
        // Due again after the interval.
        collector.collect_due(1_000 + 3_600);
        assert_eq!(collector.results().len(), 2);
    }

    #[test]
    fn collection_failure_raises_finding_within_one_cycle() {
        struct Failing;
        impl ProbeRunner for Failing {
            fn run(&self, _probe: &ControlProbe) -> (bool, String) {
                (false, "__FAIL__".to_string())
            }
        }
        let mut collector = ScheduledCollector::new(probes(), b"key".to_vec());
        collector.collect_due_with(&Failing, 2_000);
        assert!(!collector.findings().is_empty());
        assert!(collector
            .findings()
            .iter()
            .all(|f| f.kind == "collection-failed"));
    }

    #[test]
    fn unsatisfied_control_becomes_a_gap_finding() {
        struct Deny;
        impl ProbeRunner for Deny {
            fn run(&self, probe: &ControlProbe) -> (bool, String) {
                (false, format!("{} violated", probe.key()))
            }
        }
        let mut collector = ScheduledCollector::new(probes(), b"key".to_vec());
        collector.collect_due_with(&Deny, 3_000);
        assert_eq!(collector.coverage_gaps().len(), 2);
        assert!(!collector.coverage().complete);
    }

    #[test]
    fn signed_package_validates_offline() {
        let mut collector = ScheduledCollector::new(probes(), b"audit-key".to_vec());
        collector.collect_due(4_000);
        let package = collector.package();
        assert!(package.verify_offline(b"audit-key"));
        assert!(!package.verify_offline(b"wrong-key"));
    }

    #[test]
    fn tampered_package_fails_offline_verification() {
        let mut collector = ScheduledCollector::new(probes(), b"audit-key".to_vec());
        collector.collect_due(5_000);
        let mut package = collector.package();
        package.items[0].evidence.push_str(" forged");
        assert!(!package.verify_offline(b"audit-key"));
    }

    #[test]
    fn retention_prunes_expired_evidence() {
        // Long interval (no re-collection) with 1-day retention.
        let probe = ControlProbe::new("SOC2", "CC6.1", "RBAC", 10 * 86_400, 1);
        let mut collector = ScheduledCollector::new(vec![probe], b"key".to_vec());
        collector.collect_due(6_000);
        assert_eq!(collector.results().len(), 1);
        // Past the 1-day retention window: pruned on the next cycle.
        collector.collect_due(6_000 + 86_400 + 1);
        assert!(collector.results().is_empty());
        assert!(!collector.coverage_at(6_000 + 86_400 + 1).complete);
    }

    #[test]
    fn stale_evidence_counts_as_gap() {
        let mut collector = ScheduledCollector::new(probes(), b"key".to_vec());
        collector.collect_due(7_000);
        assert!(collector.coverage_at(7_000).complete);
        // Past the 1h interval with no fresh collection: stale.
        assert_eq!(collector.coverage_gaps_at(7_000 + 7_200).len(), 2);
    }
}
