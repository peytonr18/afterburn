// Copyright 2026 Red Hat, Inc.
//
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

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use libazureinit_kvp::{
    read_vm_id, DiagnosticWriter, HealthDelivery, HealthWriteError, HealthWriter,
    HealthWriterBuildError, KvpPool, KvpPoolStore, Outcome, PoolMode, ProvisioningReport,
    ReportPpsType, WireserverConfig, WireserverDelivery,
};
use slog_scope::{info, warn};

const AGENT: &str = concat!("afterburn/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug)]
pub(super) struct Reporting {
    health: Result<HealthWriter, Arc<HealthWriterBuildError>>,
    diagnostics: Option<DiagnosticWriter>,
    vm_id: String,
    failed: Arc<AtomicBool>,
}

impl Reporting {
    pub(super) fn for_endpoint(endpoint: String) -> Self {
        Self::new(
            WireserverConfig {
                endpoint,
                connect_timeout: Duration::from_secs(1),
                request_timeout: Duration::from_secs(2),
                total_retry_timeout: Duration::from_secs(15),
                retry_interval: Duration::from_millis(100),
                max_attempts: Some(3),
                xml_fallback: true,
                use_system_proxy: false,
            },
            Path::new("/var/lib/hyperv"),
            read_vm_id(),
        )
    }

    pub(super) fn new(
        config: WireserverConfig,
        pool_directory: &Path,
        identity: io::Result<String>,
    ) -> Self {
        let (vm_id, store) = match identity {
            Ok(vm_id) => {
                let store =
                    match KvpPoolStore::new_in(KvpPool::Guest, pool_directory, PoolMode::Safe) {
                        Ok(store) => Some(store),
                        Err(error) => {
                            warn!("Could not configure Azure KVP pool: {error}");
                            None
                        }
                    };
                (vm_id, store)
            }
            Err(error) => {
                warn!("Could not read Azure VM UUID; using HTTP-only reporting: {error}");
                ("unavailable".into(), None)
            }
        };
        let diagnostics = store.as_ref().and_then(|store| {
            match DiagnosticWriter::new(store.clone(), AGENT, &vm_id) {
                Ok(writer) => Some(writer),
                Err(error) => {
                    warn!("Could not configure Azure KVP diagnostics: {error}");
                    None
                }
            }
        });
        let mut builder = HealthWriter::builder(AGENT, &vm_id).wireserver(config);
        if let Some(store) = store {
            builder = builder.kvp(store);
        }
        let health = builder.build().map_err(|error| {
            warn!("Could not configure Azure health reporting: {error}");
            Arc::new(error)
        });
        Self {
            health,
            diagnostics,
            vm_id,
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn event(
        &self,
        name: &str,
        message: impl Into<String>,
        outcome: Option<Outcome>,
        duration: Option<Duration>,
    ) {
        if let Some(writer) = &self.diagnostics {
            if let Err(error) = writer.emit_event(name, message.into(), None, outcome, duration) {
                warn!("Could not write Azure diagnostic"; "operation" => name, "error" => %error);
            }
        }
    }

    pub(super) fn observe<T>(
        &self,
        name: &str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.event(name, "starting", None, None);
        let started = Instant::now();
        let result = operation();
        let (message, outcome) = if result.is_ok() {
            ("completed", Outcome::Ok)
        } else {
            ("failed; see the guest journal for details", Outcome::Error)
        };
        self.event(name, message, Some(outcome), Some(started.elapsed()));
        result
    }

    pub(super) fn operation<T>(
        &self,
        name: &str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let result = self.observe(name, operation);
        if let Err(error) = &result {
            self.failed.store(true, Ordering::Relaxed);
            warn!("Azure operation failed"; "operation" => name, "error" => format!("{error:#}"));
            // Metadata errors can contain credentials or untrusted payloads.
            let report = ProvisioningReport::failure(
                AGENT,
                &self.vm_id,
                format!("Azure {name} failed; see the guest journal for details"),
                ReportPpsType::None,
            )
            .with_extra("operation", name);
            match &self.health {
                Ok(health) => {
                    if let Err(error) = self.acknowledge(health.write_report(&report)) {
                        warn!("Could not publish Azure provisioning failure: {error:#}");
                    }
                }
                Err(error) => warn!("Azure failure reporting is unavailable: {error}"),
            }
        }
        result
    }

    pub(super) fn check_in(&self) -> Result<()> {
        self.observe("boot:check-in", || {
            if self.failed.load(Ordering::Relaxed) {
                bail!("refusing Azure readiness after an unrecovered error in this provider");
            }
            let health = self
                .health
                .as_ref()
                .map_err(|error| anyhow!("Azure health reporting is unavailable: {error}"))?;
            self.acknowledge(health.report_success(ReportPpsType::None))
        })
    }

    fn acknowledge(&self, result: Result<HealthDelivery, HealthWriteError>) -> Result<()> {
        let delivery = match result {
            Ok(HealthDelivery {
                wireserver: Some(delivery),
                ..
            }) => delivery,
            Ok(_) => bail!("Azure check-in requires an HTTP acknowledgement"),
            Err(HealthWriteError::Kvp {
                error,
                wireserver: Some(delivery),
            }) => {
                warn!("Azure KVP report failed, but HTTP acknowledged the report: {error}");
                delivery
            }
            Err(error) => {
                warn!("Azure health delivery failed: {error:?}");
                return Err(error.into());
            }
        };
        self.delivered(delivery);
        Ok(())
    }

    fn delivered(&self, delivery: WireserverDelivery) {
        let message = format!(
            "protocol={:?}, JSON attempts={}, XML attempts={}, fallback status={:?}",
            delivery.protocol,
            delivery.json_attempts,
            delivery.xml_attempts,
            delivery.fallback_status,
        );
        info!("Azure health acknowledged: {message}");
        self.event("health:acknowledged", message, Some(Outcome::Ok), None);
    }
}
