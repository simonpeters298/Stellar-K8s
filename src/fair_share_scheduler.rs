// Copyright 2024 Stellar-K8s Contributors
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

//! # Declarative Multi-Tenant Quota & Fair-Share Scheduler Extension (Epic #1480)
//!
//! Implements Dominant Resource Fairness (DRF) and weighted fair-share
//! scheduling for multi-tenant Kubernetes clusters running Stellar nodes.
//! Contains noisy neighbors, prevents resource starvation under contention,
//! calculates Jain's Fairness Index, and enforces declarative multi-tenant quotas.
//!
//! ## Architecture
//!
//! ```text
//! Incoming Workload (Pod / StellarNode)
//!    │
//!    ▼
//! Admission Webhook (Quota Check)
//!    ├── Check hard CPU / Memory limits
//!    └── Check validator / pod counts
//!    │
//!    ▼
//! Fair-Share Scheduler Extension (DRF Queue & Scoring)
//!    ├── Compute per-tenant normalized resource shares: (s_cpu, s_mem)
//!    ├── Weighted Dominant Share: D_t = max(s_cpu, s_mem) / weight_t
//!    ├── Order pending queue by ascending dominant share (lowest D_t first)
//!    ├── Fair-Share Node Placement Score [0 - 100]
//!    └── Jain's Fairness Index calculation (J >= 0.90 target)
//! ```

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};

/// Multi-tenant declarative resource quota definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenantResourceQuota {
    pub tenant_id: String,
    /// Priority weighting for fair-share (e.g. Free: 1.0, Basic: 2.0, Enterprise: 5.0).
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// Maximum CPU limit in cores (e.g. 16.0).
    pub max_cpu_cores: f64,
    /// Maximum Memory limit in bytes (e.g. 64 GiB).
    pub max_memory_bytes: u64,
    /// Maximum validator nodes allowed for this tenant.
    pub max_validators: u32,
    /// Maximum total Horizon / RPC pods allowed.
    pub max_total_pods: u32,
    /// Minimum guaranteed share percentage [0.0 - 100.0].
    #[serde(default = "default_guaranteed_share")]
    pub guaranteed_share_percent: f64,
}

fn default_weight() -> f64 {
    1.0
}
fn default_guaranteed_share() -> f64 {
    10.0
}

/// Total available cluster capacity across nodes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClusterCapacity {
    pub total_cpu_cores: f64,
    pub total_memory_bytes: u64,
}

impl Default for ClusterCapacity {
    fn default() -> Self {
        Self {
            total_cpu_cores: 64.0,
            total_memory_bytes: 256 * 1024 * 1024 * 1024, // 256 GiB
        }
    }
}

/// Current active usage for a tenant.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantUsage {
    pub allocated_cpu_cores: f64,
    pub allocated_memory_bytes: u64,
    pub active_validators: u32,
    pub active_pods: u32,
}

/// Dominant Resource Fairness share snapshot for a tenant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantDrfShare {
    pub tenant_id: String,
    pub weight: f64,
    pub cpu_share: f64,
    pub memory_share: f64,
    pub dominant_share: f64,
    pub weighted_dominant_share: f64,
}

/// Fair-share scheduling queue priority decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchedulingPriorityDecision {
    pub tenant_id: String,
    pub priority_score: i32, // 0 - 100
    pub weighted_dominant_share: f64,
    pub admits: bool,
    pub reason: String,
}

/// Core Fair-Share Scheduler Engine implementing DRF.
pub struct FairShareSchedulerEngine {
    cluster_capacity: ClusterCapacity,
    quotas: HashMap<String, TenantResourceQuota>,
    usages: HashMap<String, TenantUsage>,
}

impl FairShareSchedulerEngine {
    pub fn new(capacity: ClusterCapacity) -> Self {
        Self {
            cluster_capacity: capacity,
            quotas: HashMap::new(),
            usages: HashMap::new(),
        }
    }

    /// Set or update the declarative quota for a tenant.
    pub fn set_quota(&mut self, quota: TenantResourceQuota) {
        self.quotas.insert(quota.tenant_id.clone(), quota);
    }

    /// Update current resource usage for a tenant.
    pub fn update_usage(&mut self, tenant_id: &str, usage: TenantUsage) {
        self.usages.insert(tenant_id.to_string(), usage);
    }

    /// Update cluster capacity.
    pub fn set_cluster_capacity(&mut self, capacity: ClusterCapacity) {
        self.cluster_capacity = capacity;
    }

    /// Compute Dominant Resource Fairness share for a tenant.
    pub fn compute_drf_share(&self, tenant_id: &str) -> TenantDrfShare {
        let usage = self.usages.get(tenant_id).cloned().unwrap_or_default();
        let quota = self.quotas.get(tenant_id);
        let weight = quota.map(|q| q.weight.max(0.1)).unwrap_or(1.0);

        let cpu_share = if self.cluster_capacity.total_cpu_cores > 0.0 {
            usage.allocated_cpu_cores / self.cluster_capacity.total_cpu_cores
        } else {
            0.0
        };

        let memory_share = if self.cluster_capacity.total_memory_bytes > 0 {
            usage.allocated_memory_bytes as f64 / self.cluster_capacity.total_memory_bytes as f64
        } else {
            0.0
        };

        let dominant_share = cpu_share.max(memory_share);
        let weighted_dominant_share = dominant_share / weight;

        TenantDrfShare {
            tenant_id: tenant_id.to_string(),
            weight,
            cpu_share,
            memory_share,
            dominant_share,
            weighted_dominant_share,
        }
    }

    /// Compute scheduling score (0 - 100) for an unscheduled pod.
    /// Lower dominant share yields higher priority score.
    pub fn score_pod_for_tenant(&self, tenant_id: &str) -> SchedulingPriorityDecision {
        let quota = match self.quotas.get(tenant_id) {
            Some(q) => q,
            None => {
                // Unregistered tenant receives lowest priority
                return SchedulingPriorityDecision {
                    tenant_id: tenant_id.to_string(),
                    priority_score: 10,
                    weighted_dominant_share: 1.0,
                    admits: true,
                    reason: "Default priority for unconfigured tenant".to_string(),
                };
            }
        };

        let usage = self.usages.get(tenant_id).cloned().unwrap_or_default();

        // Check hard limits
        if usage.allocated_cpu_cores >= quota.max_cpu_cores
            || usage.allocated_memory_bytes >= quota.max_memory_bytes
            || usage.active_pods >= quota.max_total_pods
        {
            return SchedulingPriorityDecision {
                tenant_id: tenant_id.to_string(),
                priority_score: 0,
                weighted_dominant_share: 1.0,
                admits: false,
                reason: format!("Hard quota exceeded for tenant '{tenant_id}'"),
            };
        }

        let drf = self.compute_drf_share(tenant_id);

        // Score is inversely proportional to weighted dominant share:
        // A tenant using 0% dominant share gets score 100.
        // A tenant using 100% of fair share gets score 0.
        let raw_score = (100.0 * (1.0 - drf.weighted_dominant_share)).round() as i32;
        let priority_score = raw_score.clamp(1, 100);

        SchedulingPriorityDecision {
            tenant_id: tenant_id.to_string(),
            priority_score,
            weighted_dominant_share: drf.weighted_dominant_share,
            admits: true,
            reason: format!(
                "DRF fair share priority: dominant={:.1}%, weighted={:.3}",
                drf.dominant_share * 100.0,
                drf.weighted_dominant_share
            ),
        }
    }

    /// Validate admission for a new workload request against tenant quota.
    pub fn validate_admission(
        &self,
        tenant_id: &str,
        req_cpu: f64,
        req_mem: u64,
        is_validator: bool,
    ) -> Result<()> {
        let quota = self.quotas.get(tenant_id).ok_or_else(|| {
            Error::Validation(format!(
                "Unknown tenant '{tenant_id}' has no configured quota"
            ))
        })?;

        let usage = self.usages.get(tenant_id).cloned().unwrap_or_default();

        if usage.allocated_cpu_cores + req_cpu > quota.max_cpu_cores {
            return Err(Error::Validation(format!(
                "Tenant '{tenant_id}' CPU quota exceeded: currently using {:.1} cores, requesting {:.1}, limit is {:.1}",
                usage.allocated_cpu_cores, req_cpu, quota.max_cpu_cores
            )));
        }

        if usage.allocated_memory_bytes + req_mem > quota.max_memory_bytes {
            return Err(Error::Validation(format!(
                "Tenant '{tenant_id}' Memory quota exceeded: currently using {} bytes, requesting {}, limit is {}",
                usage.allocated_memory_bytes, req_mem, quota.max_memory_bytes
            )));
        }

        if is_validator && usage.active_validators + 1 > quota.max_validators {
            return Err(Error::Validation(format!(
                "Tenant '{tenant_id}' Validator quota exceeded: currently has {}, limit is {}",
                usage.active_validators, quota.max_validators
            )));
        }

        if usage.active_pods + 1 > quota.max_total_pods {
            return Err(Error::Validation(format!(
                "Tenant '{tenant_id}' Pod count quota exceeded: currently has {}, limit is {}",
                usage.active_pods, quota.max_total_pods
            )));
        }

        Ok(())
    }

    /// Compute Jain's Fairness Index across all active tenants.
    /// J = (sum x_i)^2 / (N * sum (x_i^2))
    pub fn jain_fairness_index(&self) -> f64 {
        let shares: Vec<f64> = self
            .usages
            .keys()
            .map(|t| self.compute_drf_share(t).weighted_dominant_share)
            .filter(|&s| s > 0.0)
            .collect();

        if shares.is_empty() {
            return 1.0;
        }

        let n = shares.len() as f64;
        let sum: f64 = shares.iter().sum();
        let sum_sq: f64 = shares.iter().map(|s| s * s).sum();

        if sum_sq == 0.0 {
            1.0
        } else {
            let index = (sum * sum) / (n * sum_sq);
            index.clamp(0.0, 1.0)
        }
    }
}

/// Shared multi-tenant scheduler manager for Kubernetes integration.
#[derive(Clone)]
pub struct MultiTenantSchedulerManager {
    inner: Arc<RwLock<FairShareSchedulerEngine>>,
}

impl MultiTenantSchedulerManager {
    pub fn new(capacity: ClusterCapacity) -> Self {
        Self {
            inner: Arc::new(RwLock::new(FairShareSchedulerEngine::new(capacity))),
        }
    }

    pub async fn set_quota(&self, quota: TenantResourceQuota) {
        self.inner.write().await.set_quota(quota);
    }

    pub async fn update_usage(&self, tenant_id: &str, usage: TenantUsage) {
        self.inner.write().await.update_usage(tenant_id, usage);
    }

    pub async fn score_tenant_pod(&self, tenant_id: &str) -> SchedulingPriorityDecision {
        self.inner.read().await.score_pod_for_tenant(tenant_id)
    }

    pub async fn validate_admission(
        &self,
        tenant_id: &str,
        req_cpu: f64,
        req_mem: u64,
        is_validator: bool,
    ) -> Result<()> {
        self.inner
            .read()
            .await
            .validate_admission(tenant_id, req_cpu, req_mem, is_validator)
    }

    pub async fn jain_fairness_index(&self) -> f64 {
        self.inner.read().await.jain_fairness_index()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drf_share_and_priority() {
        let capacity = ClusterCapacity {
            total_cpu_cores: 100.0,
            total_memory_bytes: 100 * 1024 * 1024 * 1024,
        };
        let mut engine = FairShareSchedulerEngine::new(capacity);

        // Tenant A: Weight 1.0, using 10% CPU and 20% Mem -> dominant = 20%
        engine.set_quota(TenantResourceQuota {
            tenant_id: "tenant-a".to_string(),
            weight: 1.0,
            max_cpu_cores: 50.0,
            max_memory_bytes: 50 * 1024 * 1024 * 1024,
            max_validators: 5,
            max_total_pods: 20,
            guaranteed_share_percent: 10.0,
        });
        engine.update_usage(
            "tenant-a",
            TenantUsage {
                allocated_cpu_cores: 10.0,
                allocated_memory_bytes: 20 * 1024 * 1024 * 1024,
                active_validators: 1,
                active_pods: 5,
            },
        );

        // Tenant B: Weight 2.0 (higher tier), using 10% CPU and 20% Mem -> dominant = 20% / 2.0 = 10%
        engine.set_quota(TenantResourceQuota {
            tenant_id: "tenant-b".to_string(),
            weight: 2.0,
            max_cpu_cores: 50.0,
            max_memory_bytes: 50 * 1024 * 1024 * 1024,
            max_validators: 5,
            max_total_pods: 20,
            guaranteed_share_percent: 20.0,
        });
        engine.update_usage(
            "tenant-b",
            TenantUsage {
                allocated_cpu_cores: 10.0,
                allocated_memory_bytes: 20 * 1024 * 1024 * 1024,
                active_validators: 1,
                active_pods: 5,
            },
        );

        let p_a = engine.score_pod_for_tenant("tenant-a");
        let p_b = engine.score_pod_for_tenant("tenant-b");

        // Tenant B has higher weight, so its weighted dominant share is lower, earning higher priority score!
        assert!(p_b.priority_score > p_a.priority_score);
    }

    #[test]
    fn test_admission_quota_enforcement() {
        let mut engine = FairShareSchedulerEngine::new(ClusterCapacity::default());
        engine.set_quota(TenantResourceQuota {
            tenant_id: "tenant-c".to_string(),
            weight: 1.0,
            max_cpu_cores: 4.0,
            max_memory_bytes: 8 * 1024 * 1024 * 1024,
            max_validators: 1,
            max_total_pods: 3,
            guaranteed_share_percent: 10.0,
        });
        engine.update_usage(
            "tenant-c",
            TenantUsage {
                allocated_cpu_cores: 3.5,
                allocated_memory_bytes: 6 * 1024 * 1024 * 1024,
                active_validators: 1,
                active_pods: 2,
            },
        );

        // Requesting 1.0 CPU will exceed 4.0 limit (3.5 + 1.0 = 4.5)
        let res1 = engine.validate_admission("tenant-c", 1.0, 1024, false);
        assert!(res1.is_err());
        assert!(res1.unwrap_err().to_string().contains("CPU quota exceeded"));

        // Requesting second validator node when limit is 1
        let res2 = engine.validate_admission("tenant-c", 0.1, 1024, true);
        assert!(res2.is_err());
        assert!(res2
            .unwrap_err()
            .to_string()
            .contains("Validator quota exceeded"));

        // Valid small request succeeds
        let res3 = engine.validate_admission("tenant-c", 0.2, 512, false);
        assert!(res3.is_ok());
    }

    #[test]
    fn test_jains_fairness_index() {
        let capacity = ClusterCapacity {
            total_cpu_cores: 100.0,
            total_memory_bytes: 100 * 1024 * 1024 * 1024,
        };
        let mut engine = FairShareSchedulerEngine::new(capacity);

        // 3 equal tenants with identical allocations
        for t in ["t1", "t2", "t3"] {
            engine.set_quota(TenantResourceQuota {
                tenant_id: t.to_string(),
                weight: 1.0,
                max_cpu_cores: 50.0,
                max_memory_bytes: 50 * 1024 * 1024 * 1024,
                max_validators: 5,
                max_total_pods: 20,
                guaranteed_share_percent: 33.3,
            });
            engine.update_usage(
                t,
                TenantUsage {
                    allocated_cpu_cores: 20.0,
                    allocated_memory_bytes: 20 * 1024 * 1024 * 1024,
                    active_validators: 1,
                    active_pods: 5,
                },
            );
        }

        let jain = engine.jain_fairness_index();
        assert!((jain - 1.0).abs() < 0.001);
    }
}
