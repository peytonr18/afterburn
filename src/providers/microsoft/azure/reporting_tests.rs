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

use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use libazureinit_kvp::{
    Diagnostic, DiagnosticPayload, DiagnosticReader, Entry, KvpPool, KvpPoolStore, Outcome,
    PoolMode, ProvisioningReport, ReportResult, WireserverConfig, PROVISIONING_REPORT_KEY,
};
use mockito::{Mock, Server};
use serde::Serialize;
use serde_json::{json, Value};
use xml::reader::{EventReader, XmlEvent};

use super::boot_services::{self, UnitState};
use super::reporting::Reporting;
use super::Azure;
use crate::providers::MetadataProvider;
use crate::retry;

const VM_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const KEYS_PATH: &str = "/metadata/instance/compute/publicKeys?api-version=2021-02-01";
const HOSTNAME_PATH: &str = "/metadata/instance/compute/name?api-version=2017-08-01&format=text";
const BAD_KEYS: &str = r#"[{"keyData":"not-an-ssh-key","path":"/home/core/.ssh/authorized_keys"}]"#;
const GOALSTATE: &str = r#"<GoalState><Incarnation>42</Incarnation><Container>
<ContainerId>demo-container</ContainerId><RoleInstanceList>
<RoleInstance><InstanceId>demo-role</InstanceId></RoleInstance></RoleInstanceList>
</Container></GoalState>"#;

fn config(server: &Server) -> WireserverConfig {
    WireserverConfig {
        endpoint: format!("{}/provisioning/health", server.url()),
        connect_timeout: Duration::from_millis(100),
        request_timeout: Duration::from_millis(200),
        total_retry_timeout: Duration::from_secs(1),
        retry_interval: Duration::ZERO,
        max_attempts: Some(1),
        xml_fallback: true,
        use_system_proxy: false,
    }
}

fn unit(name: &str, state: &str) -> UnitState {
    UnitState {
        name: name.into(),
        active_state: state.into(),
    }
}

fn fixture_reporting(config: WireserverConfig, pool: &Path, id: io::Result<String>) -> Reporting {
    Reporting::new(config, pool, id, || {
        Ok(vec![
            unit("ignition-fetch.service", "active"),
            unit("ignition-fetch-offline.service", "inactive"),
            unit("some-other.service", "failed"),
            unit("afterburn-checkin.service", "failed"),
        ])
    })
}

fn reporting(server: &Server, pool: &Path) -> Reporting {
    fixture_reporting(config(server), pool, Ok(VM_ID.into()))
}

pub(super) fn with_client(client: retry::Client, server: &Server, pool: &Path) -> Result<Azure> {
    Azure::with_client(
        Some(client),
        Azure::get_fabric_address(),
        reporting(server, pool),
    )
}

fn provider(server: &Server, pool: &Path) -> Azure {
    with_client(
        retry::Client::try_new()
            .unwrap()
            .max_retries(0)
            .mock_base_url(server.url()),
        server,
        pool,
    )
    .unwrap()
}

fn versions(server: &mut Server) -> Mock {
    server
        .mock("GET", "/?comp=versions")
        .with_body("<Versions><Supported><Version>2012-11-30</Version></Supported></Versions>")
        .expect(1)
        .create()
}

fn store(pool: &Path) -> KvpPoolStore {
    KvpPoolStore::new_in(KvpPool::Guest, pool, PoolMode::Safe).unwrap()
}

pub(super) fn messages(pool: &Path, name: &str) -> Vec<String> {
    DiagnosticReader::new(store(pool))
        .entries()
        .unwrap()
        .into_iter()
        .filter_map(|entry| {
            let Entry::Diagnostic(Diagnostic::Event(event)) = entry else {
                return None;
            };
            match event.payload {
                DiagnosticPayload::Text(text) if event.key.name == name => Some(text),
                _ => None,
            }
        })
        .collect()
}

fn report(store: &KvpPoolStore) -> ProvisioningReport {
    store.entries().unwrap()[PROVISIONING_REPORT_KEY]
        .parse()
        .unwrap()
}

fn has_outcome(store: &KvpPoolStore, name: &str, outcome: Outcome) -> bool {
    DiagnosticReader::new(store.clone())
        .entries()
        .unwrap()
        .iter()
        .any(|entry| {
            matches!(entry, Entry::Diagnostic(Diagnostic::Event(event))
                if event.key.name == name && event.result == Some(outcome)
                    && event.duration.is_some())
        })
}

// Read the pool back, rather than assuming a successful KVP write means HTTP succeeded.
fn assert_delivery_trace(store: &KvpPoolStore, acknowledged: bool) -> String {
    let report = report(store);
    let entries = DiagnosticReader::new(store.clone()).entries().unwrap();
    let announced_ready = entries.iter().any(|entry| matches!(entry,
        Entry::Diagnostic(Diagnostic::Event(event)) if event.key.name == "boot:ready"
            && matches!(&event.payload, DiagnosticPayload::Text(text) if text == "Reporting provisioning complete")
    ));
    assert_eq!(announced_ready, report.result() == ReportResult::Success);
    let mut traces = entries.iter().filter_map(|entry| match entry {
        Entry::Diagnostic(Diagnostic::Event(event)) if event.key.name.starts_with("health:") => {
            Some(event)
        }
        _ => None,
    });
    let tag = format!(
        "result={} report_timestamp={}",
        report.result(),
        report.timestamp()
    );
    let state = if report.result() == ReportResult::Success {
        "Ready"
    } else {
        "NotReady"
    };
    let send = traces.next().expect("missing initiation diagnostic");
    assert_eq!(send.key.name, "health:send");
    assert_eq!(send.result, None); // Intention to send is not a receipt.
    assert_eq!(
        send.payload,
        DiagnosticPayload::Text(format!(
            "Initiating health report to wireserver: {state} (JSON POST); {tag}"
        ))
    );
    let trace = traces.next().expect("missing delivery diagnostic");
    let (name, outcome) = if acknowledged {
        ("health:acknowledged", Outcome::Ok)
    } else {
        ("health:unacknowledged", Outcome::Error)
    };
    assert_eq!(trace.key.name, name);
    assert_eq!(trace.result, Some(outcome));
    let DiagnosticPayload::Text(text) = &trace.payload else {
        panic!("expected a text diagnostic")
    };
    assert!(text.contains(&tag));
    if acknowledged {
        assert!(text.starts_with("WireServer acknowledged health report"));
        assert!(text.contains(&format!("Report '{state}' succeeded (HTTP 2xx)")));
    } else {
        assert!(text.starts_with("Failed to send provisioning report to wireserver"));
    }
    assert!(
        traces.next().is_none(),
        "contradictory or duplicate delivery trace"
    );
    text.clone()
}

#[derive(Clone, Debug, Serialize)]
struct Request {
    path: String,
    body: String,
}

type Requests = Arc<Mutex<Vec<Request>>>;

fn health(server: &mut Server, path: &str, status: usize) -> (Mock, Requests) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let mock = server
        .mock("POST", path)
        .with_status(status)
        .with_body_from_request(move |request| {
            captured.lock().unwrap().push(Request {
                path: request.path_and_query().to_owned(),
                body: String::from_utf8(request.body().unwrap().to_vec()).unwrap(),
            });
            Vec::new()
        })
        .expect(1)
        .create();
    (mock, requests)
}

fn json_body(requests: &Requests) -> Value {
    serde_json::from_str(&requests.lock().unwrap()[0].body).unwrap()
}

fn xml_text(body: &str, field: &str) -> String {
    let mut selected = false;
    let mut text = String::new();
    for event in EventReader::from_str(body) {
        match event.unwrap() {
            XmlEvent::StartElement { name, .. } => selected = name.local_name == field,
            XmlEvent::Characters(value) | XmlEvent::CData(value) if selected => {
                text.push_str(&value);
            }
            XmlEvent::EndElement { .. } => selected = false,
            _ => {}
        }
    }
    text
}

fn evidence(name: &str, store: &KvpPoolStore, requests: &[Request], error: Option<&anyhow::Error>) {
    let entries = DiagnosticReader::new(store.clone()).entries().unwrap();
    let outcome = json!({
        "operation_result": if error.is_some() { "error" } else { "success" },
        "original_error": error.map(|error| format!("{error:#}")),
    });
    println!("\n=== Azure demo: {name} ===\n{outcome}");
    for (key, value) in store.dump().unwrap() {
        println!("{key}: {value}");
    }
    for request in requests {
        println!("POST {} {}", request.path, request.body);
    }
    if let Some(directory) = std::env::var_os("AFTERBURN_AZURE_DEMO_DIR") {
        let directory = Path::new(&directory).join(name);
        fs::create_dir(&directory).expect("use a fresh, existing AFTERBURN_AZURE_DEMO_DIR");
        fs::copy(store.path(), directory.join(".kvp_pool_1")).unwrap();
        fs::write(
            directory.join("kvp.json"),
            serde_json::to_string_pretty(&entries).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.join("http.json"),
            serde_json::to_string_pretty(requests).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.join("outcome.json"),
            serde_json::to_string_pretty(&outcome).unwrap(),
        )
        .unwrap();
        println!("Evidence saved to {}", directory.display());
    }
}

#[test]
fn azure_demo_success() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let store = store(pool.path());
    let version = versions(&mut server);
    let before_response = Arc::new(Mutex::new(Vec::new()));
    let snapshot = Arc::clone(&before_response);
    let directory = pool.path().to_owned();
    let hostname = server
        .mock("GET", HOSTNAME_PATH)
        .match_header("Metadata", "true")
        .with_body_from_request(move |_| {
            *snapshot.lock().unwrap() = messages(&directory, "imds:hostname");
            b"demo-node".to_vec()
        })
        .create();
    let keys = server
        .mock("GET", KEYS_PATH)
        .match_header("Metadata", "true")
        .with_body("[]")
        .create();
    let goalstate = server
        .mock("GET", "/machine/?comp=goalstate")
        .expect(0)
        .create();
    let (health, requests) = health(&mut server, "/provisioning/health", 200);

    let metadata = provider(&server, pool.path());
    assert_eq!(metadata.hostname().unwrap().as_deref(), Some("demo-node"));
    let get = format!("Fetching hostname from IMDS: GET http://169.254.169.254{HOSTNAME_PATH}");
    assert_eq!(before_response.lock().unwrap().as_slice(), [get.as_str()]);
    assert_eq!(
        messages(pool.path(), "imds:hostname"),
        [get.as_str(), "IMDS returned hostname: demo-node"]
    );
    assert!(metadata.ssh_keys().unwrap().is_empty());
    assert_eq!(
        messages(pool.path(), "imds:public-keys").last().unwrap(),
        "Validated 0 SSH public key(s)"
    );
    for name in ["provider:init", "hostname", "ssh-keys"] {
        assert!(has_outcome(&store, name, Outcome::Ok), "{name}");
    }
    assert!(!store
        .entries()
        .unwrap()
        .contains_key(PROVISIONING_REPORT_KEY));
    assert!(requests.lock().unwrap().is_empty());

    // The later check-in uses the same guest pool but does not need to fetch metadata again.
    reporting(&server, pool.path()).check_in().unwrap();

    assert_eq!(json_body(&requests), json!({"state": "Ready"}));
    let report = report(&store);
    assert_eq!(report.result(), ReportResult::Success);
    assert_eq!(report.vm_id(), VM_ID);
    assert_eq!(
        report.agent(),
        concat!("afterburn/", env!("CARGO_PKG_VERSION"))
    );
    assert!(has_outcome(&store, "boot:services", Outcome::Ok));
    assert!(assert_delivery_trace(&store, true).contains("protocol=Json attempts=1 xml_attempts=0"));
    version.assert();
    hostname.assert();
    keys.assert();
    goalstate.assert();
    health.assert();
    evidence("success", &store, &requests.lock().unwrap(), None);
}

#[test]
fn azure_hostname_diagnostics_do_not_export_unexpected_bodies() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let _version = versions(&mut server);
    let body = "demo-node\nunexpected second line";
    let _hostname = server.mock("GET", HOSTNAME_PATH).with_body(body).create();
    let metadata = provider(&server, pool.path());
    assert_eq!(metadata.hostname().unwrap().as_deref(), Some(body));
    assert_eq!(
        messages(pool.path(), "imds:hostname").last().unwrap(),
        "IMDS returned hostname: <omitted: unexpected label>"
    );
    assert!(store(pool.path())
        .dump()
        .unwrap()
        .iter()
        .all(|(_, v)| !v.contains("unexpected second line")));
}

#[test]
fn azure_hostname_request_failure_records_url_but_no_returned_hostname() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let _version = versions(&mut server);
    let _hostname = server.mock("GET", HOSTNAME_PATH).with_status(403).create();
    let (health, requests) = health(&mut server, "/provisioning/health", 200);
    let err = provider(&server, pool.path()).hostname().unwrap_err();
    assert!(format!("{err:#}").contains("failed to get hostname"));
    assert_eq!(
        messages(pool.path(), "imds:hostname"),
        [
            format!("Fetching hostname from IMDS: GET http://169.254.169.254{HOSTNAME_PATH}"),
            "IMDS GET failed".to_owned()
        ]
    );
    assert_eq!(json_body(&requests)["state"], "NotReady");
    health.assert();
}

fn check_failed_service(query: boot_services::Query, expected: &str, scenario: &str) {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let store = store(pool.path());
    let (health, requests) = health(&mut server, "/provisioning/health", 200);
    let reporting = Reporting::new(config(&server), pool.path(), Ok(VM_ID.into()), query);

    let error = reporting.check_in().unwrap_err();
    assert!(error.to_string().contains(expected), "{error:#}");
    let report = report(&store);
    assert_eq!(report.result(), ReportResult::Error);
    assert_eq!(report.reason(), Some(error.to_string().as_str()));
    assert_eq!(
        report.extra(),
        &[("operation".into(), "boot:services".into())]
    );
    assert!(has_outcome(&store, "boot:services", Outcome::Error));
    assert!(assert_delivery_trace(&store, true).contains("protocol=Json"));
    assert_eq!(
        json_body(&requests),
        json!({"state":"NotReady","details":{
            "subStatus":"ProvisioningFailed",
            "description":store.entries().unwrap()[PROVISIONING_REPORT_KEY]
        }})
    );
    health.assert(); // Exactly one POST: never Ready after reporting failure.
    evidence(scenario, &store, &requests.lock().unwrap(), Some(&error));
}

#[test]
fn azure_demo_failed_ignition_service() {
    check_failed_service(
        || Ok(vec![unit("ignition-fetch.service", "failed")]),
        "ignition-fetch.service",
        "ignition-failure",
    );
}

#[test]
#[ignore = "requires systemd with a failed ignition-demo-validate.service; see the demo runbook"]
fn azure_live_failed_validator() {
    check_failed_service(
        boot_services::query,
        "ignition-demo-validate.service",
        "live-ignition-failure",
    );
}

#[test]
fn azure_unreadable_service_status_withholds_ready_without_inventing_a_failure() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let store = store(pool.path());
    let health = server
        .mock("POST", "/provisioning/health")
        .expect(0)
        .create();
    let reporting = Reporting::new(config(&server), pool.path(), Ok(VM_ID.into()), || {
        anyhow::bail!("system bus unavailable")
    });
    let error = reporting.check_in().unwrap_err();
    assert!(format!("{error:#}").contains("system bus unavailable"));
    assert!(has_outcome(&store, "boot:services", Outcome::Error));
    assert!(!store
        .entries()
        .unwrap()
        .contains_key(PROVISIONING_REPORT_KEY));
    health.assert();
}

#[test]
fn azure_failed_units_keep_their_reason_when_http_fails() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let store = store(pool.path());
    let (health, requests) = health(&mut server, "/provisioning/health", 403);
    let reporting = Reporting::new(config(&server), pool.path(), Ok(VM_ID.into()), || {
        Ok(vec![
            unit("NetworkManager-wait-online.service", "failed"),
            unit("afterburn-sshkeys@private-user.service", "failed"),
            unit("systemd-networkd.service", "failed"),
            unit("ignition-files.service", "failed"),
            unit("afterburn-firstboot-checkin.service", "failed"),
        ])
    });
    let error = reporting.check_in().unwrap_err().to_string();
    assert_eq!(
        error,
        "systemd reports 4 failed boot service(s): ignition-files.service, NetworkManager-wait-online.service, afterburn-sshkeys@.service, systemd-networkd.service"
    );
    assert_eq!(report(&store).reason(), Some(error.as_str()));
    assert!(assert_delivery_trace(&store, false).contains("Wireserver replied with status 403"));
    assert_eq!(
        json_body(&requests)["details"]["description"],
        report(&store).encode()
    );
    health.assert();
}

#[test]
fn azure_many_failed_units_still_fit_one_kvp_report() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let (health, requests) = health(&mut server, "/provisioning/health", 200);
    let reporting = Reporting::new(config(&server), pool.path(), Ok(VM_ID.into()), || {
        Ok((0..40)
            .map(|i| {
                unit(
                    &format!("ignition-{}-{i}.service", "x".repeat(220)),
                    "failed",
                )
            })
            .collect())
    });
    let error = reporting.check_in().unwrap_err();
    let report = report(&store(pool.path()));
    assert_eq!(report.reason(), Some(error.to_string().as_str()));
    assert!(error.to_string().contains("40 failed"));
    assert!(error.to_string().contains("more unit names"));
    assert_eq!(
        json_body(&requests)["details"]["description"],
        report.encode()
    );
    health.assert();
}

#[test]
fn azure_metadata_failure() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let store = store(pool.path());
    let version = versions(&mut server);
    let keys = server.mock("GET", KEYS_PATH).with_body(BAD_KEYS).create();
    let hostname = server
        .mock("GET", HOSTNAME_PATH)
        .with_body("still-available")
        .create();
    let (health, requests) = health(&mut server, "/provisioning/health", 200);
    let provider = provider(&server, pool.path());

    store.insert("unrelated", "preserved").unwrap();
    assert!(!store
        .entries()
        .unwrap()
        .contains_key(PROVISIONING_REPORT_KEY));
    let error = provider.ssh_keys().unwrap_err();
    assert!(format!("{error:#}").contains("failed to parse IMDS public key at index 0"));
    assert!(format!("{error:#}").contains("/home/core/.ssh/authorized_keys"));
    let encoded = store.entries().unwrap()[PROVISIONING_REPORT_KEY].clone();
    let report = report(&store);
    assert_eq!(report.result(), ReportResult::Error);
    assert_eq!(report.extra(), &[("operation".into(), "ssh-keys".into())]);
    assert_eq!(
        json_body(&requests),
        json!({"state":"NotReady","details":{
            "subStatus":"ProvisioningFailed","description":encoded
        }})
    );
    assert!(has_outcome(&store, "ssh-keys", Outcome::Error));
    let steps = messages(pool.path(), "imds:public-keys");
    assert_eq!(
        steps.last().unwrap(),
        "SSH-key metadata did not pass validation"
    );
    assert!(store
        .dump()
        .unwrap()
        .iter()
        .all(|(_, v)| !v.contains("not-an-ssh-key") && !v.contains("/home/core/.ssh/")));
    assert_delivery_trace(&store, true);
    provider.hostname().unwrap();
    assert!(provider
        .clone()
        .boot_checkin()
        .unwrap_err()
        .to_string()
        .contains("refusing"));
    assert_eq!(store.entries().unwrap()[PROVISIONING_REPORT_KEY], encoded);
    assert_eq!(store.entries().unwrap()["unrelated"], "preserved");

    version.assert();
    keys.assert();
    hostname.assert();
    health.assert();
}

#[test]
fn azure_xml_fallback_preserves_report() {
    for status in [404, 405, 501] {
        for failed in [false, true] {
            let mut server = Server::new();
            let pool = tempfile::tempdir().unwrap();
            let store = store(pool.path());
            let version = versions(&mut server);
            let (json_health, _) = health(&mut server, "/provisioning/health", status);
            let goalstate = server
                .mock("GET", "/machine/?comp=goalstate")
                .match_header("x-ms-version", "2012-11-30")
                .with_body(GOALSTATE)
                .expect(1)
                .create();
            let (xml_health, xml_requests) = health(&mut server, "/machine/?comp=health", 200);
            let provider = provider(&server, pool.path());
            if failed {
                let _keys = server.mock("GET", KEYS_PATH).with_body(BAD_KEYS).create();
                provider.ssh_keys().unwrap_err();
            } else {
                provider.boot_checkin().unwrap();
            }
            let requests = xml_requests.lock().unwrap();
            let xml = &requests[0].body;
            assert_eq!(
                xml_text(xml, "State"),
                if failed { "NotReady" } else { "Ready" }
            );
            assert_eq!(xml_text(xml, "GoalStateIncarnation"), "42");
            if failed {
                assert_eq!(xml_text(xml, "SubStatus"), "ProvisioningFailed");
                assert_eq!(
                    xml_text(xml, "Description"),
                    store.entries().unwrap()[PROVISIONING_REPORT_KEY]
                );
            }
            let trace = assert_delivery_trace(&store, true);
            assert!(trace.contains("protocol=Xml") && trace.contains(&status.to_string()));
            version.assert();
            json_health.assert();
            goalstate.assert();
            xml_health.assert();
        }
    }
}

#[test]
fn azure_provider_construction_failure_is_reported() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let version = server
        .mock("GET", "/?comp=versions")
        .with_status(403)
        .create();
    let (health, requests) = health(&mut server, "/provisioning/health", 200);
    let error = with_client(
        retry::Client::try_new()
            .unwrap()
            .max_retries(0)
            .mock_base_url(server.url()),
        &server,
        pool.path(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("failed version compatibility check"));
    assert_eq!(report(&store(pool.path())).result(), ReportResult::Error);
    assert_eq!(json_body(&requests)["state"], "NotReady");
    version.assert();
    health.assert();
}

#[test]
fn azure_other_http_errors_never_select_xml_or_repeat_kvp() {
    for status in [401, 403, 500, 503] {
        let mut server = Server::new();
        let pool = tempfile::tempdir().unwrap();
        let attempts = if status >= 500 { 2 } else { 1 };
        let json = server
            .mock("POST", "/provisioning/health")
            .with_status(status)
            .expect(attempts)
            .create();
        let goalstate = server
            .mock("GET", "/machine/?comp=goalstate")
            .expect(0)
            .create();
        let xml = server
            .mock("POST", "/machine/?comp=health")
            .expect(0)
            .create();
        let reporting = fixture_reporting(
            WireserverConfig {
                max_attempts: Some(2),
                ..config(&server)
            },
            pool.path(),
            Ok(VM_ID.into()),
        );
        reporting.check_in().unwrap_err();
        let store = store(pool.path());
        assert_eq!(report(&store).result(), ReportResult::Success);
        let trace = assert_delivery_trace(&store, false);
        assert!(
            trace.contains(&format!("Wireserver replied with status {status}")),
            "{trace}"
        );
        assert_eq!(
            store
                .dump()
                .unwrap()
                .iter()
                .filter(|(key, _)| key == PROVISIONING_REPORT_KEY)
                .count(),
            1
        );
        json.assert();
        goalstate.assert();
        xml.assert();
    }
}

#[test]
fn azure_kvp_failure_does_not_prevent_http_checkin() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let (health, requests) = health(&mut server, "/provisioning/health", 200);
    reporting(&server, &pool.path().join("missing"))
        .check_in()
        .unwrap();
    assert_eq!(json_body(&requests)["state"], "Ready");
    health.assert();
}

#[test]
fn azure_delivery_failures_do_not_replace_or_leak_the_operation_error() {
    for missing_pool in [false, true] {
        let mut server = Server::new();
        let pool = tempfile::tempdir().unwrap();
        let directory = if missing_pool {
            pool.path().join("missing")
        } else {
            pool.path().to_owned()
        };
        let (health, requests) = health(&mut server, "/provisioning/health", 403);
        let reporting = reporting(&server, &directory);
        let error = reporting
            .operation("test:operation", || -> Result<()> {
                Err(io::Error::other("sensitive-example-value").into())
            })
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().to_string(),
            "sensitive-example-value"
        );
        assert!(!requests.lock().unwrap()[0]
            .body
            .contains("sensitive-example-value"));
        if !missing_pool {
            let report = report(&store(&directory));
            assert_eq!(report.result(), ReportResult::Error);
            assert!(!report.encode().contains("sensitive-example-value"));
        }
        health.assert();
    }
}

#[test]
fn azure_missing_identity_keeps_http_failure_reporting() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let (health, requests) = health(&mut server, "/provisioning/health", 200);
    let reporting = fixture_reporting(
        config(&server),
        pool.path(),
        Err(io::Error::new(io::ErrorKind::NotFound, "no DMI UUID")),
    );
    reporting
        .operation("test:identity", || -> Result<()> {
            anyhow::bail!("original")
        })
        .unwrap_err();
    let body = json_body(&requests);
    let report: ProvisioningReport = body["details"]["description"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(report.vm_id(), "unavailable");
    assert!(!store(pool.path()).path().exists());
    health.assert();
}

#[test]
fn azure_reporting_setup_error_preserves_failure_but_rejects_checkin() {
    let server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let reporting = fixture_reporting(
        WireserverConfig {
            endpoint: "not a URL".into(),
            ..config(&server)
        },
        pool.path(),
        Ok(VM_ID.into()),
    );
    let error = reporting
        .operation("test:setup", || -> Result<()> { anyhow::bail!("original") })
        .unwrap_err();
    assert_eq!(error.to_string(), "original");
    let another = fixture_reporting(
        WireserverConfig {
            endpoint: "not a URL".into(),
            ..config(&server)
        },
        pool.path(),
        Ok(VM_ID.into()),
    );
    assert!(another
        .check_in()
        .unwrap_err()
        .to_string()
        .contains("unavailable"));
    let entries = DiagnosticReader::new(store(pool.path())).entries().unwrap();
    let delivery_events: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Diagnostic(diagnostic) if diagnostic.key().name.starts_with("health:") => {
                Some(diagnostic.key().name.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(delivery_events, ["health:unacknowledged"; 2]); // Neither report could be sent.
}

#[test]
fn azure_timeout_is_bounded_and_does_not_select_xml() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let _json = server
        .mock("POST", "/provisioning/health")
        .with_body_from_request(|_| {
            std::thread::sleep(Duration::from_millis(300));
            Vec::new()
        })
        .create();
    let goalstate = server
        .mock("GET", "/machine/?comp=goalstate")
        .expect(0)
        .create();
    let reporting = fixture_reporting(
        WireserverConfig {
            request_timeout: Duration::from_millis(50),
            total_retry_timeout: Duration::from_millis(150),
            max_attempts: Some(2),
            ..config(&server)
        },
        pool.path(),
        Ok(VM_ID.into()),
    );
    let started = Instant::now();
    reporting.check_in().unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    goalstate.assert();
}

#[test]
fn azure_empty_goalstate_is_a_reportable_error_not_a_panic() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let _version = versions(&mut server);
    let _goalstate = server.mock("GET", "/machine/?comp=goalstate")
        .with_body("<GoalState><Incarnation>1</Incarnation><Container><ContainerId>demo</ContainerId><RoleInstanceList/></Container></GoalState>")
        .create();
    let (health, _) = health(&mut server, "/provisioning/health", 200);
    let error = provider(&server, pool.path()).attributes().unwrap_err();
    assert!(error.to_string().contains("no role instances"));
    assert_eq!(report(&store(pool.path())).result(), ReportResult::Error);
    health.assert();
}
