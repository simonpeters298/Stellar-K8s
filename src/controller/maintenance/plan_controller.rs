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
//! Kubernetes controller for [`MaintenancePlan`].
//!
//! Snapshots nodes/pods/PDBs, runs the shared engine, then applies the
//! recorded cordon/evict/prewarm actions. Does not replace
//! [`super::node_drain`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{Api, EvictParams, ListParams, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::events::{EventType, Recorder, Reporter};
use kube::runtime::watcher::Config;
use kube::{Client, Resource, ResourceExt};
use serde_json::json;
use tracing::{info, warn};

use super::plan_engine::{
    reconcile_plan, ClusterNode, ClusterOps, PdbSnapshot, SloSample, WorkloadPod,
};
use crate::crd::maintenance_plan::MaintenancePlan;
use crate::error::{Error, Result};

/// Shared state for the MaintenancePlan controller.
pub struct MaintenancePlanState {
    pub client: Client,
    pub reporter: Reporter,
}

#[derive(Clone, Debug)]
enum PendingOp {
    Prewarm { workload: String, buffer: u32 },
    Cordon { node: String },
    Uncordon { node: String },
    Evict { namespace: String, name: String },
    Recover { workload: String },
    Event { reason: String, message: String },
}

/// Start the MaintenancePlan loop. Missing CRD is non-fatal so existing
/// clusters keep running the historical node lifecycle unchanged.
pub async fn run_maintenance_plan_controller(client: Client, reporter: Reporter) -> Result<()> {
    let plans: Api<MaintenancePlan> = Api::all(client.clone());
    match plans.list(&Default::default()).await {
        Ok(_) => info!("MaintenancePlan CRD is available"),
        Err(e) => {
            warn!("MaintenancePlan CRD not installed; planned-maintenance orchestrator idle: {e}");
            return Ok(());
        }
    }

    let state = Arc::new(MaintenancePlanState { client, reporter });
    info!("Starting MaintenancePlan controller");
    Controller::new(plans, Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, state)
        .for_each(|_| async {})
        .await;
    Ok(())
}

fn error_policy(
    _obj: Arc<MaintenancePlan>,
    err: &Error,
    _ctx: Arc<MaintenancePlanState>,
) -> Action {
    warn!(error = %err, "MaintenancePlan reconcile error; retrying in 15s");
    Action::requeue(Duration::from_secs(15))
}

async fn reconcile(plan: Arc<MaintenancePlan>, ctx: Arc<MaintenancePlanState>) -> Result<Action> {
    let ns = plan.namespace().unwrap_or_else(|| "default".to_string());
    let name = plan.name_any();
    let spec = &plan.spec;
    let status = plan.status.clone().unwrap_or_default();
    let gen = plan.metadata.generation.unwrap_or(0);

    let mut ops = SnapshotOps::load(ctx.client.clone()).await?;
    let outcome = reconcile_plan(spec, &status, gen, &mut ops)?;
    apply_ops(&ctx, plan.as_ref(), &ops.pending).await?;

    let api: Api<MaintenancePlan> = Api::namespaced(ctx.client.clone(), &ns);
    let patch = json!({ "status": outcome.status });
    api.patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(Error::KubeError)?;

    Ok(Action::requeue(Duration::from_secs(
        outcome.requeue_seconds,
    )))
}

struct SnapshotOps {
    nodes: Vec<ClusterNode>,
    pods: Vec<WorkloadPod>,
    pdbs: Vec<PdbSnapshot>,
    start: Instant,
    last_cold_window_ms: u64,
    user_facing_errors: u64,
    pending: Vec<PendingOp>,
}

impl SnapshotOps {
    async fn load(client: Client) -> Result<Self> {
        let node_api: Api<Node> = Api::all(client.clone());
        let pod_api: Api<Pod> = Api::all(client.clone());
        let pdb_api: Api<PodDisruptionBudget> = Api::all(client.clone());

        let nodes = node_api
            .list(&ListParams::default())
            .await
            .map_err(Error::KubeError)?
            .items
            .into_iter()
            .map(|n| {
                let ready = n
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .map(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
                    .unwrap_or(false);
                ClusterNode {
                    name: n.name_any(),
                    labels: n.labels().clone(),
                    cordoned: n
                        .spec
                        .as_ref()
                        .and_then(|s| s.unschedulable)
                        .unwrap_or(false),
                    ready,
                }
            })
            .collect();

        let pods = pod_api
            .list(&ListParams::default())
            .await
            .map_err(Error::KubeError)?
            .items
            .into_iter()
            .filter_map(|p| {
                let node = p.spec.as_ref()?.node_name.clone()?;
                let owner_ds = p
                    .metadata
                    .owner_references
                    .as_ref()
                    .map(|ors| ors.iter().any(|o| o.kind == "DaemonSet"))
                    .unwrap_or(false);
                let workload = p
                    .labels()
                    .get("app.kubernetes.io/instance")
                    .or_else(|| p.labels().get("app"))
                    .cloned()
                    .unwrap_or_else(|| p.name_any());
                let ready = p
                    .status
                    .as_ref()
                    .and_then(|s| s.container_statuses.as_ref())
                    .map(|cs| cs.iter().all(|c| c.ready))
                    .unwrap_or(false);
                Some(WorkloadPod {
                    name: p.name_any(),
                    namespace: p.namespace().unwrap_or_else(|| "default".into()),
                    node,
                    workload,
                    ready,
                    daemon_set: owner_ds,
                })
            })
            .collect();

        let pdbs = pdb_api
            .list(&ListParams::default())
            .await
            .unwrap_or_default()
            .items
            .into_iter()
            .map(|pdb| {
                let workload = pdb
                    .spec
                    .as_ref()
                    .and_then(|s| s.selector.as_ref())
                    .and_then(|sel| sel.match_labels.as_ref())
                    .and_then(|m| {
                        m.get("app.kubernetes.io/instance")
                            .or_else(|| m.get("app"))
                            .cloned()
                    })
                    .unwrap_or_else(|| pdb.name_any());
                let status = pdb.status.as_ref();
                let current_healthy = status.map(|s| s.current_healthy as u32).unwrap_or(0);
                let allowed = status.map(|s| s.disruptions_allowed as u32).unwrap_or(0);
                let min_available = current_healthy.saturating_sub(allowed);
                PdbSnapshot {
                    name: pdb.name_any(),
                    workload,
                    min_available,
                    current_healthy,
                }
            })
            .collect();

        Ok(Self {
            nodes,
            pods,
            pdbs,
            start: Instant::now(),
            last_cold_window_ms: 0,
            user_facing_errors: 0,
            pending: Vec::new(),
        })
    }
}

impl ClusterOps for SnapshotOps {
    fn list_nodes(&self) -> Vec<ClusterNode> {
        self.nodes.clone()
    }
    fn list_pods(&self) -> Vec<WorkloadPod> {
        self.pods.clone()
    }
    fn list_pdbs(&self) -> Vec<PdbSnapshot> {
        self.pdbs.clone()
    }
    fn slo(&self) -> SloSample {
        SloSample {
            error_rate: 0.0,
            availability: 1.0,
        }
    }
    fn now_unix(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn prewarm(&mut self, workload: &str, buffer: u32) -> Result<bool> {
        self.pending.push(PendingOp::Prewarm {
            workload: workload.to_string(),
            buffer,
        });
        Ok(true)
    }

    fn workload_ready_off_node(&self, workload: &str, node: &str) -> bool {
        self.pods
            .iter()
            .any(|p| p.workload == workload && p.node != node && p.ready)
    }

    fn cordon(&mut self, node: &str) -> Result<()> {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.name == node) {
            n.cordoned = true;
        }
        self.pending.push(PendingOp::Cordon {
            node: node.to_string(),
        });
        Ok(())
    }

    fn uncordon(&mut self, node: &str) -> Result<()> {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.name == node) {
            n.cordoned = false;
        }
        self.pending.push(PendingOp::Uncordon {
            node: node.to_string(),
        });
        Ok(())
    }

    fn evict(&mut self, pod: &WorkloadPod) -> Result<()> {
        self.pods.retain(|p| p.name != pod.name);
        self.pending.push(PendingOp::Evict {
            namespace: pod.namespace.clone(),
            name: pod.name.clone(),
        });
        Ok(())
    }

    fn recover_workload(&mut self, workload: &str) -> Result<()> {
        self.pending.push(PendingOp::Recover {
            workload: workload.to_string(),
        });
        Ok(())
    }

    fn record_event(&mut self, reason: &str, message: &str) {
        self.pending.push(PendingOp::Event {
            reason: reason.to_string(),
            message: message.to_string(),
        });
    }

    fn user_facing_errors(&self) -> u64 {
        self.user_facing_errors
    }
    fn last_cold_window_ms(&self) -> u64 {
        self.last_cold_window_ms
    }
}

async fn apply_ops(
    ctx: &MaintenancePlanState,
    plan: &MaintenancePlan,
    pending: &[PendingOp],
) -> Result<()> {
    let recorder = Recorder::new(
        ctx.client.clone(),
        ctx.reporter.clone(),
        plan.object_ref(&()),
    );
    for op in pending {
        match op {
            PendingOp::Cordon { node } => {
                if let Err(e) = cordon_node(&ctx.client, node).await {
                    warn!(node, error = %e, "cordon failed");
                }
            }
            PendingOp::Uncordon { node } => {
                if let Err(e) = uncordon_node(&ctx.client, node).await {
                    warn!(node, error = %e, "uncordon failed");
                }
            }
            PendingOp::Evict { namespace, name } => {
                if let Err(e) = evict_pod(&ctx.client, namespace, name).await {
                    warn!(pod = %name, error = %e, "evict failed (PDB may be blocking)");
                }
            }
            PendingOp::Prewarm { workload, buffer } => {
                if let Err(e) = scale_deployments(&ctx.client, workload, *buffer).await {
                    warn!(workload, error = %e, "prewarm scale failed");
                }
            }
            PendingOp::Recover { workload } => {
                if let Err(e) = scale_deployments(&ctx.client, workload, 1).await {
                    warn!(workload, error = %e, "recovery scale failed");
                }
            }
            PendingOp::Event { reason, message } => {
                let _ = recorder
                    .publish(kube::runtime::events::Event {
                        type_: EventType::Normal,
                        reason: reason.clone(),
                        note: Some(message.clone()),
                        action: "MaintenancePlan".into(),
                        secondary: None,
                    })
                    .await;
            }
        }
    }
    Ok(())
}

async fn cordon_node(client: &Client, name: &str) -> Result<()> {
    let nodes: Api<Node> = Api::all(client.clone());
    let patch = json!({
        "spec": { "unschedulable": true },
        "metadata": {
            "annotations": {
                "stellar.org/maintenance-plan": "true",
                "stellar.org/maintenance-plan-time": chrono::Utc::now().to_rfc3339(),
            }
        }
    });
    nodes
        .patch(
            name,
            &PatchParams::apply("stellar-maintenance-plan"),
            &Patch::Merge(&patch),
        )
        .await
        .map_err(Error::KubeError)?;
    Ok(())
}

async fn uncordon_node(client: &Client, name: &str) -> Result<()> {
    let nodes: Api<Node> = Api::all(client.clone());
    let patch = json!({ "spec": { "unschedulable": false } });
    nodes
        .patch(
            name,
            &PatchParams::apply("stellar-maintenance-plan"),
            &Patch::Merge(&patch),
        )
        .await
        .map_err(Error::KubeError)?;
    Ok(())
}

async fn evict_pod(client: &Client, ns: &str, name: &str) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    pods.evict(name, &EvictParams::default())
        .await
        .map_err(Error::KubeError)?;
    Ok(())
}

async fn scale_deployments(client: &Client, workload: &str, buffer: u32) -> Result<()> {
    let deploys: Api<Deployment> = Api::all(client.clone());
    let list = deploys
        .list(&ListParams::default())
        .await
        .map_err(Error::KubeError)?;
    for d in list {
        let labels: BTreeMap<String, String> = d.labels().clone();
        let hit = labels.get("app.kubernetes.io/instance").map(|s| s.as_str()) == Some(workload)
            || labels.get("app").map(|s| s.as_str()) == Some(workload)
            || d.name_any() == workload;
        if !hit {
            continue;
        }
        let annotations = d.annotations();
        if annotations.contains_key("stellar.org/maintenance-prewarm-base") {
            continue;
        }
        let current = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
        let desired = current + buffer as i32;
        let ns = d.namespace().unwrap_or_else(|| "default".into());
        let api: Api<Deployment> = Api::namespaced(client.clone(), &ns);
        let patch = json!({
            "metadata": {
                "annotations": {
                    "stellar.org/maintenance-prewarm-base": current.to_string()
                }
            },
            "spec": { "replicas": desired }
        });
        let _ = api
            .patch(
                &d.name_any(),
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await;
    }
    Ok(())
}
