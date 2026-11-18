---
nav_order: 2
parent: Development
---

# Azure KVP and health reporting demo

This demo uses direct `libazureinit-kvp` APIs in Afterburn's existing Azure
provider. Ignition/Flatcar still drives provisioning; there is no new provisioning
sequence, tracing subscriber, or replacement for Afterburn's slog logger.

The dependency is pinned to
[`f56cd04e16b2ec2e0aa4021953d2d52bb39e8392`](https://github.com/peytonr18/azure-init/tree/f56cd04e16b2ec2e0aa4021953d2d52bb39e8392)
on `probertson-1.88-demo-branch`, with feature `wireserver`. See its
[API guide](https://github.com/peytonr18/azure-init/blob/f56cd04e16b2ec2e0aa4021953d2d52bb39e8392/doc/health-reporting.md).
This is a demo integration, not a dependency on a published health-reporting
crate release.

## What changed

- Azure operations emit KVP observations with their name, agent/version, VM UUID,
  timestamp, outcome, and measured duration. SSH-key parsing includes validation
  and key-count observations, without publishing key material.
- Unrecovered Azure-operation errors publish a provisioning failure to KVP and
  HTTP. Internal retries and recovered DHCP fallback do not publish final failure.
- The original operation error is returned even if either reporting destination
  fails. Published reasons are safe operation summaries; the original error stays
  in the caller's error/journal, not in telemetry.
- Only the existing final check-in publishes success. A successful metadata call
  is not VM readiness. Check-in alone no longer needs legacy version negotiation.
- Health uses JSON `/provisioning/health` first. The library owns XML formatting
  and fallback, enabled only for JSON HTTP 404, 405, or 501.
- Final check-in requires HTTP acknowledgement. KVP failure is warned about but
  does not prevent successful HTTP check-in; HTTP failure makes check-in fail
  even if KVP was written.

The implementation is in the [Azure provider](../../src/providers/microsoft/azure/mod.rs)
and its [reporting helper](../../src/providers/microsoft/azure/reporting.rs).
Local executable scenarios are in
[reporting_tests.rs](../../src/providers/microsoft/azure/reporting_tests.rs).

## 1. Local/CI demonstration: no Azure account, root, or image needed

Run from the Afterburn repository root with Rust 1.88.0 available. The first
build needs access to GitHub/crates.io and the usual Afterburn build dependencies.

```sh
export AFTERBURN_AZURE_DEMO_DIR="$(mktemp -d)"
cargo +1.88.0 test --locked --all-targets azure_demo_ -- \
  --nocapture --test-threads=1
printf 'Evidence directory: %s\n' "$AFTERBURN_AZURE_DEMO_DIR"
```

The three tests call the actual Azure provider methods with loopback HTTP
fixtures and real temporary KVP files. They use a fixed test UUID, never contact
Azure, never write the system KVP pool, and never modify real users/SSH keys or
the system hostname.

They print diagnostics, decoded provisioning reports, original errors, and
captured health POST bodies. Setting `AFTERBURN_AZURE_DEMO_DIR` additionally
saves a directory for each scenario:

```text
success/
failure/
xml-failure/
  .kvp_pool_1    actual guest-pool binary file
  kvp.json       decoded diagnostics and provisioning report
  http.json      captured health requests (body is a string)
  outcome.json   operation result and original error, if any
```

Each scenario directory contains those four files. Use a fresh `mktemp -d` for
each run: the tests refuse to overwrite existing scenario directories.
Without the environment variable, tests print evidence and clean up their pools.
CI can archive the chosen directory as a test artifact without changing these
tests or installing a KVP daemon.

### Success: diagnostics, then final health

`azure_demo_success` retrieves a hostname and valid SSH keys. It asserts that
these intermediate successes have not published a provisioning report, then
calls the existing final `boot_checkin()` path. Expected evidence:

- Successful function observations and durations.
- KVP `PROVISIONING_REPORT` with `result=success` and `agent=afterburn/<version>`.
- One JSON health request: `{"state":"Ready"}`.
- An unrelated KVP record remains intact.

For readable subsets, the following commands use optional `jq`:

```sh
jq '.[] | select(.type == "diagnostic") |
    {name, result, duration, payload}' \
  "$AFTERBURN_AZURE_DEMO_DIR/success/kvp.json"
jq '.[] | select(.type == "PROVISIONING_REPORT")' \
  "$AFTERBURN_AZURE_DEMO_DIR/success/kvp.json"
jq '.[].body | fromjson' "$AFTERBURN_AZURE_DEMO_DIR/success/http.json"
```

The `health:acknowledged` observation identifies JSON/XML, attempt counts, and
the unsupported JSON status when fallback was used.

### Failure: how to trigger the malformed SSH-key case

`azure_demo_failure` serves this IMDS response to the actual Azure SSH-key
retrieval/parsing method:

```json
[{"keyData":"not-an-ssh-key","path":"/home/core/.ssh/authorized_keys"}]
```

No production force-failure flag is needed. The existing parser rejects the
key, and the new instrumentation reports the error. To rerun just this scenario
with printed evidence, without overwriting the saved three-scenario evidence:

```sh
env -u AFTERBURN_AZURE_DEMO_DIR \
  cargo +1.88.0 test --locked --all-targets azure_demo_failure -- --nocapture
```

Expected:

- The original error includes `failed to parse IMDS public key at index 0`.
- `ssh-keys:parse` and `ssh-keys` diagnostics have error outcomes.
- KVP reports `result=error`, a safe SSH-key operation reason, and supporting
  `operation=ssh-keys`.
- JSON reports `NotReady` / `ProvisioningFailed`. Its description equals the
  encoded KVP report, including the same timestamp.
- A later successful hostname call does not overwrite that failure. Check-in
  using the same provider or its clone refuses to publish `Ready`.

```sh
cat "$AFTERBURN_AZURE_DEMO_DIR/failure/outcome.json"
jq '.[] | select(.type == "PROVISIONING_REPORT")' \
  "$AFTERBURN_AZURE_DEMO_DIR/failure/kvp.json"
jq '.[].body | fromjson' "$AFTERBURN_AZURE_DEMO_DIR/failure/http.json"
```

The Rust test **passes because it asserts the expected operation error**.
`outcome.json` records that operation's failure; a green test does not mean the
SSH-key operation succeeded. HTTP 200 here acknowledges delivery of failure,
not successful provisioning.

Do not try to force this by submitting invalid SSH keys to Azure's VM-creation
API: Azure may reject them before a guest exists. Bad hostname text, empty
metadata, a nonexistent local user, or an unwritable output path are not
equivalent triggers for these Azure-method hooks.

### JSON-to-XML compatibility

`azure_demo_xml_fallback` tests both success and failure with each of 404, 405,
and 501. The saved `xml-failure` case shows JSON 404 followed by library-owned
goal-state lookup and an XML `NotReady` / `ProvisioningFailed` health POST:

```sh
jq -r '.[] | .path, .body' \
  "$AFTERBURN_AZURE_DEMO_DIR/xml-failure/http.json"
```

Run all three scenarios together to create all three evidence directories.
The tests parse the XML and compare its decoded failure description with KVP;
an XML acknowledgement never converts the failed operation into success.

For additional partial-delivery, setup, retry, and regression coverage:

```sh
env -u AFTERBURN_AZURE_DEMO_DIR \
  cargo +1.88.0 test --locked --all-targets azure
```

## 2. Inspect the binary KVP pool independently

The Rust dependency does not install its CLI. Optionally install the inspector
at the same pinned revision into a repository-local tools directory:

```sh
cargo +1.88.0 install --locked \
  --git https://github.com/peytonr18/azure-init.git \
  --rev f56cd04e16b2ec2e0aa4021953d2d52bb39e8392 \
  --root "$PWD/target/azure-kvp-tools" libazureinit-kvp
KVP="$PWD/target/azure-kvp-tools/bin/libazureinit-kvp"
"$KVP" --dir "$AFTERBURN_AZURE_DEMO_DIR/failure" --pool guest dump --parse
```

Run the evidence-producing tests first and retain their environment variable.
The final command independently reads `.kvp_pool_1` and decodes diagnostics and
the provisioning report. Without `--parse`, `dump` shows the raw key/value
records, including `DIAG|...` keys and `PROVISIONING_REPORT`.
Do not use the CLI's `clear` or report-writing commands on a live shared pool.
Those report commands write KVP only; they do not demonstrate HTTP check-in.

## 3. Optional disposable Azure VM: no rebuilt image required

Build the modified binary for the VM's architecture and compatible userspace:

```sh
cargo +1.88.0 build --locked --release
```

Copy it and, optionally, the KVP inspector to writable locations in a
**disposable test VM**. Do not overwrite the installed Afterburn or modify
active provisioning services for this initial smoke test. Check dynamic-library
compatibility if the build host and guest differ.

On the VM, use absolute paths:

```sh
AFTERBURN=/absolute/path/to/modified-afterburn
KVP=/absolute/path/to/libazureinit-kvp
WORK="$(mktemp -d)"
sudo test -r /sys/class/dmi/id/product_uuid
sudo ls -ld /var/lib/hyperv
```

For actual host collection, verify the Hyper-V KVP driver/daemon is available
and reading the same guest pool. Service names vary by distribution. This
implementation uses `/var/lib/hyperv/.kvp_pool_1`; its parent must exist and
allow access, including SELinux policy where applicable. If the pool is
unavailable, HTTP still runs but diagnostic/pool writes warn and fail.

### Show live metadata diagnostics and a success health call

```sh
sudo "$AFTERBURN" multi --provider azure --hostname "$WORK/hostname"
sudo "$KVP" --dir /var/lib/hyperv --pool guest dump --parse
sudo "$AFTERBURN" multi --provider azure --check-in
sudo "$KVP" --dir /var/lib/hyperv --pool guest dump --parse
```

The hostname goes into a scratch file, not the live hostname configuration.
Only the check-in command sends final success. Its slog output reports the
accepting protocol, attempts, and fallback status. Both the explicit `multi`
syntax above and the existing legacy `--provider azure --check-in` syntax work.

### Optional forced IMDS failure, only after rehearsal

A per-service systemd network filter can deny IMDS (`169.254.169.254`) while
leaving WireServer (normally `168.63.129.16`, discovered by existing DHCP logic)
reachable. This requires systemd/cgroup-BPF support. It changes neither global
firewall rules nor installed provisioning services:

```sh
sudo systemd-run --unit=afterburn-azure-failure-demo --wait --pipe --collect \
  --property=Type=oneshot \
  --property=IPAddressDeny=169.254.169.254/32 \
  --property=TimeoutStartSec=120s \
  "$AFTERBURN" multi --provider azure --hostname "$WORK/blocked-hostname"
```

A nonzero result is expected. If running under a shell with `set -e`, execute
this as the condition of an `if` statement so subsequent inspection still runs.
Metadata has its existing retries, independent of the 15-second health budget.
The network filter does not provide a shortened metadata retry policy.

```sh
sudo journalctl -u afterburn-azure-failure-demo --no-pager
sudo "$KVP" --dir /var/lib/hyperv --pool guest dump --parse
```

Verify the error is the **hostname IMDS request**, not missing libraries,
provider version negotiation, unsupported filtering, or a service timeout.
If the filter is not enforced or an environment proxy bypasses the intended
route, this is not a valid fault-injection result. Use the deterministic local
fixture if this cannot be rehearsed reliably.

Do not run a separate success check-in immediately afterward: that new process
could overwrite the failure report. Save the evidence first and use separate
fresh scenarios/VMs for independent success/failure demonstrations.

## 4. What the demo proves, and what it does not

| Evidence | What it proves |
|---|---|
| Local provider tests and captured requests | The actual Afterburn Azure methods produce diagnostics, preserve an operation error, and deliver correctly encoded KVP/JSON/XML reports to controlled endpoints. |
| Decoded local `.kvp_pool_1` | The library wrote readable guest-format records; not that a hypervisor collected them. |
| Manual run on an already booted Azure VM | Real guest/fabric transport smoke coverage; not a first-boot provisioning experiment. |
| Independent host/fabric observations on fresh VMs | Required before claiming Azure accepted the intended provisioning state or host collection worked end to end. |

Pool writes do not themselves notify the host. HTTP acknowledgement is not proof
of Azure-side provisioning completion. KVP and HTTP are independent delivery
destinations, not a transaction. KVP contains the operation result even when
HTTP delivery fails; final check-in still returns an HTTP delivery error.

The failure latch is shared by clones of one provider, not an atomic
cross-process/boot coordinator. This demo neither clears pools nor resets
in-progress on every invocation. Existing callers remain responsible for when
final readiness is appropriate.

Generic output-file writes and local user lookup happen outside the Azure
methods. Ignition fetch/parse/disk/file errors can also happen without Afterburn
running. These hooks do not automatically observe those errors.

### Follow-up slide: reporting failures outside Afterburn

**Question:** How should Flatcar/Ignition hand an external provisioning failure
to Afterburn's Azure reporter, and which boot-time integration point should
provide that handoff?

One candidate, not implemented in this demo:

```text
systemd -> wrapper -> unchanged Ignition stage
                       |
                       +-- nonzero -> new Afterburn reporting-only command
                                      -> diagnostics + KVP/HTTP failure
                    wrapper returns original Ignition status
                    existing emergency handling continues
```

The reporting-only command would live in Afterburn; the wrapper and service
launch changes would live in Flatcar's boot integration. They must be present
in the initramfs before the bad config is processed. No change to Ignition's
parser is necessary, but boot ordering, networking, bounded reporting,
cross-process final-state policy, and pool persistence need investigation.
This is not required for the local demo or manual VM smoke test above.

## Validation before presenting

```sh
cargo +1.88.0 test --locked --all-targets
cargo +1.88.0 clippy --locked --all-targets -- -D warnings
cargo +1.88.0 fmt -- --check -l
```

The reviewed baseline also produces 29 pre-existing
`clippy::uninlined_format_args` findings under the strict Clippy command above.
They reproduce without this integration. To check the remaining lints without
editing unrelated code, use:

```sh
cargo +1.88.0 clippy --locked --all-targets -- \
  -D warnings -A clippy::uninlined_format_args
```

This exception is a validation workaround, not a change to the repository's
lint policy.

Keep the recorded Git revisions, local evidence directory, and a captured
terminal run available as fallback. Present fixture results as fixture results,
not as observations from an actual failed first boot.
