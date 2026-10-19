#!/usr/bin/env python3
"""Operator-only Boat spill-provider measurements (Python 3.10+, standard library).

Usage: <key reader> | python3 script/boat-probe/probe.py --key-stdin --output DIR
Or set BOAT_API_KEY in the parent environment; never pass the key as an argument.
DIR must be new. JSON evidence is checkpointed there and printed after cleanup.
Recover an interrupted run: same credentials, --cleanup DIR/results.json.
Render evidence without credentials: --render DIR/results.json --findings FILE.
"""
from __future__ import annotations

import argparse
import base64
from datetime import datetime, timezone
import json
import os
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
    data["cleanup_complete"] = not data["pending_creates"] and all(r.get("deleted") for r in data["resources"])
    data["cleanup_summary"] = ("All created sandboxes and named snapshots deleted." if data["cleanup_complete"] else
                               "CLEANUP INCOMPLETE: inspect API errors/deletion operations; an accepted deletion may await upload URL expiry.")
    checkpoint()


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


def render(data, path):
    def display(value):
        return json.dumps(value, ensure_ascii=True).replace("|", "\\|").replace("\n", " ")

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
    cleanup_rows = ["| Resource | Fully purged | Deletion status / stage / expectedBy | Boat usage (seconds / dollars) |",
                    "| --- | --- | --- | --- |"]
    purge_pending = False
    for r in data.get("resources", []):
        usage = r.get("usage", {})
        operation = r.get("deletion_state", {})
        if not operation:
            operation = next((event["operation"] for event in reversed(data.get("cleanup", []))
                              if event.get("id") == r["id"] and "operation" in event), {})
        purge_pending |= operation.get("stage") == "waiting_for_uploads" and not r.get("deleted", False)
        cleanup_rows.append(f"| {r['kind']} {r['id']} | {display(r.get('deleted', False))} | "
                            f"{display([operation.get('status'), operation.get('stage'), operation.get('expectedBy')])} | "
                            f"{display([usage.get('seconds'), usage.get('dollars')])} |")
    fields = {name: display(data.get(name, "unmeasured")) for name in
              ("run_id", "started_at", "finished_at", "measurements_complete", "error", "cleanup_summary")}
    fields["error"] = display(data["error"]) if data.get("error") is not None else "none recorded"
    if purge_pending:
        fields["cleanup_summary"] = "Deletion accepted; final data purge awaits upload URL expiry (see expectedBy above)."
    ssh = data.get("ssh_reach")
    fields["ssh_table"] = "Not requested."
    if ssh:
        ssh_rows = ["| Step | Result | Seconds to result / echo |", "| --- | --- | --- |"]
        for check in ssh.get("checks", []):
            evidence = check["evidence"]
            elapsed = evidence.get("seconds_to_echo", evidence.get("seconds"))
            ssh_rows.append(f"| {check['name']} | {check['status']} | {display(elapsed)} |")
        fields["ssh_table"] = "\n".join(ssh_rows) + "\n\nFull evidence: `ssh_reach.checks` and `ssh_reach.phases`."
    fields.update(timing_table="\n".join(rows), cleanup_table="\n".join(cleanup_rows),
                  toolchain_bytes=display(data.get("toolchain", {}).get("installed_bytes", "unmeasured")),
                  few_gb=display(data.get("toolchain", {}).get("few_gb_installed", "unmeasured")),
                  ip_pairs=display([[c.get("ip_before"), c.get("ip_after")] for c in cycles]),
                  persistence=display([c.get("persistence_matches") for c in cycles]),
                  initial_tcp=display(data.get("network_initial", {}).get("after_reapply", {})),
                  resume_tcp=display([c.get("network", {}).get("before_reapply", {}) for c in cycles]))
    text = TEMPLATE
    for name, value in fields.items():
        text = text.replace("{{" + name + "}}", value)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def read_key(args):
    environment = os.environ.pop("BOAT_API_KEY", "")
    key = sys.stdin.read(16384).strip() if args.key_stdin else environment.strip()
    if not key or len(key) > 8192 or any(c.isspace() for c in key):
        raise ProbeError("supply a single Boat key through --key-stdin or BOAT_API_KEY (not argv)")
    return key


def interrupt(signum, frame):
    raise KeyboardInterrupt("received signal " + str(signum))


def main():
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
    args = parser.parse_args()
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
        checkpoint()
    render(scrub(data, key), args.findings or path.with_suffix(".md"))
    print(json.dumps(scrub(data, key), indent=2))
    if args.cleanup:
        return 0 if data.get("cleanup_complete") else 1
    return 0 if (data.get("measurements_complete") and data.get("cleanup_complete") and not data.get("error") and
                 data.get("ssh_reach", {}).get("all_passed", True) and
                 data.get("runner_host", {}).get("all_passed", True)) else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ProbeError, OSError, ValueError) as error:
        print("boat-probe: " + str(error), file=sys.stderr)
        raise SystemExit(1)
