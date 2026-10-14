# motor-vm-guest

`motor-vm-guest` is the supervisor that runs as root inside a Firecracker guest
image and executes exactly one command per VM, then reports a clean status. No
runner backend uses it in this build.

[docs/linux-microvm.md](../../docs/linux-microvm.md#motor-vm-guest) describes
its wire schema, image requirements and tests.
