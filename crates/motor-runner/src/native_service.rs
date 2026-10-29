use motor_runner::Runner;
use std::{
    ffi::OsString,
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher,
};
use windows_sys::Win32::System::Power::{
    GetSystemPowerStatus, SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
    SYSTEM_POWER_STATUS,
};
static STOP: AtomicBool = AtomicBool::new(false);
const NAME: &str = "MotorRunner";
define_windows_service!(service_entry, service_main);
pub(super) fn dispatch() -> io::Result<()> {
    let mut config = super::default_config()?;
    super::configure_options(&mut config, std::env::args().skip(2))?;
    // Install before the Service Control Manager callback can emit events. Ordinary
    // serve uses the same bounded logs/runner.log sink under the instance state
    // directory. No command output is copied into the host service log.
    super::service_logging::init(&config.state_root.join(&config.instance))?;
    tracing::info!("native runner service starting");
    service_dispatcher::start(NAME, service_entry).map_err(io::Error::other)
}
fn service_status(state: ServiceState, code: u32) -> ServiceStatus {
    let stopping = state == ServiceState::StopPending;
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(code),
        checkpoint: if stopping { 1 } else { 0 },
        wait_hint: Duration::from_secs(if stopping { 60 } else { 35 }),
        process_id: None,
    }
}
fn service_main(_: Vec<OsString>) {
    let status_slot = Arc::new(Mutex::new(None::<ServiceStatusHandle>));
    let callback_status = Arc::clone(&status_slot);
    let service_status_handle =
        match service_control_handler::register(NAME, move |event| match event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                // Runner::open can be hashing the parent on a synchronous control call.
                // Publish StopPending and request cancellation before Runner::open returns.
                STOP.store(true, Ordering::Release);
                motor_runner::request_windows_service_stop();
                tracing::info!(
                    service_state = "StopPending",
                    "native runner service received stop control"
                );
                if let Ok(slot) = callback_status.lock() {
                    if let Some(handle) = slot.as_ref() {
                        let _ =
                            handle.set_service_status(service_status(ServiceState::StopPending, 0));
                    }
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }) {
            Ok(handle) => handle,
            Err(_) => return,
        };
    if let Ok(mut slot) = status_slot.lock() {
        *slot = Some(service_status_handle);
    } else {
        return;
    }
    if STOP.load(Ordering::Acquire) {
        if let Ok(slot) = status_slot.lock() {
            if let Some(handle) = slot.as_ref() {
                let _ = handle.set_service_status(service_status(ServiceState::StopPending, 0));
                let _ = handle.set_service_status(service_status(ServiceState::Stopped, 0));
            }
        }
        return;
    }
    if let Ok(slot) = status_slot.lock() {
        if let Some(handle) = slot.as_ref() {
            if handle
                .set_service_status(service_status(ServiceState::Running, 0))
                .is_err()
            {
                return;
            }
        } else {
            return;
        }
    } else {
        return;
    }
    let args = std::iter::once("serve".to_owned())
        .chain(std::env::args().skip(2))
        .collect();
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .and_then(|runtime| runtime.block_on(super::run(args)));
    let stopping = STOP.load(Ordering::Acquire);
    if let Err(error) = &result {
        if !stopping {
            tracing::error!(%error, "native runner service failed");
        }
    }
    let code = if result.is_ok() || stopping { 0 } else { 1 };
    tracing::info!(code, "native runner service stopped");
    if let Ok(slot) = status_slot.lock() {
        if let Some(handle) = slot.as_ref() {
            let _ = handle.set_service_status(service_status(ServiceState::Stopped, code));
        }
    };
}
fn on_ac() -> io::Result<bool> {
    let mut power: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut power) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(power.ACLineStatus == 1)
}
pub(super) fn watch_power_and_stop(runner: Runner) -> io::Result<()> {
    let mut battery_drain = !on_ac()?;
    if battery_drain {
        runner.begin_drain(Duration::ZERO, false);
    }
    // Execution-state requests belong to an OS thread, not a Tokio task that
    // may migrate between workers. This thread also clears its request on exit.
    std::thread::spawn(move || {
        loop {
            if STOP.load(Ordering::Acquire) {
                runner.begin_drain(Duration::from_secs(25), true);
                break;
            }
            let ac = on_ac().unwrap_or(false);
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS | if ac { ES_SYSTEM_REQUIRED } else { 0 });
            }
            if !ac && !runner.is_draining() {
                runner.begin_drain(Duration::from_secs(25), false);
                battery_drain = true;
            }
            if ac && battery_drain {
                runner.resume();
                battery_drain = false;
            }
            if runner.is_draining() && runner.drain_should_exit() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    });
    Ok(())
}
