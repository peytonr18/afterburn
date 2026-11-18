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
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use libazureinit_kvp::{
    read_vm_id, DiagnosticWriter, HealthWriteError, HealthWriter, HealthWriterBuildError, KvpPool,
    KvpPoolStore, Outcome, PoolMode, ProvisioningReport, ReportPpsType, ReportResult,
    WireserverConfig, WireserverError,
};
use slog_scope::{info, warn};

use super::boot_services;

const AGENT: &str = concat!("afterburn/", env!("CARGO_PKG_VERSION"));

// The library can create the pool file. Only its parent directory must already exist.
pub(super) fn wait_for_pool_directory(
    dir: &Path,
    budget: Duration,
    request_daemon: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let ready = || match dir.metadata() {
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => bail!("{} is not a directory", dir.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("checking {}", dir.display())),
    };
    if ready()? {
        return Ok(());
    }

    info!(
        "Waiting for the Hyper-V KVP daemon to prepare {}",
        dir.display()
    );
    let started = Instant::now();
    // The guest may already have scheduled kvpd (possibly under another unit name).
    if let Err(error) = request_daemon() {
        warn!(
            "Could not request hypervkvpd.service; still waiting for the pool directory: {error:#}"
        );
    }
    while !ready()? {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            bail!("timed out waiting for {}", dir.display());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(250)));
    }
    Ok(())
}

fn response_status(error: &WireserverError) -> Option<u16> {
    match error {
        WireserverError::HttpStatus { status, .. } => Some(status.as_u16()),
        WireserverError::AttemptsExhausted { last_error, .. }
        | WireserverError::Timeout {
            last_error: Some(last_error),
            ..
        } => response_status(last_error),
        WireserverError::Fallback { xml, .. } => response_status(xml),
        _ => None,
    }
}

#[derive(Debug)]
pub(super) struct Reporting {
    health: Result<HealthWriter, HealthWriterBuildError>,
    diagnostics: Option<DiagnosticWriter>,
    vm_id: String,
    failed: AtomicBool,
    boot_units: boot_services::Query,
}

impl Reporting {
    pub(super) fn for_endpoint(endpoint: String) -> Self {
        let dir = KvpPool::default_dir();
        let identity = read_vm_id();
        // The root-filesystem daemon may not be available until after switch-root.
        // Never hold up Ignition in the initramfs waiting for it.
        if identity.is_ok() && !Path::new("/etc/initrd-release").exists() {
            if let Err(error) = wait_for_pool_directory(
                dir,
                Duration::from_secs(30),
                boot_services::request_kvp_daemon,
            ) {
                warn!("Could not prepare Azure KVP directory {}; continuing with best-effort KVP and HTTP reporting: {error:#}", dir.display());
            }
        }
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
            dir,
            identity,
            boot_services::query,
        )
    }

    pub(super) fn new(
        config: WireserverConfig,
        pool_directory: &Path,
        identity: io::Result<String>,
        boot_units: boot_services::Query,
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
        let health = builder.build().inspect_err(|error| {
            warn!("Could not configure Azure health reporting: {error}");
        });
        Self {
            health,
            diagnostics,
            vm_id,
            failed: AtomicBool::new(false),
            boot_units,
        }
    }

    pub(super) fn note(&self, name: &str, message: impl Into<String>) {
        self.event(name, message, None, None);
    }

    fn event(
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

    fn observe<T>(&self, name: &str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
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
            warn!("Azure operation failed"; "operation" => name, "error" => format!("{error:#}"));
            // Metadata errors can contain credentials or untrusted payloads.
            self.publish_failure(
                name,
                &format!("Azure {name} failed; see the guest journal for details"),
            );
        }
        result
    }

    fn publish_failure(&self, operation: &str, reason: &str) {
        self.failed.store(true, Ordering::Relaxed);
        let report = ProvisioningReport::failure(AGENT, &self.vm_id, reason, ReportPpsType::None)
            .with_extra("operation", operation);
        if let Err(error) = self.publish(&report) {
            warn!("Could not deliver Azure provisioning failure: {error:#}");
        }
    }

    fn check_boot_services(&self) -> Result<()> {
        let started = Instant::now();
        let units = (self.boot_units)().map_err(|error| {
            let message = "Could not read systemd boot service status; withholding Azure readiness";
            self.event(
                "boot:services",
                message,
                Some(Outcome::Error),
                Some(started.elapsed()),
            );
            error.context(message)
        })?;
        if let Some(reason) = boot_services::failure_reason(units) {
            self.event(
                "boot:services",
                &reason,
                Some(Outcome::Error),
                Some(started.elapsed()),
            );
            self.publish_failure("boot:services", &reason);
            bail!("{reason}");
        }
        self.event(
            "boot:services",
            "No watched boot services are currently failed",
            Some(Outcome::Ok),
            Some(started.elapsed()),
        );
        Ok(())
    }

    pub(super) fn check_in(&self) -> Result<()> {
        self.event(
            "boot:check-in",
            "Afterburn is checking Azure boot readiness",
            None,
            None,
        );
        self.observe("boot:check-in", || {
            if self.failed.load(Ordering::Relaxed) {
                bail!("refusing Azure readiness after an unrecovered error in this provider");
            }
            self.check_boot_services()?;
            self.event("boot:ready", "Reporting provisioning complete", None, None);
            self.publish(&ProvisioningReport::success(
                AGENT,
                &self.vm_id,
                ReportPpsType::None,
            ))
        })
    }

    fn publish(&self, report: &ProvisioningReport) -> Result<()> {
        // The report key is replaced on each publication; diagnostic records are appended.
        let tag = format!(
            "result={} report_timestamp={}",
            report.result(),
            report.timestamp()
        );
        let state = match report.result() {
            ReportResult::Success => "Ready",
            ReportResult::Error | ReportResult::InProgress => "NotReady",
        };
        let ack = self
            .health
            .as_ref()
            .map_err(|error| anyhow!("Azure health reporting is unavailable: {error}"))
            .and_then(|health| {
                self.event(
                    "health:send",
                    format!("Initiating health report to wireserver: {state} (JSON POST); {tag}"),
                    None,
                    None,
                );
                match health.write_report(report) {
                    Ok(delivery) => delivery
                        .wireserver
                        .context("Azure check-in requires an HTTP acknowledgement"),
                    Err(HealthWriteError::Kvp {
                        error,
                        wireserver: Some(ack),
                    }) => {
                        warn!("Azure KVP report failed, but WireServer acknowledged it: {error}");
                        Ok(ack)
                    }
                    Err(error) => Err(error.into()),
                }
            })
            .inspect_err(|error| {
                // Include status when the transport actually returned one; not the response body.
                let status = error
                    .chain()
                    .find_map(|cause| response_status(cause.downcast_ref::<WireserverError>()?))
                    .map(|code| format!("; Wireserver replied with status {code}"))
                    .unwrap_or_default();
                self.event(
                    "health:unacknowledged",
                    format!(
                        "Failed to send provisioning report to wireserver: {state}; {tag}{status}"
                    ),
                    Some(Outcome::Error),
                    None,
                );
            })?;
        let message = format!(
            "WireServer acknowledged health report: Report '{state}' succeeded (HTTP 2xx); protocol={:?} attempts={} xml_attempts={} fallback_status={:?}; {tag}",
            ack.protocol, ack.json_attempts, ack.xml_attempts, ack.fallback_status,
        );
        info!("{message}");
        self.event("health:acknowledged", message, Some(Outcome::Ok), None);
        Ok(())
    }
}
