#!/usr/bin/env python3
"""Small synchronous QMP client for this VM; the socket is host-local and private."""
import argparse
import json
import socket
import time

SOCKET = "/srv/build/windows-vm/qmp.sock"


def execute(stream, command, arguments=None):
    request = {"execute": command, "id": command}
    if arguments is not None:
        request["arguments"] = arguments
    stream.write((json.dumps(request) + "\n").encode())
    stream.flush()
    while True:
        line = stream.readline()
        if not line:
            raise RuntimeError("QEMU disconnected before replying to " + command)
        reply = json.loads(line)
        if reply.get("id") != command:
            continue
        if "error" in reply:
            raise RuntimeError(str(reply["error"]))
        return reply["return"]


def wait_for_guest_shutdown(stream, client):
    """A socket disconnect or host-side termination is not a clean guest shutdown."""
    deadline = time.monotonic() + 165
    while True:
        client.settimeout(max(0.1, deadline - time.monotonic()))
        line = stream.readline()
        if not line:
            raise RuntimeError("QEMU disconnected without a guest SHUTDOWN event")
        event = json.loads(line)
        if event.get("event") == "SHUTDOWN":
            data = event.get("data", {})
            if data.get("guest") is not True or data.get("reason") != "guest-shutdown":
                raise RuntimeError("QEMU shutdown was not initiated by the guest: " + str(data))
            return
        if time.monotonic() >= deadline:
            raise TimeoutError("Guest did not shut down; inspect before restarting")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("status", "shutdown", "screenshot", "enter"))
    args = parser.parse_args()
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(165)
        client.connect(SOCKET)
        with client.makefile("rwb") as stream:
            greeting = json.loads(stream.readline())
            if "QMP" not in greeting:
                raise RuntimeError("Missing QMP greeting")
            execute(stream, "qmp_capabilities")
            if args.action == "status":
                print(json.dumps(execute(stream, "query-status")))
            elif args.action == "enter":
                print(execute(stream, "human-monitor-command", {"command-line": "sendkey ret"}))
            elif args.action == "screenshot":
                execute(stream, "screendump", {"filename": "/srv/build/windows-vm/screen.ppm"})
                print("/srv/build/windows-vm/screen.ppm")
            else:
                execute(stream, "system_powerdown")
                wait_for_guest_shutdown(stream, client)
                print("QEMU guest shutdown observed")


if __name__ == "__main__":
    main()
