#!/usr/bin/env python3
"""Type into the provisioning VM's console over its host-local QMP socket.

The Windows job image has no SSH by design, so a trusted operator inspects a
provisioning guest through its console: send keys, then take a screenshot with
control.py. Usage:
  console.py keys esc            press named keys (QEMU qcodes), one after another
  console.py combo meta_l r      press keys together (here: Win+R)
  console.py type 'text'         type text on a US layout, one key at a time
"""
import json
import socket
import sys
import time

from control import SOCKET, execute

SHIFTED = {
    "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6", "&": "7", "*": "8",
    "(": "9", ")": "0", "_": "minus", "+": "equal", "{": "bracket_left",
    "}": "bracket_right", "|": "backslash", ":": "semicolon", '"': "apostrophe",
    "<": "comma", ">": "dot", "?": "slash", "~": "grave_accent",
}
PLAIN = {
    " ": "spc", "-": "minus", "=": "equal", "[": "bracket_left", "]": "bracket_right",
    "\\": "backslash", ";": "semicolon", "'": "apostrophe", ",": "comma", ".": "dot",
    "/": "slash", "`": "grave_accent", "\n": "ret", "\t": "tab",
}


def qcodes(char):
    if char.isalpha() and char.isascii():
        return (["shift"] if char.isupper() else []) + [char.lower()]
    if char.isdigit():
        return [char]
    if char in PLAIN:
        return [PLAIN[char]]
    if char in SHIFTED:
        return ["shift", SHIFTED[char]]
    raise SystemExit(f"cannot type {char!r}")


def press(stream, keys):
    execute(stream, "send-key", {"keys": [{"type": "qcode", "data": key} for key in keys]})
    time.sleep(0.04)


def main():
    action, args = sys.argv[1], sys.argv[2:]
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(30)
        client.connect(SOCKET)
        with client.makefile("rwb") as stream:
            if "QMP" not in json.loads(stream.readline()):
                raise RuntimeError("Missing QMP greeting")
            execute(stream, "qmp_capabilities")
            if action == "keys":
                for key in args:
                    press(stream, [key])
            elif action == "combo":
                press(stream, args)
            elif action == "type":
                for char in " ".join(args):
                    press(stream, qcodes(char))
            else:
                raise SystemExit("action must be keys, combo or type")


if __name__ == "__main__":
    main()
