#!/usr/bin/env python3
"""Operator-only Boat spill-provider measurements (Python 3.10+, standard library).

Usage: <key reader> | python3 script/boat-probe/probe.py --key-stdin --output DIR
Or set BOAT_API_KEY in the parent environment; never pass the key as an argument.
DIR must be new. JSON evidence is checkpointed there and printed after cleanup.
Recover an interrupted run: same credentials, --cleanup DIR/results.json.
Render evidence without credentials: --render DIR/results.json --findings FILE.
Exit status: 0 when the verdict passed (cleanup complete, or Boat accepted every deletion and
only its final purge is pending), 1 when the verdict failed, 3 when resources may still exist
and need --cleanup. JSON `verdict` and `cleanup_state` report the two separately.
"""
from __future__ import annotations

import argparse
import base64
import contextlib
from datetime import datetime, timezone
import json
import os
import re
from pathlib import Path
import shlex
import signal
import statistics
import sys
import time
import urllib.error
import urllib.request
import uuid

from reach import tcp_probe

API = "https://boat.dev/api/v1"
READY = {"ready", "idle", "running"}


class ProbeError(Exception):
    pass


class ApiError(ProbeError):
    def __init__(self, status, body):
        self.status, self.body = status, body
        super().__init__(f"Boat HTTP {status}: {body}")


def scrub(value, key):
    if isinstance(value, str):
        return value.replace(key, "[REDACTED]") if key else value
    if isinstance(value, list):
        return [scrub(item, key) for item in value]
    if isinstance(value, dict):
        return {scrub(k, key): ("[REDACTED]" if k.lower() in {"desktopurl", "authorization", "token", "secret"}
                                else scrub(v, key)) for k, v in value.items()}
    return value


def parse_response(raw):
    try:
        body = json.loads(raw)
    except (ValueError, UnicodeError) as error:
        raise ProbeError("Boat returned non-JSON") from error
    if not isinstance(body, dict) or body.get("ok") is not True:
        raise ProbeError("Boat response is not an ok JSON envelope")
    return body


def sandbox(body):
    item = body.get("sandbox")
    if not isinstance(item, dict) or not isinstance(item.get("id"), str) or not item["id"].startswith("bx_"):
        raise ProbeError("missing sandbox.id in Boat response")
    return item


def command_output(body):
    if body.get("exitCode") != 0 or body.get("timedOut") or body.get("stdoutTruncated") or body.get("stderrTruncated"):
        raise ProbeError("guest command failed, timed out or truncated: " + json.dumps(body))
    try:
        result = json.loads(body["stdout"])
    except (KeyError, TypeError, ValueError) as error:
        raise ProbeError("guest command did not return one JSON object") from error
    if not isinstance(result, dict):
        raise ProbeError("guest command JSON is not an object")
    return result


def retryable_start(error):
    try:
        body = json.loads(error.body)
        code = body.get("code", "")
        detail = body.get("error", {}).get("details", {})
        return ((error.status == 409 and code in {"sandbox_starting", "boat_starting"}) or
                (error.status == 502 and detail.get("retryable") is True and
                 "restoring" in body.get("message", "")))
    except (ValueError, AttributeError):
        return False


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ProbeError("refusing to forward Boat credentials through an HTTP redirect")


class Client:
    def __init__(self, key, evidence, checkpoint):
        self.key, self.evidence, self.checkpoint = key, evidence, checkpoint
        self.opener = urllib.request.build_opener(NoRedirect())

    def request(self, method, path, body=None, idempotency=None, confirm=None):
        headers = {"Authorization": "Bearer " + self.key, "Content-Type": "application/json"}
        if idempotency:
            headers["Idempotency-Key"] = idempotency
        if confirm:
            headers["X-Ascii-Confirm-Delete"] = confirm
        data = json.dumps(body).encode() if body is not None else None
        event = {"method": method, "path": path, "at": datetime.now(timezone.utc).isoformat()}
        self.evidence["api"].append(event)
        start = time.monotonic()
        try:
            request = urllib.request.Request(API + path, data=data, headers=headers, method=method)
            try:
                with self.opener.open(request, timeout=40) as response:
                    event["status"] = response.status
                    raw = response.read()
            except urllib.error.HTTPError as error:
                event["status"] = error.code
                raw = error.read()
                event["response"] = scrub(raw.decode(errors="replace"), self.key)
                raise ApiError(error.code, event["response"]) from None
            result = parse_response(raw)
            event["response"] = scrub(result, self.key)
            return result
        except (OSError, ValueError) as error:
            event["error"] = scrub(str(error), self.key)
            raise ProbeError(event["error"]) from None
        finally:
            event["seconds"] = time.monotonic() - start
            self.checkpoint()


def save(path, data, key=""):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as stream:
        json.dump(scrub(data, key), stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


class Probe:
    cargo_seed_manifest: 'Path | None'
    cargo_seed_sha256: 'str | None'
    cargo_seed_archive: 'Path | None'

    def __init__(self, client, data, checkpoint, port=9443, minutes=30):
        self.client, self.data, self.checkpoint, self.port = client, data, checkpoint, port
        self.deadline = time.monotonic() + minutes * 60
        self.minutes = minutes
        self.base = ".boat-probe-" + data["run_id"]
        self.source = Path(__file__).with_name("guest.py").read_bytes()

    def request(self, *args, **kwargs):
        if time.monotonic() >= self.deadline:
            raise ProbeError("measurement wall-clock budget expired; entering cleanup")
        return self.client.request(*args, **kwargs)

    def wait_sandbox(self, sid, states):
        until = min(self.deadline, time.monotonic() + 900)
        while time.monotonic() < until:
            item = sandbox(self.request("GET", "/sandboxes/" + sid))
            if item.get("state") in states and ("archived" not in states or
                                                (item.get("stop") or {}).get("status") == "completed"):
                return item
            if item.get("state") in {"error", "cancelled"}:
                raise ProbeError("sandbox entered failure state: " + json.dumps(item))
            time.sleep(2)
        raise ProbeError("timed out waiting for sandbox " + sid)

    def wait_hydrated(self, sid):
        while True:
            item = sandbox(self.request("GET", "/sandboxes/" + sid))
            if item.get("hydrated") is True:
                return item
            if item.get("state") not in READY:
                raise ProbeError("sandbox stopped or failed during hydration")
            time.sleep(2)

    def execute(self, sid, text, poll_seconds=60):
        body = None
        for attempt in range(20):
            try:
                body = self.request("POST", f"/sandboxes/{sid}/commands",
                                    {"command": text, "detached": True})
                break
            except ApiError as error:
                if not retryable_start(error) or attempt == 19:
                    raise
                time.sleep(2)
        if body is None:
            raise ProbeError("no command-start response")
        process = body.get("processId")
        if type(process) is not int or process < 1:
            raise ProbeError("missing numeric processId for detached command")
        # Boat does not time-limit detached commands; deletion bounds abandoned work.
        until = min(self.deadline, time.monotonic() + poll_seconds + 20)
        while time.monotonic() < until:
            status = self.request("GET", f"/sandboxes/{sid}/commands/{process}?tailBytes=8388608")
            if status.get("status") == "exited":
                return status
            if status.get("status") == "lost":
                raise ProbeError("Boat lost detached command after agent restart")
            time.sleep(2)
        raise ProbeError("timed out polling command " + str(process))

    def guest(self, sid, phase, timeout=90, version="1.85.1", port=None):
        encoded = base64.b64encode(self.source).decode()
        script = self.base + "/guest.py"
        port = self.port if port is None else port
        text = ("cd \"$HOME\" && mkdir -p " + shlex.quote(self.base) + " && printf %s " + shlex.quote(encoded) +
                " | base64 -d > " + shlex.quote(script) + "; python3 " + shlex.quote(script) + " " +
                shlex.quote(phase) + " " + shlex.quote(self.base) + " --port " + str(port) +
                " --toolchain-version " + shlex.quote(version))
        return command_output(self.execute(sid, text, timeout))

    def network(self, item, apply):
        sid = item["id"]
        listening = self.guest(sid, "listener")
        out: dict = {"listener": listening, "firewall_before": self.guest(sid, "firewall-read")}
        if listening.get("ready") is not True:
            out["error"] = "guest loopback TLS readiness failed; no outside/firewall inference"
            return out
        out["before_reapply"] = tcp_probe(item.get("ip"), self.port, listening["certificate_pem"])
        if apply:
            out["firewall_apply"] = self.guest(sid, "firewall-open")
            out["after_reapply"] = tcp_probe(item.get("ip"), self.port, listening["certificate_pem"])
        return out

    def create(self, snapshot_name=None):
        body = {"type": "large", "ttlSeconds": 1800, "noEnv": True}
        if snapshot_name:
            body["from"] = snapshot_name
        token = "boat-probe-" + uuid.uuid4().hex
        pending = {"idempotency": token, "body": body}
        self.data["pending_creates"].append(pending)
        self.checkpoint()
        start = time.monotonic()
        response = self.request("POST", "/sandboxes", body, idempotency=token)
        item = sandbox(response)
        self.data["resources"].append({"kind": "sandbox", "id": item["id"]})
        self.data["pending_creates"].remove(pending)
        self.checkpoint()
        ready = self.wait_sandbox(item["id"], READY)
        return ready, {"seconds_to_ready": time.monotonic() - start, "sandbox": ready}

    def cycle(self, item, records=None, toolchain=False):
        sid = item["id"]
        record = {"ip_before": item.get("ip")}
        (self.data["cycles"] if records is None else records).append(record)
        start = time.monotonic()
        record["stop_response"] = self.request("POST", f"/sandboxes/{sid}/stop", {})
        record["stopped"] = self.wait_sandbox(sid, {"archived"})
        record["stop_seconds"] = time.monotonic() - start
        start = time.monotonic()
        record["resume_response"] = self.request("POST", f"/sandboxes/{sid}/resume", {})
        ready = self.wait_sandbox(sid, READY)
        record["resume_seconds"] = time.monotonic() - start
        record["ip_after"] = ready.get("ip")
        record["ip_changed"] = record["ip_before"] != record["ip_after"]
        record["hydrated"] = self.wait_hydrated(sid)
        record["resume_to_hydrated_seconds"] = time.monotonic() - start
        record["persistence"] = self.guest(sid, "persistence")
        record["resume_to_guest_seconds"] = time.monotonic() - start
        record["persistence_matches"] = record["persistence"].get("sha256") == self.data["guest"]["persistence_sha256"]
        if toolchain:
            record["toolchain"] = self.guest(sid, "toolchain-check", timeout=180)
            record["resume_to_toolchain_seconds"] = time.monotonic() - start
        record["network"] = self.network(ready, apply=True)
        self.checkpoint()
        return ready

    def snapshot_and_clone(self, item):
        sid = item["id"]
        name = "motor-probe-" + self.data["run_id"]
        self.data["resources"].append({"kind": "named-snapshot", "id": name})
        self.checkpoint()
        start = time.monotonic()
        self.request("POST", "/named-snapshots", {"sandboxId": sid, "name": name})
        while True:
            state = self.request("GET", "/named-snapshots/" + name)["snapshot"]
            if state["status"] == "failed":
                raise ProbeError("snapshot save failed: " + json.dumps(state))
            if state["status"] == "ready":
                break
            time.sleep(2)
        self.data["snapshot"] = {"seconds": time.monotonic() - start, "snapshot": state}
        self.request("POST", f"/sandboxes/{sid}/stop", {})
        self.wait_sandbox(sid, {"archived"})
        start = time.monotonic()
        clone, self.data["create_from_snapshot"] = self.create(name)
        self.data["create_from_snapshot"]["hydrated"] = self.wait_hydrated(clone["id"])
        self.data["create_from_snapshot"]["seconds_to_hydrated"] = time.monotonic() - start
        self.data["create_from_snapshot"]["persistence"] = self.guest(clone["id"], "persistence")
        self.data["create_from_snapshot"]["toolchain"] = self.guest(clone["id"], "toolchain-check", timeout=180)
        self.data["create_from_snapshot"]["seconds_to_toolchain"] = time.monotonic() - start
        records = []
        self.data["toolchain_cycles"] = records
        self.cycle(clone, records, toolchain=True)
        self.checkpoint()

    def run(self):
        item, self.data["create"] = self.create()
        if item.get("vcpu") != 8 or item.get("memoryGB") != 16:
            raise ProbeError("provider did not report requested large sizing (8 vCPU, 16 GB)")
        self.data["guest"] = self.guest(item["id"], "inspect", timeout=180)
        self.data["network_initial"] = self.network(item, apply=True)
        for _ in range(3):
            item = self.cycle(item)
        self.data["medians"] = {key: statistics.median(cycle[key] for cycle in self.data["cycles"])
                                for key in ("stop_seconds", "resume_seconds")}
        installs = []
        self.data["toolchain_installs"] = installs
        for version in ("1.85.1", "1.86.0", "1.87.0"):
            installs.append(self.guest(item["id"], "toolchain-install", timeout=600, version=version))
            self.checkpoint()
        self.data["toolchain"] = self.guest(item["id"], "toolchain")
        self.data["toolchain"]["few_gb_installed"] &= all(
            step.get("exit_code") == 0 for install in installs for step in install["steps"])
        self.checkpoint()
        self.snapshot_and_clone(item)


def cleanup(client, data, checkpoint, attempts=3, pause=time.sleep):
    """Delete every resource even if one fails; a stop keeps resumable data, not deleted data."""
    data["cleanup_complete"] = False
    for pending in list(data["pending_creates"]):
        for attempt in range(attempts):
            try:
                started = datetime.fromisoformat(data["started_at"])
                if (datetime.now(timezone.utc) - started).total_seconds() >= 23 * 3600:
                    raise ProbeError("create idempotency key nearly expired; manual account inspection required")
                response = client.request("POST", "/sandboxes", pending["body"], idempotency=pending["idempotency"])
                sid = sandbox(response)["id"]
                if not any(r["id"] == sid for r in data["resources"]):
                    data["resources"].append({"kind": "sandbox", "id": sid})
                data["pending_creates"].remove(pending)
                checkpoint()
                break
            except Exception as error:
                data["cleanup"].append({"pending_create": pending, "error": str(error)})
                if attempt + 1 < attempts:
                    pause(2)
    for resource in reversed(data["resources"]):
        if retained_image(resource):
            continue
        if resource.get("deleted"):
            continue
        sid = resource["id"]
        path = ("/sandboxes/" if resource["kind"] == "sandbox" else "/named-snapshots/") + sid
        if resource["kind"] == "sandbox" and "usage" not in resource:
            try:
                resource["usage"] = client.request("GET", path + "/usage")
            except Exception as error:
                resource["usage_error"] = str(error)
        for attempt in range(attempts):
            event = {"kind": resource["kind"], "id": sid, "attempt": attempt + 1}
            data["cleanup"].append(event)
            try:
                if resource["kind"] == "sandbox":
                    if not resource.get("deletion_operation"):
                        response = client.request("DELETE", path, confirm=sid)
                        resource["deletion_operation"] = response["operation"]["id"]
                        checkpoint()
                    operation = resource["deletion_operation"]
                    # 202 and a hidden sandbox are not proof of completed data deletion.
                    for _ in range(30):
                        response = client.request("GET", "/deletion-operations/" + operation)
                        event["operation"] = response["operation"]
                        resource["deletion_state"] = response["operation"]
                        if response["operation"]["status"] == "completed":
                            resource["deleted"] = True
                            break
                        if response["operation"].get("stage") == "waiting_for_uploads":
                            event["purge_pending"] = True
                            break
                        pause(2)
                    if event.get("purge_pending"):
                        break
                    if not resource.get("deleted"):
                        raise ProbeError("deletion accepted but not completed; retry --cleanup")
                else:
                    response = client.request("DELETE", path)
                    if response.get("status") != "deleted":
                        raise ProbeError("named snapshot delete was not confirmed")
                    resource["deleted"] = True
                event["deleted"] = True
                break
            except ApiError as error:
                if error.status == 404 and not resource.get("deletion_operation"):
                    resource["deleted"] = True
                    event["verified_absent"] = True
                    break
                event["error"] = str(error)
                if resource["kind"] == "named-snapshot" and error.status == 409:
                    for _ in range(90):
                        try:
                            snapshot_state = client.request("GET", path)["snapshot"]["status"]
                            if snapshot_state != "saving":
                                break
                        except Exception:
                            break
                        pause(2)
            except Exception as error:
                event["error"] = str(error)
            finally:
                checkpoint()
            if attempt + 1 < attempts:
                pause(2)
        if not resource.get("deleted") and resource["kind"] == "sandbox" and not resource.get("deletion_operation"):
            try:
                resource["fallback_stop"] = client.request("POST", path + "/stop", {})
            except Exception as error:
                resource["fallback_stop_error"] = str(error)
    retained = [r for r in data['resources'] if retained_image(r) and not r.get('deleted')]
    data['operator_retained_resources'] = retained
    data["cleanup_complete"] = not data["pending_creates"] and all(r.get("deleted") or retained_image(r) for r in data["resources"])
    data["cleanup_state"] = cleanup_state(data)
    prepared = data.get('runner_host', {}).get('prepared_image')
    if prepared:
        for resource in data['resources']:
            if resource['kind'] == 'named-snapshot' and resource['id'] == prepared['name']:
                prepared.update(deleted=bool(resource.get('deleted')), operator_retained=retained_image(resource) and not resource.get('deleted'))
    data["cleanup_summary"] = ("All created sandboxes and named snapshots deleted." if data["cleanup_complete"] else
                                PURGE_PENDING_SUMMARY if data["cleanup_state"] == "purge_pending" else
                                "CLEANUP INCOMPLETE: inspect API errors/deletion operations and retry --cleanup.")
    if retained:
        if data['cleanup_complete']:
            data['cleanup_summary'] = 'All non-retained execution resources deleted.'
        data['cleanup_summary'] += f" {len(retained)} prepared image(s) explicitly retained by the operator; not deleted."
    checkpoint()


PURGE_PENDING_SUMMARY = ("Deletion accepted for every remaining sandbox; Boat's final data purge awaits upload URL "
                         "expiry (expectedBy). No action is required; --cleanup after expectedBy confirms the purge.")


def retained_image(resource):
    return resource["kind"] == "named-snapshot" and resource.get("operator_retained") is True


def cleanup_state(data):
    """Classify cleanup separately from the measurement verdict.

    complete: every resource is deleted or an operator-retained image.
    purge_pending: the rest are sandboxes whose deletion Boat accepted and which only wait
    for upload URL expiry; nothing is left running and retrying cannot speed this up.
    incomplete: a create may be unrecorded or a resource may still exist; retry --cleanup.
    """
    if data.get("pending_creates"):
        return "incomplete"
    pending = False
    for resource in data.get("resources", []):
        if resource.get("deleted") or retained_image(resource):
            continue
        stage = (resource.get("deletion_state") or {}).get("stage")
        if resource["kind"] == "sandbox" and resource.get("deletion_operation") and stage == "waiting_for_uploads":
            pending = True
            continue
        return "incomplete"
    return "purge_pending" if pending else "complete"


def required_checks_passed(checks):
    # An unproven provider reclaim is an observation gap, not a Motor failure.
    # Parity, storage and every other check still require a positive PASS.
    return bool(checks) and all(
        row.get('status') == 'PASS' or (row.get('status') == 'INCONCLUSIVE'
                                      and row.get('name', '').endswith('.idle_reclaim_observed'))
        for row in checks)


def verdict(data):
    """Require all required checks to pass, listing unproven idle-reclaim checks separately."""
    reasons = []
    inconclusive = []
    if not data.get("measurements_complete"):
        reasons.append("measurements incomplete")
    if data.get("error"):
        reasons.append("error recorded")
    for mode in ("ssh_reach", "runner_host"):
        if mode in data:
            checks = data[mode].get('checks', [])
            inconclusive.extend(mode + '.' + row['name'] for row in checks if row.get('status') == 'INCONCLUSIVE')
            if data[mode].get("all_passed") is not True:
                if mode != 'runner_host' or not required_checks_passed(checks):
                    reasons.append(mode + " checks did not all pass")
    result = {"passed": not reasons, "reasons": reasons}
    if inconclusive:
        result['inconclusive'] = inconclusive
    return result


# Exit statuses. Cleanup that Boat accepted but has not purged yet does not fail a run.
EXIT_PASSED, EXIT_FAILED, EXIT_CLEANUP_INCOMPLETE = 0, 1, 3


def exit_status(data, cleanup_only=False):
    """3 whenever resources may still exist (they cost money and need --cleanup), else the verdict."""
    if cleanup_state(data) == "incomplete":
        return EXIT_CLEANUP_INCOMPLETE
    if cleanup_only:
        return EXIT_PASSED
    return EXIT_PASSED if verdict(data)["passed"] else EXIT_FAILED


TEMPLATE = """# Boat runner measurement findings

Run: {{run_id}}; started: {{started_at}}; finished: {{finished_at}}.
Evidence: the accompanying `results.json` (API responses and guest stdout/stderr).
Measurement completion: {{measurements_complete}}. Error: {{error}}.

## Timing (seconds, measured from the outside)

{{timing_table}}

Ready means Boat reported ready/idle/running, not full disk hydration. Guest and
toolchain timings wait for full hydration and include command dispatch and polling. Medians use only the
three fresh-sandbox cycles. No guest-supplied usage is used for billing.

## Evidence to inspect

| Question | JSON evidence |
| --- | --- |
| Identity, uid, sudo, capabilities | `guest.uid`, `guest.privilege_prefix`, `guest.checks.identity`, `guest.checks.capabilities_*` |
| Kernel, PID 1, systemd, cgroup v2 and writable controllers | `guest.checks.kernel_init`, `systemctl`, `systemd_run`, `cgroup` |
| User/mount/PID/network namespaces, KVM | `guest.checks.namespace_*`, `guest.checks.kvm` |
| Raw IP TCP + our TLS, outbound, firewall before/after resume | `network_initial`, `cycles[].network`, `guest.checks.egress` |
| IP changes | `cycles[].ip_before`, `ip_after`, `ip_changed` |
| Disk size/filesystem/FICLONE and persisted marker | `guest.checks.disk`, `guest.reflink`, `cycles[].persistence_matches` |
| Installed toolchains and actual allocated bytes | `toolchain`, `create_from_snapshot.toolchain`, `toolchain_cycles` |
| Provider usage and cleanup | `resources[].usage`, `cleanup`, `cleanup_complete` |

Installed toolchain bytes: {{toolchain_bytes}}; at least 2 GiB installed: {{few_gb}}.
Public IP pairs: {{ip_pairs}}. Persistence matches: {{persistence}}.
TLS verified echo after initial firewall open: {{initial_tcp}}.
Before firewall reapply after each resume: {{resume_tcp}}.

An outside timeout alone does not establish firewall state: inspect the rule
dumps, loopback readiness and the outside client's IPv4/IPv6 reachability.
This is an environment probe, not a motor-runner token-enforcement test.

## Cleanup and provider usage

{{cleanup_table}}

{{cleanup_summary}}

## SSH-focused checks

{{ssh_table}}

## Operator conclusion (fill only from returned evidence)

- Can the existing systemd job sandbox run unchanged? **Unassessed.**
- Is direct raw TCP with runner-owned TLS reachable from ck-motor? **Unassessed.**
- Broker must reapply firewall rules after resume? **Unassessed.**
- Few-GB named-snapshot create/resume acceptable? **Unassessed.**
- Limits / follow-up: **Unassessed.**
"""


def display(value):
    return json.dumps(value, ensure_ascii=True).replace("|", "\\|").replace("\n", " ")


def cleanup_section(data):
    """The provider cleanup table and its summary line, shared by every report kind."""
    rows = ["| Resource | Fully purged | Deletion status / stage / expectedBy | Boat usage (seconds / dollars) |",
            "| --- | --- | --- | --- |"]
    purge_pending = False
    for r in data.get("resources", []):
        usage = r.get("usage", {})
        operation = r.get("deletion_state", {})
        if not operation:
            operation = next((event["operation"] for event in reversed(data.get("cleanup", []))
                              if event.get("id") == r["id"] and "operation" in event), {})
        purge_pending |= operation.get("stage") == "waiting_for_uploads" and not r.get("deleted", False)
        purged = "retained by operator" if retained_image(r) and not r.get("deleted") else display(r.get("deleted", False))
        rows.append(f"| {r['kind']} {r['id']} | {purged} | "
                    f"{display([operation.get('status'), operation.get('stage'), operation.get('expectedBy')])} | "
                    f"{display([usage.get('seconds'), usage.get('dollars')])} |")
    summary = display(data.get("cleanup_summary", "unmeasured"))
    if purge_pending:
        summary = "Deletion accepted; final data purge awaits upload URL expiry (see expectedBy above)."
    return "\n".join(rows), summary


def render(data, path):
    text = render_runner_host(data) if "runner_host" in data else render_environment(data)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def render_environment(data):
    cycles = data.get("cycles", [])
    timing = [("Fresh create to Boat-ready", data.get("create", {}).get("seconds_to_ready"))]
    timing.extend((f"Cycle {n + 1}: stop / resume / guest", [c.get("stop_seconds"), c.get("resume_seconds"),
                                                          c.get("resume_to_guest_seconds")]) for n, c in enumerate(cycles))
    timing += [("Fresh-cycle medians", data.get("medians")),
               ("Named snapshot save", data.get("snapshot", {}).get("seconds")),
               ("Snapshot create to ready / toolchain", [data.get("create_from_snapshot", {}).get("seconds_to_ready"),
                                                         data.get("create_from_snapshot", {}).get("seconds_to_toolchain")]),
               ("Snapshot clone stop/resume/toolchain", [{k: c.get(k) for k in ("stop_seconds", "resume_seconds", "resume_to_guest_seconds", "resume_to_toolchain_seconds")}
                                                for c in data.get("toolchain_cycles", [])])]
    rows = ["| Measurement | Seconds |", "| --- | --- |"]
    rows.extend(f"| {name} | {display(value)} |" for name, value in timing)
    cleanup_table, cleanup_summary = cleanup_section(data)
    fields = {name: display(data.get(name, "unmeasured")) for name in
              ("run_id", "started_at", "finished_at", "measurements_complete", "error")}
    fields["error"] = display(data["error"]) if data.get("error") is not None else "none recorded"
    fields["cleanup_summary"] = cleanup_summary
    ssh = data.get("ssh_reach")
    fields["ssh_table"] = "Not requested."
    if ssh:
        ssh_rows = ["| Step | Result | Seconds to result / echo |", "| --- | --- | --- |"]
        for check in ssh.get("checks", []):
            evidence = check["evidence"]
            elapsed = evidence.get("seconds_to_echo", evidence.get("seconds"))
            ssh_rows.append(f"| {check['name']} | {check['status']} | {display(elapsed)} |")
        fields["ssh_table"] = "\n".join(ssh_rows) + "\n\nFull evidence: `ssh_reach.checks` and `ssh_reach.phases`."
    fields.update(timing_table="\n".join(rows), cleanup_table=cleanup_table,
                  toolchain_bytes=display(data.get("toolchain", {}).get("installed_bytes", "unmeasured")),
                  few_gb=display(data.get("toolchain", {}).get("few_gb_installed", "unmeasured")),
                  ip_pairs=display([[c.get("ip_before"), c.get("ip_after")] for c in cycles]),
                  persistence=display([c.get("persistence_matches") for c in cycles]),
                  initial_tcp=display(data.get("network_initial", {}).get("after_reapply", {})),
                  resume_tcp=display([c.get("network", {}).get("before_reapply", {}) for c in cycles]))
    text = TEMPLATE
    for name, value in fields.items():
        text = text.replace("{{" + name + "}}", value)
    return text


def job_evidence(phase):
    """Exit code, wall seconds and parsed isolation payload for one runner-host phase, tolerating gaps."""
    def code(name):
        return (phase.get(name, {}).get("result") or {}).get("code", "unmeasured")
    try:
        isolation = json.loads(phase["isolation"]["result"]["stdout"])
    except (KeyError, TypeError, ValueError):
        isolation = {}
    seconds = {name: phase.get(name, {}).get("seconds", "unmeasured") for name in ("exit", "rust", "isolation")}
    return code, isolation, seconds


def rounded(value, digits):
    if value is None:
        return "unmeasured"
    return round(value, digits) if isinstance(value, float) else value


def mib(value):
    return f"{value / (1 << 20):.0f}" if isinstance(value, (int, float)) else "unmeasured"


def render_runner_host(data):
    """Findings for --runner-host: the verdict, per-phase jobs, timings and job memory budget."""
    runner = data["runner_host"]
    checks = runner.get("checks", [])
    failing = [c["name"] for c in checks if c.get("status") not in ('PASS', 'INCONCLUSIVE')]
    outcome = data.get("verdict") or verdict(data)
    error = display(data["error"]) if data.get("error") is not None else "none recorded"
    status = data.get("exit_status")
    status_text = (f"Exit status: {status}" if status is not None else
                   f"Exit status under the current rules: {exit_status(data)} (the run did not record one)")
    lines = ["# Boat runner-host findings", "",
             f"Run: {display(data.get('run_id'))}; started: {display(data.get('started_at'))}; "
             f"finished: {display(data.get('finished_at'))}.",
             f"Source revision: {display(runner.get('source_revision', 'unmeasured'))}.",
             "Evidence: the accompanying `results.json` (API responses, guest output, raw job terminals).", "",
             "## Verdict", "",
             f"Runner-host verdict: **{'PASSED' if outcome['passed'] else 'FAILED'}**. "
              f"Checks: {sum(c.get('status') == 'PASS' for c in checks)} of {len(checks)} passed.",
              f"Failing checks: {display(failing) if failing else 'none'}.",
              f"Inconclusive preconditions: {display(outcome.get('inconclusive')) if outcome.get('inconclusive') else 'none'}.",
             f"Reasons: {display(outcome['reasons']) if outcome['reasons'] else 'none'}.",
             f"Measurement completion: {display(data.get('measurements_complete', False))}. Error: {error}.",
             f"Cleanup state (reported separately from the verdict): {display(cleanup_state(data))}. "
             f"{status_text} (0 passed, 1 failed, 3 resources need --cleanup).", "",
             "## Phases", "",
             "Every phase runs real jobs through the motor client over the pinned SSH forward.", "",
             "| Phase | Checks passed | Exit job code | Rust job code | Job seconds (exit / rust / isolation) | "
             "Job user | Job interfaces | memory.max | cpu.max | pids.max | nproc | PID namespace shared with host |",
             "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
    for phase in runner.get("phases", []):
        name = phase.get("name", "unnamed")
        own = [c for c in checks if c["name"].startswith(name + ".")]
        code, job, seconds = job_evidence(phase)
        limits = job.get("limits", {})
        host_pid = (phase.get("host") or {}).get("namespaces", {}).get("pid")
        shared = display(job["namespaces"]["pid"] == host_pid) if job.get("namespaces") and host_pid else "unmeasured"
        lines.append(f"| {name} | {sum(c['status'] == 'PASS' for c in own)} of {len(own)} | {display(code('exit'))} | "
                     f"{display(code('rust'))} | {display([rounded(seconds[k], 2) for k in ('exit', 'rust', 'isolation')])} | "
                     f"{display(job.get('user', 'unmeasured'))} | {display(job.get('interfaces', 'unmeasured'))} | "
                     f"{display(limits.get('memory.max', 'unmeasured'))} | {display(limits.get('cpu.max', 'unmeasured'))} | "
                     f"{display(limits.get('pids.max', 'unmeasured'))} | {display(job.get('nproc', 'unmeasured'))} | {shared} |")
    other = [c for c in checks if not any(c["name"].startswith(p.get("name", "") + ".") for p in runner.get("phases", []))]
    lines += ["", "Checks outside a phase: " + (", ".join(f"{c['name']} {c['status']}" for c in other) or "none") + ".",
               "The limit values are read by the job from its own cgroup files; no load is generated.", ""]
    failures = [c for c in checks if c.get('status') == 'FAIL']
    if failures:
        # Keep the recorded verdict; rendering old evidence must not silently
        # reinterpret a failed run using newer probe rules.
        lines += ["## Failed check reasons", "", *[
            f"- `{check['name']}`: {display(check.get('reason', 'not recorded in this report'))}"
            for check in failures], ""]
    lines += ["## Timing (seconds, measured from the operator's Mac)", "",
              "| Measurement | Seconds |", "| --- | --- |"]
    prepared, snapshot = runner.get("prepared_image") or {}, runner.get("snapshot") or {}
    for label, value in (("Create to Boat-ready (first sandbox)", runner.get("create", {}).get("seconds_to_ready")),
                         ("Source upload and recipe install", runner.get("install_seconds")),
                         ("Prepared image save", prepared.get("seconds")),
                         ("Measurement snapshot save", snapshot.get("seconds")),
                         ("Create from measurement snapshot to Boat-ready", runner.get("create_from_snapshot", {}).get("seconds_to_ready")),
                         ("Create from measurement snapshot to hydrated", runner.get("create_from_snapshot", {}).get("seconds_to_hydrated")),
                         ("Create from measurement snapshot to endpoint ready marker", runner.get("snapshot_create_to_endpoint_ready_seconds")),
                         ("Create from measurement snapshot to first finished job", runner.get("snapshot_create_to_first_job_seconds"))):
        lines.append(f"| {label} | {display(rounded(value, 1))} |")
    lines += ["", "First-job time includes waiting for Boat to report full hydration, the endpoint start-up "
              "checks below, SSH and memory budgeting. Prepared-image save is absent when an existing image was reused.", "",
              "## Endpoint start-up", "",
              "Seconds until `cat /run/motor/endpoint-ready` printed the sandbox id, from the broker's identity "
              "install (or from hydration when the identity predates it).", "",
              "| Phase | Order | Ready | Seconds | Polls |", "| --- | --- | --- | --- | --- |"]
    for phase in runner.get("phases", []):
        flows = phase.get("broker_order") or {}
        rows = [("broker order, install before restore finished", flows.get("after_hydration")),
                ("broker order, re-install", flows.get("after_reinstall")),
                ("watcher armed, then install", (phase.get("armed_then_install") or {}).get("wait")),
                ("identity already present", (phase.get("existing_then_reinstall") or {}).get("first")),
                ("re-install while serving", (phase.get("existing_then_reinstall") or {}).get("after"))]
        for order, wait in rows:
            if wait:
                lines.append(f"| {phase.get('name')} | {order} | {display(wait.get('ready'))} | "
                             f"{display(rounded(wait.get('seconds'), 1))} | {display(wait.get('polls'))} |")
    lines += ["",
              "## Job memory budget", "",
              "memory.max = MemAvailable before runner start - runner idle - margin, rounded down to whole MiB.", "",
              "| Phase | MemAvailable bytes | Runner idle bytes | Margin bytes | Budget memory.max bytes | Budget MiB | Job memory.max bytes |",
              "| --- | --- | --- | --- | --- | --- | --- |"]
    budgets = []
    for phase in runner.get("phases", []):
        budget = (phase.get("host") or {}).get("memory_budget", {})
        _, job, _ = job_evidence(phase)
        if isinstance(budget.get("memory_max_bytes"), int):
            budgets.append(budget["memory_max_bytes"])
        lines.append(f"| {phase.get('name')} | " + " | ".join(display(budget.get(k, "unmeasured")) for k in
                     ("mem_available_bytes", "runner_idle_bytes", "margin_bytes", "memory_max_bytes")) +
                     f" | {mib(budget.get('memory_max_bytes'))} | {display(job.get('limits', {}).get('memory.max', 'unmeasured'))} |")
    lines += ["", f"Smallest job memory budget across phases: {mib(min(budgets)) if budgets else 'unmeasured'} MiB.", "",
              "## Snapshots", "",
              "| Role | Name | Snapshot id | Size bytes | Retained | Real jobs qualified |", "| --- | --- | --- | --- | --- | --- |"]
    for role, item in (("prepared source (borrowed, not deleted)", runner.get("prepared_source")),
                       ("prepared image", runner.get("prepared_image")), ("measurement snapshot", runner.get("snapshot"))):
        if item:
            lines.append(f"| {role} | {display(item.get('name'))} | {display(item.get('id'))} | {display(item.get('size_bytes'))} | "
                         f"{display(bool(item.get('operator_retained')))} | {display(item.get('real_jobs_qualified', 'n/a'))} |")
    if prepared.get("delete_command"):
        lines += ["", f"Delete the prepared image (pipe the Boat key reader into stdin): `{prepared['delete_command']}`"]
    cleanup_table, cleanup_summary = cleanup_section(data)
    lines += ["", "## Cleanup and provider usage", "", cleanup_table, "", cleanup_summary, ""]
    return "\n".join(lines)


def read_key(args):
    environment = os.environ.pop("BOAT_API_KEY", "")
    key = sys.stdin.read(16384).strip() if args.key_stdin else environment.strip()
    if not key or len(key) > 8192 or any(c.isspace() for c in key):
        raise ProbeError("supply a single Boat key through --key-stdin or BOAT_API_KEY (not argv)")
    return key


def interrupt(signum, frame):
    raise KeyboardInterrupt("received signal " + str(signum))


def main(report_stream=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--key-stdin", action="store_true")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--cleanup", type=Path, help="retry deletion using a checkpoint, no measurements")
    parser.add_argument("--render", type=Path, help="render existing JSON without contacting Boat")
    parser.add_argument("--write-template", type=Path, help="export the Markdown findings template, offline")
    parser.add_argument("--findings", type=Path, help="Markdown destination (default: beside JSON)")
    parser.add_argument("--max-minutes", type=int, default=30, choices=range(5, 61), metavar="5..60")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--ssh-reach", action="store_true", help="one fresh large; pinned SSH, loopback TLS forwarding and key restrictions")
    mode.add_argument("--runner-prerequisites", action="store_true", help="one fresh large; inspect OS, PrivatePIDs, SSH configuration and KVM without installing tools")
    mode.add_argument("--runner-host", action="store_true", help="install Boat recipe, real motor jobs, snapshot timing; at most 30 minutes")
    parser.add_argument('--prepared-image', action='store_true', help='explicitly retain a prepared named image for future retry reuse')
    parser.add_argument('--from-prepared-image', help='start from an operator-owned prepared image; reapply this recipe and run all checks')
    parser.add_argument('--verify-prepared-image', help='qualify an existing snapshot unchanged: hydrate, install broker identity, real jobs, resume; no recipe reapply or service restart')
    parser.add_argument('--delete-retained-images', action='store_true', help='with --cleanup, also delete explicitly retained prepared images')
    parser.add_argument('--cargo-seed-manifest', type=Path, help='operator server capture, required for runner-host registry parity')
    parser.add_argument('--cargo-seed-sha256', help='independently recorded SHA-256 of the server seed manifest')
    parser.add_argument('--cargo-seed-archive', type=Path, help='tenant-1 private seed; required when applying the recipe')
    parser.add_argument('--acknowledge-tenant-one-private-cache', action='store_true',
                        help='required for runner-host: seed/image contain private fleet dependencies, never for customers')
    args = parser.parse_args()
    if (args.prepared_image or args.from_prepared_image or args.verify_prepared_image) and not args.runner_host:
        parser.error('prepared image options require --runner-host')
    if args.from_prepared_image and not re.fullmatch(r'[a-z0-9][a-z0-9-]{0,62}', args.from_prepared_image):
        parser.error('invalid prepared named-snapshot handle')
    if args.verify_prepared_image:
        if args.prepared_image or args.from_prepared_image:
            parser.error('--verify-prepared-image cannot rebuild or retain an image')
        if not re.fullmatch(r'[a-z0-9][a-z0-9-]{0,62}', args.verify_prepared_image):
            parser.error('invalid prepared named-snapshot handle')
    if args.delete_retained_images and not args.cleanup:
        parser.error('--delete-retained-images requires --cleanup')
    if args.runner_host and not args.cleanup and not args.render:
        if not (args.cargo_seed_manifest and args.cargo_seed_sha256 and args.acknowledge_tenant_one_private_cache):
            parser.error('--runner-host requires --cargo-seed-manifest, --cargo-seed-sha256 and --acknowledge-tenant-one-private-cache')
        if not args.verify_prepared_image and not args.cargo_seed_archive:
            parser.error('recipe application requires --cargo-seed-archive (private tenant-1 fleet only)')
    if args.write_template:
        args.write_template.parent.mkdir(parents=True, exist_ok=True)
        args.write_template.write_text(TEMPLATE)
        print("Wrote findings template: " + str(args.write_template))
        return 0
    if args.render:
        render(json.loads(args.render.read_text()), args.findings or args.render.with_suffix(".md"))
        return 0
    if bool(args.output) == bool(args.cleanup):
        parser.error("choose exactly one of --output DIR or --cleanup RESULTS.json")
    key = read_key(args)
    if args.cleanup:
        path = args.cleanup
        data = json.loads(path.read_text())
        if args.delete_retained_images:
            for resource in data['resources']:
                resource.pop('operator_retained', None)
    else:
        args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
        path = args.output / "results.json"
        data = {"schema": 1, "run_id": uuid.uuid4().hex, "started_at": datetime.now(timezone.utc).isoformat(),
                "api": [], "resources": [], "pending_creates": [], "cycles": [], "cleanup": []}
    checkpoint = lambda: save(path, data, key)
    client = Client(key, data, checkpoint)
    signal.signal(signal.SIGTERM, interrupt)
    try:
        if not args.cleanup:
            runner = Probe(client, data, checkpoint, minutes=args.max_minutes)
            runner.results_path = path.resolve()
            runner.retain_prepared_image = args.prepared_image
            runner.from_prepared_image = args.from_prepared_image
            runner.verify_prepared_image = args.verify_prepared_image
            runner.cargo_seed_manifest = args.cargo_seed_manifest
            runner.cargo_seed_sha256 = args.cargo_seed_sha256
            runner.cargo_seed_archive = args.cargo_seed_archive
            if args.runner_host:
                from runner_host import RunnerHost
                RunnerHost(runner).run()
            elif args.runner_prerequisites:
                from runner_prerequisites import run
                run(runner)
            elif args.ssh_reach:
                from ssh_reach import SSHReach
                SSHReach(runner).run()
            else:
                runner.run()
            data["measurements_complete"] = True
    except (Exception, KeyboardInterrupt) as error:
        data["error"] = scrub(str(error), key)
    finally:
        # A second signal must not interrupt deletion of resources already created.
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        cleanup(client, data, checkpoint)
        data["finished_at"] = datetime.now(timezone.utc).isoformat()
        if not args.cleanup:
            data["verdict"] = verdict(data)
        data["exit_status"] = exit_status(data, cleanup_only=bool(args.cleanup))
        checkpoint()
    render(scrub(data, key), args.findings or path.with_suffix(".md"))
    print(json.dumps(scrub(data, key), indent=2, allow_nan=False), file=report_stream or sys.stdout)
    return data["exit_status"]


def entrypoint():
    output = sys.stdout
    offline = any(arg in sys.argv for arg in ('--help', '-h', '--render', '--write-template'))
    redirect = contextlib.nullcontext() if offline else contextlib.redirect_stdout(sys.stderr)
    try:
        with redirect:
            return main(report_stream=output)
    except SystemExit as error:
        if error.code == 0:
            return 0
        failure = {'schema':1, 'error':'invalid probe arguments', 'measurements_complete':False, 'cleanup_complete':False}
    except (Exception, KeyboardInterrupt) as error:
        failure = {'schema':1, 'error':'probe failed before producing its report', 'error_type':type(error).__name__,
                   'measurements_complete':False, 'cleanup_complete':False}
    print(json.dumps(failure, allow_nan=False), file=output)
    return 1


if __name__ == "__main__":
    raise SystemExit(entrypoint())
