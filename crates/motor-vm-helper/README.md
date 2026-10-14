# motor-vm-helper

`motor-vm-helper` is a Linux root service that owns a pool of Firecracker VM
slots and hands the unprivileged runner only file descriptors. No runner
backend uses it in this build: installing it does not change how any runner
instance runs jobs.

[docs/linux-microvm.md](../../docs/linux-microvm.md#motor-vm-helper) describes
its host prerequisites, installation, protocol, restart behavior and tests.
