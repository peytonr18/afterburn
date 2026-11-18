// Copyright 2017 CoreOS, Inc.
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

//! Azure provider, metadata and wireserver fetcher.

pub(crate) mod config;
mod reporting;

use super::goalstate;

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use libazureinit_kvp::Outcome;
use openssh_keys::PublicKey;
use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use slog_scope::warn;

use crate::providers::MetadataProvider;
use crate::retry;
use nix::unistd::Uid;
use reporting::Reporting;

#[cfg(test)]
mod mock_tests;
#[cfg(test)]
mod reporting_tests;

static HDR_AGENT_NAME: &str = "x-ms-agent-name";
static HDR_VERSION: &str = "x-ms-version";

const MS_AGENT_NAME: &str = "com.coreos.afterburn";
const MS_VERSION: &str = "2012-11-30";

/// This is a known working wireserver endpoint within Azure.
/// See: https://blogs.msdn.microsoft.com/mast/2015/05/18/what-is-the-ip-address-168-63-129-16/
#[cfg(not(test))]
const FALLBACK_WIRESERVER_ADDR: [u8; 4] = [168, 63, 129, 16]; // for grep: 168.63.129.16

#[derive(Debug, Deserialize, Clone)]
struct Versions {
    #[serde(rename = "Supported")]
    pub supported: Supported,
}

#[derive(Debug, Deserialize, Clone)]
struct Supported {
    #[serde(rename = "Version", default)]
    pub versions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Azure {
    client: retry::Client,
    endpoint: IpAddr,
    reporting: Reporting,
}

#[derive(Debug, Default)]
struct Attributes {
    pub virtual_ipv4: Option<IpAddr>,
    pub dynamic_ipv4: Option<IpAddr>,
}

#[derive(Debug, Deserialize)]
struct ImdsSshKey {
    #[serde(rename = "keyData")]
    key_data: String,
    #[serde(default)]
    path: String,
}

fn imds_key_path_context(path: &str) -> String {
    let value = path.trim();
    if value.is_empty() {
        String::new()
    } else {
        format!(" (path: {value})")
    }
}

pub(crate) fn parse_imds_public_keys(body: &str) -> Result<Vec<PublicKey>> {
    let items: Vec<ImdsSshKey> =
        serde_json::from_str(body).context("failed to parse IMDS publicKeys JSON")?;

    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let key_data = item.key_data.replace(['\r', '\n'], "").trim().to_string();

            if key_data.is_empty() {
                anyhow::bail!(
                    "IMDS publicKeys entry at index {}{} has empty keyData",
                    index,
                    imds_key_path_context(&item.path),
                );
            }

            PublicKey::parse(&key_data).with_context(|| {
                format!(
                    "failed to parse IMDS public key at index {}{}",
                    index,
                    imds_key_path_context(&item.path)
                )
            })
        })
        .collect()
}

impl Azure {
    /// Try to build a new provider agent for Azure.
    ///
    /// This internally tries to reach the WireServer and verify compatibility.
    pub fn try_new() -> Result<Self> {
        let (endpoint, reporting) = Self::platform_reporting();
        Self::with_client(None, endpoint, reporting)
    }

    pub(crate) fn check_in() -> Result<()> {
        let (_, reporting) = Self::platform_reporting();
        reporting.check_in()
    }

    fn platform_reporting() -> (IpAddr, Reporting) {
        let started = Instant::now();
        let endpoint = Self::get_fabric_address();
        let reporting = Reporting::for_endpoint(format!("http://{endpoint}/provisioning/health"));
        reporting.event(
            "fabric:discovery",
            format!("selected WireServer address {endpoint}"),
            Some(Outcome::Ok),
            Some(started.elapsed()),
        );
        (endpoint, reporting)
    }

    fn with_client(
        client: Option<retry::Client>,
        endpoint: IpAddr,
        reporting: Reporting,
    ) -> Result<Azure> {
        reporting.clone().operation("provider:init", || {
            let mut client = match client {
                Some(c) => c,
                None => retry::Client::try_new()?,
            };

            // Add headers required by the metadata API.
            client = client
                .header(
                    HeaderName::from_static(HDR_AGENT_NAME),
                    HeaderValue::from_static(MS_AGENT_NAME),
                )
                .header(
                    HeaderName::from_static(HDR_VERSION),
                    HeaderValue::from_static(MS_VERSION),
                );

            let azure = Azure {
                client,
                endpoint,
                reporting,
            };

            azure
                .reporting
                .observe("fabric:versions", || azure.is_fabric_compatible(MS_VERSION))
                .inspect_err(|_e| {
                    if !Uid::current().is_root() {
                        // See https://github.com/coreos/bugs/issues/2468.
                        warn!("unable to reach Azure endpoints, please check whether firewall rules are blocking access to them");
                    }
                })
                .context("failed version compatibility check")?;

            Ok(azure)
        })
    }

    /// Retrieve `goalstate` content from the WireServer.
    fn fetch_goalstate(&self) -> Result<goalstate::GoalState> {
        self.reporting.observe("fabric:goalstate", || {
            self.client
                .get(
                    retry::Xml,
                    format!("{}/machine/?comp=goalstate", self.fabric_base_url()),
                )
                .send()
                .context("failed to get goal state")?
                .ok_or_else(|| anyhow!("failed to get goal state: not found response"))
        })
    }

    #[cfg(not(test))]
    fn get_fabric_address() -> IpAddr {
        // try to fetch from dhcp, else use fallback; this is similar to what WALinuxAgent does
        Azure::get_fabric_address_from_dhcp().unwrap_or_else(|e| {
            warn!("Failed to get fabric address from DHCP: {}", e);
            slog_scope::info!("using fallback address");
            IpAddr::from(FALLBACK_WIRESERVER_ADDR)
        })
    }

    #[cfg(not(test))]
    fn get_fabric_address_from_dhcp() -> Result<IpAddr> {
        let v = crate::util::DhcpOption::AzureFabricAddress.get_value()?;
        // value is an 8 digit hex value, with colons if it came from
        // NetworkManager.  Convert it to u32 and then parse that into an
        // IP.  Ipv4Addr::from(u32) performs conversion from big-endian.
        slog_scope::trace!("found fabric address in hex - {:?}", v);
        let dec = u32::from_str_radix(&v.replace(':', ""), 16)
            .with_context(|| format!("failed to convert '{v}' from hex"))?;
        Ok(IpAddr::V4(dec.into()))
    }

    fn fabric_base_url(&self) -> String {
        format!("http://{}", self.endpoint)
    }

    #[cfg(test)]
    fn get_fabric_address() -> IpAddr {
        use std::net::Ipv4Addr;
        IpAddr::from(Ipv4Addr::new(127, 0, 0, 1))
    }

    fn is_fabric_compatible(&self, version: &str) -> Result<()> {
        let versions: Versions = self
            .client
            .get(
                retry::Xml,
                format!("{}/?comp=versions", self.fabric_base_url()),
            )
            .send()
            .context("failed to get versions")?
            .ok_or_else(|| anyhow!("failed to get versions: not found"))?;

        if versions.supported.versions.iter().any(|v| v == version) {
            Ok(())
        } else {
            Err(anyhow!(
                "fabric version '{}' not supported by the WireServer at '{}'",
                version,
                self.endpoint
            ))
        }
    }

    fn metadata_endpoint() -> String {
        "http://169.254.169.254".into()
    }

    fn get_attributes(&self) -> Result<Attributes> {
        use std::net::SocketAddr;

        let goalstate = self.fetch_goalstate()?;
        let endpoint = &goalstate
            .container
            .role_instance_list
            .role_instances
            .first()
            .context("goal state has no role instances")?
            .configuration
            .shared_config;

        let shared_config: goalstate::SharedConfig =
            self.reporting.observe("fabric:shared-config", || {
                self.client
                    .get(retry::Xml, endpoint.to_string())
                    .send()
                    .context("failed to get shared configuration")?
                    .ok_or_else(|| anyhow!("failed to get shared configuration: not found"))
            })?;

        let mut attributes = Attributes::default();

        for instance in shared_config.instances.instances {
            if instance.id == shared_config.incarnation.instance {
                attributes.dynamic_ipv4 = Some(instance.address.parse().with_context(|| {
                    format!("failed to parse instance ip address: {}", instance.address)
                })?);
                for endpoint in instance.input_endpoints.endpoints {
                    attributes.virtual_ipv4 =
                        match endpoint.load_balanced_public_address.parse::<SocketAddr>() {
                            Ok(lbpa) => Some(lbpa.ip()),
                            Err(_) => continue,
                        };
                }
            }
        }

        Ok(attributes)
    }

    fn fetch_hostname(&self) -> Result<Option<String>> {
        const NAME_URL: &str = "metadata/instance/compute/name?api-version=2017-08-01&format=text";
        let url = format!("{}/{}", Self::metadata_endpoint(), NAME_URL);

        let name = self
            .client
            .clone()
            .header(
                HeaderName::from_static("metadata"),
                HeaderValue::from_static("true"),
            )
            .get(retry::Raw, url)
            .send()
            .context("failed to get hostname")?;
        Ok(name)
    }

    fn fetch_vmsize(&self) -> Result<String> {
        const VMSIZE_URL: &str =
            "metadata/instance/compute/vmSize?api-version=2017-08-01&format=text";
        let url = format!("{}/{}", Self::metadata_endpoint(), VMSIZE_URL);

        let vmsize = self
            .client
            .clone()
            .header(
                HeaderName::from_static("metadata"),
                HeaderValue::from_static("true"),
            )
            .get(retry::Raw, url)
            .send()?
            .context("failed to get vmsize")?;
        Ok(vmsize)
    }

    fn fetch_admin_username(&self) -> Result<Option<String>> {
        const URL: &str =
            "metadata/instance/compute/osProfile/adminUsername?api-version=2021-02-01&format=text";
        let url = format!("{}/{}", Self::metadata_endpoint(), URL);

        let username = self
            .client
            .clone()
            .header(
                HeaderName::from_static("metadata"),
                HeaderValue::from_static("true"),
            )
            .get(retry::Raw, url)
            .send::<String>()
            .context("failed to query IMDS for adminUsername")?;

        match username.map(|u| u.trim().to_owned()) {
            Some(u) if !u.is_empty() => Ok(Some(u)),
            _ => Ok(None),
        }
    }

    fn fetch_ssh_keys(&self) -> Result<Vec<PublicKey>> {
        const URL: &str = "metadata/instance/compute/publicKeys?api-version=2021-02-01";
        let url = format!("{}/{}", Self::metadata_endpoint(), URL);

        let body = self
            .client
            .clone()
            .header(
                HeaderName::from_static("metadata"),
                HeaderValue::from_static("true"),
            )
            .get(retry::Raw, url)
            .send::<String>()
            .context("failed to query IMDS for publicKeys")?
            .ok_or_else(|| anyhow::anyhow!("IMDS did not return a publicKeys payload"))?;

        self.reporting.event(
            "imds:public-keys",
            "received SSH-key metadata; validating keys",
            None,
            None,
        );
        let keys = self
            .reporting
            .observe("ssh-keys:parse", || parse_imds_public_keys(&body))?;
        self.reporting.event(
            "ssh-keys:count",
            format!("validated {} SSH keys", keys.len()),
            Some(Outcome::Ok),
            None,
        );
        Ok(keys)
    }
}

impl MetadataProvider for Azure {
    fn attributes(&self) -> Result<HashMap<String, String>> {
        self.reporting.operation("attributes", || {
            let attributes = self.get_attributes()?;
            let vmsize = self
                .reporting
                .observe("imds:vm-size", || self.fetch_vmsize())?;
            let mut out = HashMap::with_capacity(3);

            if let Some(virtual_ipv4) = attributes.virtual_ipv4 {
                out.insert("AZURE_IPV4_VIRTUAL".to_string(), virtual_ipv4.to_string());
            }

            if let Some(dynamic_ipv4) = attributes.dynamic_ipv4 {
                out.insert("AZURE_IPV4_DYNAMIC".to_string(), dynamic_ipv4.to_string());
            }

            out.insert("AZURE_VMSIZE".to_string(), vmsize);

            Ok(out)
        })
    }

    fn hostname(&self) -> Result<Option<String>> {
        self.reporting
            .operation("hostname", || self.fetch_hostname())
    }

    fn admin_username(&self) -> Result<Option<String>> {
        self.reporting
            .operation("admin:username", || self.fetch_admin_username())
    }

    fn admin_password_hash(&self) -> Result<Option<String>> {
        self.reporting.operation("admin:password-hash", || {
            config::read_ovf_admin_password()?
                .map(|password| config::hash_admin_password(&password))
                .transpose()
        })
    }

    fn ssh_keys(&self) -> Result<Vec<PublicKey>> {
        self.reporting
            .operation("ssh-keys", || self.fetch_ssh_keys())
    }

    fn boot_checkin(&self) -> Result<()> {
        self.reporting.check_in()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_imds_public_keys_rejects_malformed_key() {
        let body = r#"
[
    {
        "keyData": "not-an-ssh-key",
        "path": "/home/core/.ssh/authorized_keys"
    }
]
"#;

        let err = parse_imds_public_keys(body).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("failed to parse IMDS public key"));
        assert!(message.contains("/home/core/.ssh/authorized_keys"));
    }

    #[test]
    fn test_parse_imds_public_keys_accepts_valid_key() {
        let body = r#"
[
    {
        "keyData": "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQDYVEprvtYJXVOBN0XNKVVRNCRX6BlnNbI+USLGais1sUWPwtSg7z9K9vhbYAPUZcq8c/s5S9dg5vTHbsiyPCIDOKyeHba4MUJq8Oh5b2i71/3BISpyxTBH/uZDHdslW2a+SrPDCeuMMoss9NFhBdKtDkdG9zyi0ibmCP6yMdEX8Q== Generated by Nova",
        "path": "/home/core/.ssh/authorized_keys"
    }
]
"#;

        let keys = parse_imds_public_keys(body).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0]
            .to_key_format()
            .starts_with("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQDYVEprvtYJ"));
    }

    #[test]
    fn test_parse_imds_public_keys_rejects_empty_key_data() {
        // Exercises the empty-keyData guard distinct from the
        // PublicKey::parse failure path.
        let body = r#"
[
    {
        "keyData": "   ",
        "path": "/home/core/.ssh/authorized_keys"
    }
]
"#;

        let err = parse_imds_public_keys(body).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("empty keyData"),
            "unexpected error: {message}"
        );
        assert!(message.contains("/home/core/.ssh/authorized_keys"));
    }

    #[test]
    fn test_parse_imds_public_keys_strips_embedded_newlines() {
        // Some sources copy keys with hard-wrapped CR/LF newlines; the
        // parser must normalize them before handing to PublicKey::parse.
        // We inject newlines into a known-valid key (the same one used
        // by the mock-tests fixture) so the test exercises real key bytes.
        const VALID_KEY: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCzTI2wld1/Ib3q2N8ynJoTk+1PfanCoPdcYXxd7k8DK/T7BlvD3AHajUiqJ4Im2GfyMCloBkJGbI4/utt7qCF6y3Vb5Suxdd5/nXpAr75ocHkg43TzZdU5zSPHdgLefe2VEKXokAobU13wHAwj6d6bJaTpAJx5MkQJAb88HD1LtBFMz5C8b+wVloxB+Zusj0dX/Bc+rZFo62KW50BoaWxDzO5jN18DGuamcN34WBCkMmRvVrKGQaOru+rBVnxeQtVw+hygq7rb6zekar9zyEyW5IvaZiRkqC60QiycV7I9fIxRJfp8FvrlusiVyWpsLILL3CxK95c8Sju4qQN6AofTh52XJtFkdP8ngXKrobXqrcLS5GaEnAo6BZowUt9cpr7HdmdqJmYk3+ueJOrLiAkRE2Gguc28sbpQl/ok4vHWhXEi/GzK+FK0lrN5L7LY5D4MfJ1XZ5sZw4ulJXjiB3x/aKLT0lLFc3lNitl+UPw46Lp1PTR0dJkYNyvZvEuWLgk= core@host";

        // Splice CRLF mid-base64 and a trailing LF; normalization must
        // restore the original key byte-for-byte.
        let (head, tail) = VALID_KEY.split_at(60);
        let wrapped = format!("{head}\\r\\n{tail}\\n");
        let body =
            format!(r#"[{{"keyData": "{wrapped}", "path": "/home/core/.ssh/authorized_keys"}}]"#);

        let keys = parse_imds_public_keys(&body).unwrap();
        assert_eq!(keys.len(), 1);
        let serialized = keys[0].to_key_format();
        assert!(
            !serialized.contains('\n') && !serialized.contains('\r'),
            "serialized key contained line endings: {serialized:?}"
        );
        assert!(serialized.starts_with("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQ"));
    }

    #[test]
    fn test_parse_imds_public_keys_preserves_multiple_keys_in_order() {
        let body = r#"
[
    {
        "keyData": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIE5dnA1jzwJZ2ndPmoZAvB1MDLU+UJpw0o9EzGm6tF5Y first@host",
        "path": "/home/core/.ssh/authorized_keys"
    },
    {
        "keyData": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBFmlqemnJgtwQfrqlmwR0sFE+wLDmHGm2I0i/uVy/JJ second@host",
        "path": "/home/core/.ssh/authorized_keys"
    }
]
"#;

        let keys = parse_imds_public_keys(body).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys[0].to_key_format().ends_with(" first@host"));
        assert!(keys[1].to_key_format().ends_with(" second@host"));
    }

    #[test]
    fn test_parse_imds_public_keys_handles_missing_path_field() {
        // `path` is marked `#[serde(default)]`; payloads that omit it
        // should parse and produce an error message without a `(path: )`
        // suffix when keyData is invalid.
        let body = r#"
[
    {
        "keyData": "not-an-ssh-key"
    }
]
"#;

        let err = parse_imds_public_keys(body).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("index 0"), "unexpected error: {message}");
        assert!(
            !message.contains("(path:"),
            "empty path should not appear in error: {message}"
        );
    }
}
