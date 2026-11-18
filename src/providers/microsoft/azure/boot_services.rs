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

//! Read-only snapshot of boot service failures still visible to systemd at check-in.

use anyhow::{Context, Result};
use std::time::Duration;
use zbus::{blocking::connection::Builder, proxy, zvariant::OwnedObjectPath};

// ListUnitsFiltered returns a(ssssssouso). Ignore descriptions and command output.
type ListedUnit = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

#[proxy(
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1",
    interface = "org.freedesktop.systemd1.Manager"
)]
trait SystemdManager {
    fn list_units_filtered(&self, states: &[&str]) -> zbus::Result<Vec<ListedUnit>>;
}

pub(super) struct UnitState {
    pub name: String,
    pub active_state: String,
}

pub(super) type Query = fn() -> Result<Vec<UnitState>>;

pub(super) fn query() -> Result<Vec<UnitState>> {
    let bus = Builder::system()?
        .method_timeout(Duration::from_secs(5))
        .build()
        .context("connecting to the system bus")?;
    let manager = SystemdManagerProxyBlocking::new(&bus).context("connecting to systemd")?;
    let rows = manager
        .list_units_filtered(&["failed"])
        .context("listing failed systemd units")?;
    Ok(rows
        .into_iter()
        .map(|(name, _, _, active_state, ..)| UnitState { name, active_state })
        .collect())
}

// Service stems (before any @instance); never let a previous check-in failure
// become its own reason for refusing readiness on the next check-in.
fn watched(stem: &str) -> bool {
    if matches!(stem, "afterburn-checkin" | "afterburn-firstboot-checkin") {
        return false;
    }
    stem.starts_with("ignition-")
        || stem.starts_with("afterburn-")
        || stem.starts_with("flatcar-afterburn-")
        || stem.starts_with("coreos-metadata-")
        || stem.starts_with("systemd-networkd-")
        || stem.starts_with("NetworkManager-")
        || matches!(
            stem,
            "ignition"
                | "initrd-setup-root-after-ignition"
                | "afterburn"
                | "coreos-metadata"
                | "flatcar-metadata-hostname"
                | "systemd-networkd"
                | "systemd-network-generator"
                | "systemd-resolved"
                | "NetworkManager"
                | "nm-initrd"
                | "nm-wait-online-initrd"
                | "network"
                | "networking"
                | "network-cleanup"
        )
}

pub(super) fn failure_reason(units: Vec<UnitState>) -> Option<String> {
    let mut names = Vec::new();
    for unit in units {
        let Some(stem) = unit.name.strip_suffix(".service") else {
            continue;
        };
        let (stem, instance) = stem.split_once('@').unwrap_or((stem, ""));
        if unit.active_state == "failed" && watched(stem) {
            // An instance can contain a username or other guest-supplied data.
            names.push(if instance.is_empty() {
                unit.name
            } else {
                format!("{stem}@.service")
            });
        }
    }
    if names.is_empty() {
        return None;
    }
    let count = names.len();
    names.sort_unstable_by(|a, b| {
        b.starts_with("ignition")
            .cmp(&a.starts_with("ignition"))
            .then_with(|| a.cmp(b))
    });
    names.dedup();
    let mut reason = format!("systemd reports {count} failed boot service(s): ");
    for (index, name) in names.iter().enumerate() {
        // Leave room for identity, timestamp, and encoding in the 1022-byte
        // safe KVP value, even if a large number of units failed.
        if reason.len() + name.len() + 2 > 512 {
            reason.push_str(&format!(" (+{} more unit names)", names.len() - index));
            break;
        }
        if index != 0 {
            reason.push_str(", ");
        }
        reason.push_str(name);
    }
    Some(reason)
}
