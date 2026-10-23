use std::io;

fn main() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        native::run()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "motor-macvm requires macOS",
        ))
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use motor_macvm::*;
    use objc2::{
        AllocAnyThread, DefinedClass, MainThreadOnly, Message, define_class, msg_send,
        rc::Retained, runtime::ProtocolObject,
    };
    use objc2_foundation::{
        NSArray, NSData, NSDate, NSError, NSObject, NSObjectProtocol, NSRunLoop, NSString, NSURL,
    };
    use objc2_virtualization::*;
    use std::cell::RefCell;
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn error(message: impl ToString) -> io::Error {
        io::Error::other(message.to_string())
    }
    fn url(path: &Path) -> io::Result<Retained<NSURL>> {
        let path = path.to_str().ok_or_else(|| error("non-UTF8 VM path"))?;
        Ok(NSURL::fileURLWithPath(&NSString::from_str(path)))
    }
    fn data(path: &Path) -> io::Result<Retained<NSData>> {
        Ok(NSData::with_bytes(&std::fs::read(path)?))
    }

    // Every framework operation stays on the main queue. Only duplicated socket
    // descriptors and plain Rust messages cross into worker threads.
    unsafe fn base_configuration(
        image: &Path,
    ) -> io::Result<Retained<VZVirtualMachineConfiguration>> {
        unsafe {
            let config = VZVirtualMachineConfiguration::new();
            config.setCPUCount(4);
            config.setMemorySize(6 * 1024 * 1024 * 1024);
            config.setBootLoader(Some(&VZMacOSBootLoader::new()));
            let platform = VZMacPlatformConfiguration::new();
            let model = VZMacHardwareModel::initWithDataRepresentation(
                VZMacHardwareModel::alloc(),
                &*data(&image.join(HARDWARE_MODEL))?,
            )
            .ok_or_else(|| error("invalid hardware-model"))?;
            if !model.isSupported() {
                return Err(error("image hardware-model is not supported on this host"));
            }
            let machine = VZMacMachineIdentifier::initWithDataRepresentation(
                VZMacMachineIdentifier::alloc(),
                &*data(&image.join(MACHINE_IDENTIFIER))?,
            )
            .ok_or_else(|| error("invalid machine-identifier"))?;
            platform.setHardwareModel(&model);
            platform.setMachineIdentifier(&machine);
            platform.setAuxiliaryStorage(Some(&VZMacAuxiliaryStorage::initWithURL(
                VZMacAuxiliaryStorage::alloc(),
                &*url(&image.join(AUXILIARY_STORAGE))?,
            )));
            config.setPlatform(&platform);
            Ok(config)
        }
    }

    unsafe fn disk(
        path: &Path,
        read_only: bool,
    ) -> io::Result<Retained<VZStorageDeviceConfiguration>> {
        unsafe {
            let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &*url(path)?,
                read_only,
            )
            .map_err(error)?;
            Ok(VZVirtioBlockDeviceConfiguration::initWithAttachment(
                VZVirtioBlockDeviceConfiguration::alloc(),
                &attachment,
            )
            .into_super())
        }
    }

    unsafe fn job_configuration(
        plan: &JobVmConfig,
    ) -> io::Result<Retained<VZVirtualMachineConfiguration>> {
        unsafe {
            let config = base_configuration(&plan.request().clone)?;
            apply_job_devices(&config, plan)?;
            config.validateWithError().map_err(error)?;
            Ok(config)
        }
    }

    unsafe fn apply_job_devices(
        config: &VZVirtualMachineConfiguration,
        plan: &JobVmConfig,
    ) -> io::Result<()> {
        unsafe {
            let disks: Vec<_> = plan
                .disks()
                .iter()
                .map(|d| disk(&d.path, d.read_only))
                .collect::<io::Result<_>>()?;
            config.setStorageDevices(&NSArray::from_retained_slice(&disks));
            let socket: Retained<VZSocketDeviceConfiguration> =
                VZVirtioSocketDeviceConfiguration::new().into_super();
            config.setSocketDevices(&NSArray::from_retained_slice(&[socket]));
            let mut shares: Vec<Retained<VZDirectorySharingDeviceConfiguration>> = Vec::new();
            for share in plan.shares() {
                let directory = VZSharedDirectory::initWithURL_readOnly(
                    VZSharedDirectory::alloc(),
                    &*url(&share.location.host)?,
                    share.read_only,
                );
                let single = VZSingleDirectoryShare::initWithDirectory(
                    VZSingleDirectoryShare::alloc(),
                    &directory,
                );
                let device = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                    VZVirtioFileSystemDeviceConfiguration::alloc(),
                    &NSString::from_str(&share.location.tag),
                );
                device.setShare(Some(&single));
                shares.push(device.into_super());
            }
            config.setDirectorySharingDevices(&NSArray::from_retained_slice(&shares));
            config.setNetworkDevices(&NSArray::new());
            config.setGraphicsDevices(&NSArray::new());
            Ok(())
        }
    }

    #[derive(Debug)]
    enum Event {
        Gate(GateEvent),
        Started(Result<(), String>),
        Stopped,
        StopError(String),
        Failed(String),
    }

    struct ListenerState {
        acceptor: RefCell<Option<SessionAcceptor>>,
        session: UnixStream,
        events: mpsc::Sender<Event>,
        connection: RefCell<Option<Retained<VZVirtioSocketConnection>>>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "MotorMacVmSocketDelegate"]
        #[ivars = ListenerState]
        struct SocketDelegate;
        unsafe impl NSObjectProtocol for SocketDelegate {}
        unsafe impl VZVirtualMachineDelegate for SocketDelegate {
            #[unsafe(method(guestDidStopVirtualMachine:))]
            unsafe fn guest_stopped(&self, _vm: &VZVirtualMachine) {
                let _ = self.ivars().events.send(Event::Stopped);
            }
            #[unsafe(method(virtualMachine:didStopWithError:))]
            unsafe fn stopped_with_error(&self, _vm: &VZVirtualMachine, error: &NSError) {
                let _ = self.ivars().events.send(Event::Failed(error.to_string()));
                let _ = self.ivars().events.send(Event::Stopped);
            }
        }
        unsafe impl VZVirtioSocketListenerDelegate for SocketDelegate {
            #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
            unsafe fn accept_connection(
                &self,
                _listener: &VZVirtioSocketListener,
                connection: &VZVirtioSocketConnection,
                _device: &VZVirtioSocketDevice,
            ) -> objc2::runtime::Bool {
                let state = self.ivars();
                let Some(mut acceptor) = state.acceptor.borrow_mut().take() else {
                    return false.into();
                };
                if acceptor.claim().is_err() {
                    return false.into();
                }
                let result = (|| -> io::Result<()> {
                    let fd = unsafe {
                        libc::fcntl(connection.fileDescriptor(), libc::F_DUPFD_CLOEXEC, 5)
                    };
                    if fd < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let guest = unsafe { UnixStream::from_raw_fd(fd) };
                    let host = state.session.try_clone()?;
                    let events = state.events.clone();
                    state.connection.replace(Some(connection.retain()));
                    std::thread::spawn(move || {
                        if let Err(e) = proxy(guest, host, acceptor) {
                            let _ = events.send(Event::Failed(e.to_string()));
                        }
                    });
                    Ok(())
                })();
                if let Err(e) = result {
                    let _ = state.events.send(Event::Failed(e.to_string()));
                    return false.into();
                }
                true.into()
            }
        }
    );

    fn proxy(
        mut guest: UnixStream,
        mut host: UnixStream,
        acceptor: SessionAcceptor,
    ) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let flags = unsafe { libc::fcntl(guest.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(guest.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) }
                == -1
        {
            return Err(io::Error::last_os_error());
        }
        guest.set_read_timeout(Some(Duration::from_secs(10)))?;
        let hello = acceptor.receive_hello(&mut guest)?;
        guest.set_read_timeout(None)?;
        motor_winagent::write_frame(&mut host, &hello)?;
        let mut guest_write = guest.try_clone()?;
        let mut host_read = host.try_clone()?;
        std::thread::spawn(move || {
            let _ = io::copy(&mut host_read, &mut guest_write);
            let _ = guest_write.shutdown(std::net::Shutdown::Write);
        });
        io::copy(&mut guest, &mut host)?;
        host.shutdown(std::net::Shutdown::Write)
    }

    fn report(value: HelperReport) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        let mut stdout = io::stdout().lock();
        stdout.write_all(&bytes)?;
        stdout.flush()
    }

    pub fn run() -> io::Result<()> {
        let mut args = std::env::args().skip(1);
        let command = args.next().ok_or_else(|| {
            error("usage: motor-macvm job <request-json> | image-build <image-directory>")
        })?;
        let argument = args
            .next()
            .ok_or_else(|| error("missing subcommand argument"))?;
        let restore = args.next();
        if args.next().is_some() || (command == "job" && restore.is_some()) {
            return Err(error("unexpected subcommand argument"));
        }
        match command.as_str() {
            "job" => run_job(JobVmConfig::new(serde_json::from_str(&argument)?)?),
            "image-build" => unsafe {
                image_build(Path::new(&argument), restore.as_deref().map(Path::new))
            },
            _ => Err(error("unknown subcommand; expected job or image-build")),
        }
    }

    fn run_job(plan: JobVmConfig) -> io::Result<()> {
        // A runner that stops reading status must not prevent gate cancellation.
        let flags = unsafe { libc::fcntl(1, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(1, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let (tx, rx) = mpsc::channel();
        let gate_tx = tx.clone();
        std::thread::spawn(move || {
            let _ = watch_gate(io::stdin().lock(), |event| {
                let _ = gate_tx.send(Event::Gate(event));
            });
        });
        let mut lifecycle = HelperLifecycle::default();
        // Do not even initialize Virtualization.framework until the recorded gate
        // opens. An EOF here proves that this helper never started a VM.
        loop {
            match rx.recv().map_err(error)? {
                Event::Gate(e) => lifecycle.gate(e),
                _ => unreachable!(),
            }
            match lifecycle.next_action() {
                Some(HelperAction::Start) => break,
                Some(HelperAction::Exit) => {
                    return report(HelperReport::Stopped {
                        reason: lifecycle.stop_reason(true, false, false),
                    });
                }
                _ => {}
            }
        }
        let result = unsafe { run_open_job(plan, lifecycle, tx, rx) };
        if let Err(e) = &result {
            let _ = report(HelperReport::Failed {
                detail: e.to_string(),
            });
        }
        result
    }

    unsafe fn run_open_job(
        plan: JobVmConfig,
        mut lifecycle: HelperLifecycle,
        tx: mpsc::Sender<Event>,
        rx: mpsc::Receiver<Event>,
    ) -> io::Result<()> {
        unsafe {
            if libc::fcntl(3, libc::F_GETFD) == -1 || libc::fcntl(4, libc::F_GETFD) == -1 {
                return Err(error(
                    "job helper requires inherited session and slot descriptors",
                ));
            }
            let _slot = File::from_raw_fd(4);
            // fd 4 survives this exec intentionally, but must not leak through
            // any later exec performed by framework code in the helper.
            if libc::fcntl(4, libc::F_SETFD, libc::FD_CLOEXEC) == -1 {
                return Err(io::Error::last_os_error());
            }
            let session = UnixStream::from_raw_fd(3);
            let nonce = read_boot_config(File::open(&plan.request().nonce_disk)?)?.nonce;
            let config = job_configuration(&plan)?;
            let vm = VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &config);
            let state = ListenerState {
                acceptor: RefCell::new(Some(SessionAcceptor::new(
                    nonce,
                    plan.request().clone.display().to_string(),
                )?)),
                session,
                events: tx.clone(),
                connection: RefCell::new(None),
            };
            let delegate: Retained<SocketDelegate> =
                msg_send![super(SocketDelegate::alloc().set_ivars(state)), init];
            let listener = VZVirtioSocketListener::new();
            listener.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            vm.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            let devices = vm.socketDevices();
            let socket = devices
                .objectAtIndex(0)
                .downcast::<VZVirtioSocketDevice>()
                .map_err(|_| error("not a virtio socket device"))?;
            socket.setSocketListener_forPort(&listener, VSOCK_PORT);
            let start_tx = tx.clone();
            vm.startWithCompletionHandler(&block2::RcBlock::new(move |e: *mut NSError| {
                let result = if e.is_null() {
                    Ok(())
                } else {
                    Err((&*e).to_string())
                };
                let _ = start_tx.send(Event::Started(result));
            }));
            let mut started = false;
            let mut start_completed = false;
            let mut service_reported = false;
            let mut started_at = Instant::now();
            let mut failed = false;
            let mut gate_eof = false;
            let mut stop_called = false;
            loop {
                for event in rx.try_iter() {
                    match event {
                        Event::Gate(e) => {
                            gate_eof |= e == GateEvent::Eof;
                            lifecycle.gate(e);
                        }
                        Event::Started(Ok(())) => {
                            started = true;
                            start_completed = true;
                            started_at = Instant::now();
                        }
                        Event::Started(Err(detail)) => {
                            failed = true;
                            let _ = report(HelperReport::Failed { detail });
                            lifecycle.vm_stopped();
                        }
                        Event::Stopped => lifecycle.vm_stopped(),
                        Event::Failed(detail) => {
                            if gate_eof && stop_called {
                                continue;
                            }
                            failed = true;
                            let _ = report(HelperReport::Failed { detail });
                            lifecycle.gate(GateEvent::Eof);
                        }
                        Event::StopError(detail) => {
                            let _ = report(HelperReport::Failed {
                                detail: format!("VM stop failed; retaining helper: {detail}"),
                            });
                        }
                    }
                }
                if started && !service_reported && !failed {
                    let identity = process_identity(std::process::id() as i32).and_then(|helper| {
                        process::identify_service(&plan.request().clone, helper)
                            .map(|service| (helper, service))
                    });
                    match identity {
                        Ok((helper, service)) => {
                            if report(HelperReport::Started { helper, service }).is_err() {
                                lifecycle.gate(GateEvent::Eof);
                            }
                            service_reported = true;
                        }
                        Err(e)
                            if e.kind() == io::ErrorKind::NotFound
                                && started_at.elapsed() < Duration::from_secs(2) => {}
                        Err(e) => {
                            failed = true;
                            let _ = report(HelperReport::Failed {
                                detail: format!("could not identify VM service: {e}"),
                            });
                            lifecycle.gate(GateEvent::Eof);
                        }
                    }
                }
                if start_completed && vm.state() == VZVirtualMachineState::Stopped {
                    lifecycle.vm_stopped();
                }
                match lifecycle.next_action() {
                    Some(HelperAction::Exit) => {
                        report(HelperReport::Stopped {
                            reason: lifecycle.stop_reason(gate_eof, stop_called, failed),
                        })?;
                        return if failed {
                            Err(error("VM job helper failed"))
                        } else {
                            Ok(())
                        };
                    }
                    Some(HelperAction::Stop) if vm.canStop() => {
                        stop_called = true;
                        let stop_tx = tx.clone();
                        vm.stopWithCompletionHandler(&block2::RcBlock::new(
                            move |e: *mut NSError| {
                                let event = if e.is_null() {
                                    Event::Stopped
                                } else {
                                    Event::StopError((&*e).to_string())
                                };
                                let _ = stop_tx.send(event);
                            },
                        ));
                    }
                    _ => {}
                }
                objc2::rc::autoreleasepool(|_| {
                    NSRunLoop::mainRunLoop()
                        .runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.01));
                });
            }
        }
    }

    unsafe fn image_build(image: &Path, restore: Option<&Path>) -> io::Result<()> {
        use objc2_app_kit::{
            NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask,
            NSWindow, NSWindowStyleMask,
        };
        use objc2_foundation::{NSDefaultRunLoopMode, NSPoint, NSRect, NSSize};
        unsafe {
            let config = base_configuration(image)?;
            config.setStorageDevices(&NSArray::from_retained_slice(&[disk(
                &image.join(BOOT_DISK),
                false,
            )?]));
            let network = VZVirtioNetworkDeviceConfiguration::new();
            network.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
            config.setNetworkDevices(&NSArray::from_retained_slice(&[network.into_super()]));
            let graphics = VZMacGraphicsDeviceConfiguration::new();
            let display = VZMacGraphicsDisplayConfiguration::initWithWidthInPixels_heightInPixels_pixelsPerInch(VZMacGraphicsDisplayConfiguration::alloc(), 1920, 1200, 144);
            graphics.setDisplays(&NSArray::from_retained_slice(&[display]));
            config.setGraphicsDevices(&NSArray::from_retained_slice(&[graphics.into_super()]));
            config.setKeyboards(&NSArray::from_retained_slice(&[
                VZUSBKeyboardConfiguration::new().into_super(),
            ]));
            config.setPointingDevices(&NSArray::from_retained_slice(&[
                VZUSBScreenCoordinatePointingDeviceConfiguration::new().into_super(),
            ]));
            config.validateWithError().map_err(error)?;
            let vm = VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &config);
            let mtm = objc2::MainThreadMarker::new()
                .ok_or_else(|| error("image-build must run on the main thread"))?;
            let app = NSApplication::sharedApplication(mtm);
            app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
            let rect = NSRect::new(NSPoint::new(0., 0.), NSSize::new(1280., 800.));
            let window = NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            );
            window.setReleasedWhenClosed(false);
            window.setTitle(&NSString::from_str("Motor image builder (network enabled)"));
            let view = VZVirtualMachineView::initWithFrame(VZVirtualMachineView::alloc(mtm), rect);
            view.setVirtualMachine(Some(&vm));
            window.setContentView(Some(&view));
            window.makeKeyAndOrderFront(None);
            app.finishLaunching();
            let (done_tx, done_rx) = mpsc::channel();
            let completion = block2::RcBlock::new(move |e: *mut NSError| {
                let result = if e.is_null() {
                    Ok(())
                } else {
                    Err((&*e).to_string())
                };
                let _ = done_tx.send(result);
            });
            let installer = if let Some(restore) = restore {
                let installer = VZMacOSInstaller::initWithVirtualMachine_restoreImageURL(
                    VZMacOSInstaller::alloc(),
                    &vm,
                    &*url(restore)?,
                );
                installer.installWithCompletionHandler(&completion);
                Some(installer)
            } else {
                vm.startWithCompletionHandler(&completion);
                None
            };
            let mut operation_done = false;
            loop {
                objc2::rc::autoreleasepool(|_| {
                    let deadline = NSDate::dateWithTimeIntervalSinceNow(0.01);
                    if let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                        NSEventMask::Any,
                        Some(&deadline),
                        NSDefaultRunLoopMode,
                        true,
                    ) {
                        app.sendEvent(&event);
                    }
                    app.updateWindows();
                });
                if let Ok(result) = done_rx.try_recv() {
                    result.map_err(error)?;
                    operation_done = true;
                }
                if !window.isVisible() && vm.canStop() {
                    vm.stopWithCompletionHandler(&block2::RcBlock::new(|_: *mut NSError| {}));
                }
                if operation_done && vm.state() == VZVirtualMachineState::Stopped {
                    drop(installer);
                    return Ok(());
                }
                if vm.state() == VZVirtualMachineState::Error {
                    return Err(error("image-build VM error"));
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn framework_job_devices_match_portable_plan_without_booting_vm() {
            let root = cortexkit_test_support::scratch_root()
                .join(format!("macvm-devices-{}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let location = |name: &str, guest: &str| ShareLocation {
                host: root.join(name),
                tag: name.into(),
                guest: guest.into(),
            };
            let plan = JobVmConfig::new(JobVmRequest {
                clone: root.join("image"),
                nonce_disk: root.join("nonce.raw"),
                workspace: location("workspace", "/Users/job/workspace"),
                siblings: vec![location("sibling", "/Users/job/sibling")],
                cargo_home: location("cargo-home", CARGO_HOME),
            })
            .unwrap();
            std::fs::create_dir_all(root.join("image")).unwrap();
            for disk in plan.disks() {
                File::create(&disk.path)
                    .unwrap()
                    .set_len(1024 * 1024)
                    .unwrap();
            }
            unsafe {
                let config = VZVirtualMachineConfiguration::new();
                apply_job_devices(&config, &plan).unwrap();
                assert_eq!(config.storageDevices().len(), 2);
                assert_eq!(config.socketDevices().len(), 1);
                assert_eq!(config.networkDevices().len(), 0);
                assert_eq!(config.graphicsDevices().len(), 0);
                assert_eq!(config.directorySharingDevices().len(), 3);
                for (index, expected) in plan.disks().iter().enumerate() {
                    let block = config
                        .storageDevices()
                        .objectAtIndex(index)
                        .downcast::<VZVirtioBlockDeviceConfiguration>()
                        .unwrap();
                    let attachment = block
                        .attachment()
                        .downcast::<VZDiskImageStorageDeviceAttachment>()
                        .unwrap();
                    assert_eq!(attachment.isReadOnly(), expected.read_only);
                }
                for (index, expected) in plan.shares().iter().enumerate() {
                    let device = config
                        .directorySharingDevices()
                        .objectAtIndex(index)
                        .downcast::<VZVirtioFileSystemDeviceConfiguration>()
                        .unwrap();
                    assert_eq!(device.tag().to_string(), expected.location.tag);
                    let share = device
                        .share()
                        .unwrap()
                        .downcast::<VZSingleDirectoryShare>()
                        .unwrap();
                    assert_eq!(share.directory().isReadOnly(), expected.read_only);
                }
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
