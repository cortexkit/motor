"""Minimal Ethernet guest for root tests, using the same TAP as Firecracker."""
import fcntl
import json
import os
import select
import socket
import struct
import subprocess
import sys
import time

TAP = "mtap17"
MAC = bytes.fromhex("02fc00001102")
IP = socket.inet_aton("10.200.17.2")
GATEWAY = socket.inet_aton("10.200.17.1")
link = json.loads(subprocess.check_output(["/usr/sbin/ip", "-j", "link", "show", TAP]))[0]
gateway_mac = bytes.fromhex(link["address"].replace(":", ""))
fd = os.open("/dev/net/tun", os.O_RDWR | os.O_NONBLOCK)
fcntl.ioctl(fd, 0x400454CA, struct.pack("16sH", TAP.encode(), 0x1002))


def checksum(data):
    if len(data) % 2:
        data += b"\0"
    value = sum(struct.unpack("!%dH" % (len(data) // 2), data))
    while value >> 16:
        value = (value & 0xFFFF) + (value >> 16)
    return (~value) & 0xFFFF


def send(destination, payload):
    udp = struct.pack("!HHHH", 41000, 42000, 8 + len(payload), 0) + payload
    header = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(udp), 1, 0, 64, 17, 0, IP, socket.inet_aton(destination))
    header = header[:10] + struct.pack("!H", checksum(header)) + header[12:]
    os.write(fd, gateway_mac + MAC + b"\x08\x00" + header + udp)


def receive():
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        if not select.select([fd], [], [], max(0, deadline - time.monotonic()))[0]:
            break
        packet = os.read(fd, 65536)
        if packet[12:14] == b"\x08\x06" and len(packet) >= 42:
            arp = packet[14:42]
            if arp[6:8] == b"\x00\x01" and arp[24:28] == IP:
                reply = arp[:6] + b"\x00\x02" + MAC + IP + arp[8:14] + arp[14:18]
                os.write(fd, arp[8:14] + MAC + b"\x08\x06" + reply)
        elif packet[12:14] == b"\x08\x00" and len(packet) >= 42:
            ihl = (packet[14] & 15) * 4
            offset = 14 + ihl
            if packet[23] == 17 and packet[30:34] == IP and packet[offset + 2:offset + 4] == struct.pack("!H", 41000):
                length = struct.unpack("!H", packet[offset + 4:offset + 6])[0]
                return packet[offset + 8:offset + length].decode()
    return "timeout"


print("ready", flush=True)
for line in sys.stdin:
    parts = line.strip().split(" ", 2)
    if parts[0] == "send":
        send(parts[1], parts[2].encode())
        print(receive(), flush=True)
    elif parts[0] == "receive":
        print(receive(), flush=True)
