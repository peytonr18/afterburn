---
nav_order: 2
parent: Development
---

# Azure demo: read the KVP log, then show what WireServer accepted

The two cases to present are **Ready** and **a failed Ignition service**. Both
run the same Azure check-in code. Afterburn reads systemd's currently failed
units, writes observations with `DiagnosticWriter`, and uses `HealthWriter` to
publish one final report to the guest KVP pool **and** WireServer. No tracing
subscriber or Ignition changes are involved.

## 1. Run both cases locally or in CI

From the Afterburn checkout on Linux (Rust 1.88.0; no root, systemd, or Azure
account needed):

```sh
export AFTERBURN_AZURE_DEMO_DIR="$(mktemp -d)"
cargo +1.88.0 test --locked --all-targets azure_demo_ -- --nocapture --test-threads=1
```

| Evidence directory | Service snapshot supplied to check-in | Expected KVP report | Expected HTTP POST |
|---|---|---|---|
| `success/` | No watched service failed | `result=success` | `{"state":"Ready"}` |
| `ignition-failure/` | `ignition-fetch.service` failed | `result=error`, reason names the unit | `NotReady` / `ProvisioningFailed`; description is the exact encoded KVP report |

These are real pool writes and real HTTP requests to a **loopback receiver**;
the service snapshots and metadata are fixtures. In `success/`, look for
`provider:init`, `hostname`, `imds:public-keys`, and `ssh-keys` **before** the
health events. The test fetches a hostname and queries SSH keys (an empty list)
and checks that neither a provisioning report nor a health POST exists yet.
The [Azure metadata methods](../../src/providers/microsoft/azure/mod.rs)
also record `attributes`, `admin:username`, and `admin:password-hash` when
invoked. Operation outcomes include elapsed time.
The failure test passes because it verifies
check-in **returned an error** and sent one failure POST, not a subsequent `Ready`.
Use a fresh output directory for each run. Each case saves `.kvp_pool_1`,
`kvp.json` (decoded pool), `http.json` (receiver capture), and `outcome.json`.

**Presenter's commands** (optional `jq`):

```sh
for case in success ignition-failure; do
  dir="$AFTERBURN_AZURE_DEMO_DIR/$case"
  printf '\n=== %s ===\n' "$case"
  jq -r '.[] | select(.type=="diagnostic") | "\(.name): \(.payload)"' "$dir/kvp.json"
  jq '.[] | select(.type=="PROVISIONING_REPORT") | {result, reason, timestamp}' "$dir/kvp.json"
  jq '.[] | {path, body: (.body | fromjson)}' "$dir/http.json"
done
```

For the in-function steps, show the hostname request and response from that same pool:

```sh
jq -r '.[] | select(.type=="diagnostic" and .name=="imds:hostname") | .payload' \
  "$AFTERBURN_AZURE_DEMO_DIR/success/kvp.json"
# Fetching hostname from IMDS: GET http://169.254.169.254/metadata/instance/compute/name?api-version=2017-08-01&format=text
# IMDS returned hostname: demo-node
```

In the fixture the URL is the provider's logical IMDS URL; its client redirects
the request to a loopback mock. On a guest, the same messages describe the real
request. Lookups also log the WireServer version check, goal-state/shared-config
steps, VM size and SSH-key count. Only short hostname/VM size labels are echoed.
Admin usernames and passwords report presence only; keys, hashes, raw bodies
and goal-state-provided URLs are never copied into KVP. A failed IMDS request
records the attempted URL and `IMDS GET failed`, not a made-up returned value.

Point out **two different facts**: `PROVISIONING_REPORT` is the latest
*provisioning result*; a diagnostic saying **“WireServer acknowledged health
report”** is Afterburn's observation of *HTTP delivery*. Match `result` and
`report_timestamp` in the diagnostic to `result` and `timestamp` in the
report, rather than relying on adjacent lines: the report key is reused and
diagnostics are appended. An acknowledged **failure** is still a failure. The pool is not an
HTTP capture; `http.json` shows the actual POST in the local demo. HTTP
acknowledgement alone does not establish Azure's final provisioning state.

The sequence in the decoded pool is:

1. `health:send`: **“Initiating health report to wireserver: Ready”** (or
   `NotReady`). This records intent; it is not evidence that the peer received it.
2. `health:acknowledged`: **“WireServer acknowledged health report”**, with
   the protocol, JSON POST attempt count (`attempts`), XML attempts and any
   fallback status. The API confirms a 2xx, not the exact success code or headers.
3. **Instead of** an acknowledgement on delivery failure, look for
   `health:unacknowledged`: **“Failed to send provisioning report to wireserver”**.
   It includes an HTTP error status if available. A setup failure can produce
   this diagnostic without a send. No raw headers or response bodies go into KVP.

### Read the binary pool independently

Install the optional inspector from the same Git revision (the library
dependency does not install it). `dump --parse` only reads the pool:

```sh
cargo +1.88.0 install --locked --git https://github.com/peytonr18/azure-init.git \
  --rev f56cd04e16b2ec2e0aa4021953d2d52bb39e8392 \
  --root "$PWD/target/azure-kvp-tools" libazureinit-kvp
KVP="$PWD/target/azure-kvp-tools/bin/libazureinit-kvp"
"$KVP" --dir "$AFTERBURN_AZURE_DEMO_DIR/ignition-failure" --pool guest dump --parse
```

## 2. First boot on Azure: pass actual good and bad user data

**Image build checklist:**

- Pin the image build to a commit containing the entire demo, including
  [boot_services.rs](../../src/providers/microsoft/azure/boot_services.rs).
  For an offline package build, include the locked Git dependency in the vendor sources.
- Check the image recipe, not just the Cargo build. The [stock Flatcar
  ebuild][flatcar-ebuild] enables `cl-legacy` (not a feature of this checkout),
  renames the binary to `coreos-metadata`, and installs legacy metadata units.
  If using that recipe, adapt it or package the demo alongside it. Make sure
  the metadata invocations you want to observe **and** the enabled Azure
  [check-in service](../../systemd/afterburn-checkin.service) use the demo
  binary. This repo's unit expects `/usr/bin/afterburn` and `ignition.platform.id=azure`.
- Use your normal image build. **No Make flag, drop-in or guest setup is
  required for KVP readiness.** The Azure binary handles it even when the image
  supplies its own check-in unit. On the real root filesystem, if the VM UUID
  is available but `/var/lib/hyperv` is missing, it asks systemd to start
  `hypervkvpd.service` and waits up to 30 seconds for the directory. An existing
  directory needs no wait or daemon start. Afterburn does not create, chmod or
  clear that directory. The image still needs its usual Hyper-V daemon for host
  collection. If the start request is denied it still waits, since the image
  may have scheduled the daemon independently. A KVP timeout does not suppress HTTP.
- For a manual emergency-shell demo, include the
  binary and its libraries in the initramfs too. An initramfs-only pool is not
  automatically the pool the post-boot daemon reads.

Use **two fresh disposable VMs** from the **same generalized Flatcar image**.
The image must **already** have this Afterburn build installed, its Azure
check-in unit enabled and applicable, and guest pool access. A malformed config
cannot install its own reporter. Enable boot diagnostics and arrange serial
console access *before* boot. No `ignition-validate` or stage wrapper is used.

For example, set `RG`, `IMAGE`, `SSH_PUBKEY` and `BOOTDIAG_STORAGE` to your
test resource group, modified image ID, public key file, and pre-existing boot
diagnostics storage account. Add your image's usual network/plan flags as needed.
Use a config version supported by your image:

```sh
printf '%s\n' '{"ignition":{"version":"3.4.0"}}' > good.ign
printf '%s\n' '{"ignition":{"version":"3.4.0"}'  > bad.ign  # missing final }
for case in good bad; do
  az vm create -g "$RG" -n "afterburn-$case" --image "$IMAGE" \
    --admin-username core --ssh-key-values "$SSH_PUBKEY" \
    --boot-diagnostics-storage "$BOOTDIAG_STORAGE" --user-data "./$case.ign" --no-wait
done
```

Use distinct, unused VM names. [Flatcar's Azure guide](https://www.flatcar.org/docs/latest/deploy/cloud/azure/#butane-config)
also documents custom data; use the input your image consumes. Azure accepting
the create request is **not** a check-in acknowledgement.

1. **Good VM:** confirm the modified check-in ran (`journalctl -b -u
   afterburn-checkin.service`). On the guest, use the matching inspector:
   `sudo /path/to/libazureinit-kvp --dir /var/lib/hyperv --pool guest dump --parse`.
   Look for the report and its matching acknowledgement, just as in the local demo.
2. **Bad VM:** use Boot Diagnostics / Serial Console; do not assume SSH works.
   From the deployment machine: `az vm boot-diagnostics get-boot-log -g "$RG"
   -n afterburn-bad`. If a guest shell is available, check:

   ```sh
   systemctl --failed --type=service
   journalctl -b -u ignition-fetch-offline.service -u ignition-fetch.service \
     -u afterburn-checkin.service --no-pager
   ```

3. **Did check-in actually run?** If the failed unit was still visible, expect
   the error report and a `NotReady` POST; inspect the pool **before rerunning
   anything**. If Ignition entered emergency mode before late check-in, there
   may be no report. A timeout or absence of `Ready` is *not* proof of `NotReady`.
4. **Optional manual check-in:** from the emergency shell, only if the failed
   watched unit is still listed and the **modified** binary is available there,
   run `/usr/bin/afterburn multi --provider azure --check-in`. Expect nonzero
   for a failed service. Capture the pool and console output there; an initramfs
   pool may disappear at switch-root and HTTP requires WireServer reachability.
   Label this **manual reporting after a real first-boot failure**, not automatic
   delivery. If the binary or failed state is unavailable, use the local demo.

### WireServer acknowledged Ready but the guest pool is empty

`Could not write Azure diagnostic: No such file or directory` followed by
`Azure KVP report failed, but WireServer acknowledged it` means **HTTP succeeded,
KVP did not**. The writer can create `.kvp_pool_1` but not its parent directory.
Check both units on the **same boot** and use the daemon name from the guest:

```sh
sudo journalctl -b -o short-monotonic --no-pager -l \
  -u afterburn-checkin.service -u hypervkvpd.service
systemctl show hypervkvpd.service -p Type -p ActiveState
sudo stat /var/lib/hyperv /var/lib/hyperv/.kvp_pool_1
```

Seeing `afterburn-checkin.service` in the journal doesn't tell you which unit
file the image packaged. `systemctl cat afterburn-checkin.service` shows its
source and `ExecStart`; what matters is that this command runs the new binary.
The repo's unit also orders itself after `hypervkvpd.service` when both jobs
are scheduled. The binary checks for the directory itself, since a daemon
being marked started is not proof its files are ready.

On an **older disposable good VM only**, if the daemon is now active and the
directory exists, you can confirm the diagnosis without rebuilding:

```sh
sudo systemctl restart afterburn-checkin.service
sudo /path/to/libazureinit-kvp --dir /var/lib/hyperv --pool guest dump --parse
```

Restarting deliberately sends a **new** health report; save existing evidence
first. This restart is **not** part of the new image setup. Lost diagnostics are
not replayed. Initramfs invocations do not wait for a post-switch-root daemon.
If the pool exists but is still empty,
check the full error, write permissions, mount namespace and SELinux policy.

## 3. Optional middle ground: real systemd, no broken boot or Azure account

On a disposable systemd VM with `ignition-validate` installed, create the
`bad.ign` above. Run the validator in its **own** transient unit, not in an
installed Ignition unit. Check that the journal shows a parse error rather than
a missing binary. Don't use `--collect` or reset the unit before the test:

```sh
unit=ignition-demo-validate.service
sudo systemd-run --unit="$unit" --property=Type=oneshot --wait \
  "$(command -v ignition-validate)" "$PWD/bad.ign"  # expected nonzero
systemctl show "$unit" -p ActiveState -p Result -p ExecMainStatus
sudo journalctl -b -u "$unit" --no-pager -n 10
cargo +1.88.0 test --locked azure_live_failed_validator -- --ignored --nocapture
sudo systemctl reset-failed "$unit"  # after saving evidence
```

This opt-in test reads the **real** system manager and uses a temporary pool and
loopback health receiver. Alternatively, on a disposable Azure guest, run the
modified binary's `multi --provider azure --check-in` to use the real endpoints.

## Scope and implementation

- [Service selection](../../src/providers/microsoft/azure/boot_services.rs) is
  read-only. Missing/skipped services are not failed services. If systemd
  cannot be queried, check-in withholds `Ready` and emits a diagnostic; it
  does not invent an Ignition failure or overwrite the report.
- [Reporting](../../src/providers/microsoft/azure/reporting.rs) also publishes
  unrecovered Azure metadata errors, without exporting key material or config.
  Run `cargo +1.88.0 test --locked azure_metadata_failure` for the secondary
  malformed-key test. Intermediate successes never send `Ready`.
- HTTP and KVP are independent. If HTTP fails, an available pool records the
  lack of acknowledgement. If KVP is unwritable, use the guest journal; an
  acknowledged HTTP POST can still succeed. XML fallback on JSON 404/405/501
  and retries belong to the library, not Afterburn.
- Health telemetry follows [Azure-init's JSON POST messages][upstream],
  the pinned demo branch's [acknowledgement][sender], and its
  [delivery failure handler][send-error]. Afterburn calls the KVP APIs directly;
  it does not install a subscriber, duplicate the POST, or log invented
  per-attempt responses. See also the [library contract][api].

[upstream]: https://github.com/Azure/azure-init/blob/7cb30e07322f6f21d9e649960c641d1d2b57095a/libazureinit/src/wireserver.rs#L92-L181
[sender]: https://github.com/peytonr18/azure-init/blob/f56cd04e16b2ec2e0aa4021953d2d52bb39e8392/libazureinit/src/wireserver.rs#L21-L101
[send-error]: https://github.com/peytonr18/azure-init/blob/f56cd04e16b2ec2e0aa4021953d2d52bb39e8392/src/main.rs#L340-L351
[api]: https://github.com/peytonr18/azure-init/blob/f56cd04e16b2ec2e0aa4021953d2d52bb39e8392/doc/health-reporting.md
[flatcar-ebuild]: https://github.com/flatcar/coreos-overlay/blob/c8d38f9460ab3e8234b2549753f4870ada172372/coreos-base/afterburn/afterburn-4.0.0-r4.ebuild#L250-L260
