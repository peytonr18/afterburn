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
    Diagnostic, DiagnosticReader, Entry, KvpPool, KvpPoolStore, Outcome, PoolMode,
    ProvisioningReport, ReportResult, WireserverConfig, PROVISIONING_REPORT_KEY,
};
use mockito::{Mock, Server};
use serde::Serialize;
use serde_json::{json, Value};
use xml::reader::{EventReader, XmlEvent};

use super::reporting::Reporting;
use super::Azure;
use crate::providers::MetadataProvider;
use crate::retry;

const VM_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const KEYS_PATH: &str = "/metadata/instance/compute/publicKeys?api-version=2021-02-01";
const HOSTNAME_PATH: &str = "/metadata/instance/compute/name?api-version=2017-08-01&format=text";
const GOOD_KEYS: &str = r#"[{"keyData":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIE5dnA1jzwJZ2ndPmoZAvB1MDLU+UJpw0o9EzGm6tF5Y demo"}]"#;
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

fn reporting(server: &Server, pool: &Path) -> Reporting {
    Reporting::new(config(server), pool, Ok(VM_ID.into()))
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
                path: request.path().to_owned(),
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
    for entry in &entries {
        match entry {
            Entry::Diagnostic(Diagnostic::Event(event)) => println!(
                "DIAG {} outcome={:?} duration={:?} {:?}",
                event.key.name, event.result, event.duration, event.payload
            ),
            Entry::Report(report) => {
                println!(
                    "PROVISIONING_REPORT {}",
                    serde_json::to_string(report).unwrap()
                );
            }
            _ => {}
        }
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
    let hostname = server
        .mock("GET", HOSTNAME_PATH)
        .match_header("Metadata", "true")
        .with_body("demo-host")
        .create();
    let keys = server
        .mock("GET", KEYS_PATH)
        .match_header("Metadata", "true")
        .with_body(GOOD_KEYS)
        .create();
    let (health, requests) = health(&mut server, "/provisioning/health", 200);
    let provider = provider(&server, pool.path());
    store.insert("unrelated", "preserved").unwrap();

    assert_eq!(provider.hostname().unwrap().as_deref(), Some("demo-host"));
    assert_eq!(provider.ssh_keys().unwrap().len(), 1);
    assert!(!store
        .entries()
        .unwrap()
        .contains_key(PROVISIONING_REPORT_KEY));
    provider.boot_checkin().unwrap();

    assert_eq!(json_body(&requests), json!({"state": "Ready"}));
    let report = report(&store);
    assert_eq!(report.result(), ReportResult::Success);
    assert_eq!(report.vm_id(), VM_ID);
    assert_eq!(
        report.agent(),
        concat!("afterburn/", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(store.entries().unwrap()["unrelated"], "preserved");
    for name in [
        "provider:init",
        "hostname",
        "ssh-keys",
        "ssh-keys:parse",
        "boot:check-in",
    ] {
        assert!(has_outcome(&store, name, Outcome::Ok), "{name}");
    }
    version.assert();
    hostname.assert();
    keys.assert();
    health.assert();
    evidence("success", &store, &requests.lock().unwrap(), None);
}

#[test]
fn azure_demo_failure() {
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
    assert!(has_outcome(&store, "ssh-keys:parse", Outcome::Error));
    assert!(has_outcome(&store, "ssh-keys", Outcome::Error));
    provider.hostname().unwrap();
    assert!(provider
        .clone()
        .boot_checkin()
        .unwrap_err()
        .to_string()
        .contains("refusing"));
    assert_eq!(store.entries().unwrap()[PROVISIONING_REPORT_KEY], encoded);

    version.assert();
    keys.assert();
    hostname.assert();
    health.assert();
    evidence("failure", &store, &requests.lock().unwrap(), Some(&error));
}

#[test]
fn azure_demo_xml_fallback() {
    for status in [404, 405, 501] {
        for failed in [false, true] {
            let mut server = Server::new();
            let pool = tempfile::tempdir().unwrap();
            let store = store(pool.path());
            let version = versions(&mut server);
            let (json_health, json_requests) = health(&mut server, "/provisioning/health", status);
            let goalstate = server
                .mock("GET", "/machine/?comp=goalstate")
                .match_header("x-ms-version", "2012-11-30")
                .with_body(GOALSTATE)
                .expect(1)
                .create();
            let (xml_health, xml_requests) = health(&mut server, "/machine/?comp=health", 200);
            let provider = provider(&server, pool.path());
            let error = if failed {
                let _keys = server.mock("GET", KEYS_PATH).with_body(BAD_KEYS).create();
                Some(provider.ssh_keys().unwrap_err())
            } else {
                provider.boot_checkin().unwrap();
                None
            };
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
            let entries = DiagnosticReader::new(store.clone()).entries().unwrap();
            assert!(entries.iter().any(|entry| matches!(
                entry, Entry::Diagnostic(diagnostic)
                    if diagnostic.key().name == "health:acknowledged"
                        && format!("{:?}", diagnostic.payload()).contains("protocol=Xml")
                        && format!("{:?}", diagnostic.payload()).contains(&status.to_string())
            )));
            version.assert();
            json_health.assert();
            goalstate.assert();
            xml_health.assert();
            if status == 404 && failed {
                let mut all_requests = json_requests.lock().unwrap().clone();
                all_requests.extend_from_slice(&requests);
                evidence("xml-failure", &store, &all_requests, error.as_ref());
            }
        }
    }
}

#[test]
fn azure_checkin_does_not_negotiate_legacy_metadata() {
    let mut server = Server::new();
    let pool = tempfile::tempdir().unwrap();
    let versions = server.mock("GET", "/?comp=versions").expect(0).create();
    let goalstate = server
        .mock("GET", "/machine/?comp=goalstate")
        .expect(0)
        .create();
    let (health, _) = health(&mut server, "/provisioning/health", 200);
    reporting(&server, pool.path()).check_in().unwrap();
    versions.assert();
    goalstate.assert();
    health.assert();
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
        let reporting = Reporting::new(
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
    let reporting = Reporting::new(
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
    let reporting = Reporting::new(
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
    let another = Reporting::new(
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
    let reporting = Reporting::new(
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
