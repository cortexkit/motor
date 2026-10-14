# motor-vm-loader

`motor-vm-loader` is a static, unprivileged loader that serves a Firecracker
snapshot's memory through `userfaultfd`, in 2 MiB pages. `motor-vm-helper`
starts it inside an empty root. No runner backend uses it in this build.

[docs/linux-microvm.md](../../docs/linux-microvm.md#motor-vm-loader) describes
how to build it, its interface and its tests.
