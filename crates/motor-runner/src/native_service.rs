use motor_runner::Runner;
use std::{
    ffi::OsString,
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
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
    service_dispatcher::start(NAME, service_entry).map_err(io::Error::other)
}
fn service_main(_: Vec<OsString>) {
    let handler = match service_control_handler::register(NAME, |event| match event {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            STOP.store(true, Ordering::Release);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    }) {
        Ok(handler) => handler,
        Err(_) => return,
    };
    let status = |state, code| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(code),
        checkpoint: 0,
        wait_hint: Duration::from_secs(35),
        process_id: None,
    };
    if handler
        .set_service_status(status(ServiceState::Running, 0))
        .is_err()
    {
        return;
    }
    let args = std::iter::once("serve".to_owned())
        .chain(std::env::args().skip(2))
        .collect();
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .and_then(|runtime| runtime.block_on(super::run(args)));
    if let Err(error) = &result {
        eprintln!("native runner service failed: {error}");
        use std::io::Write;
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(r"C:\ProgramData\MotorRunner\evidence\service-errors.log")
        {
            let _ = writeln!(log, "native runner service failed: {error}");
        }
    }
    let _ = handler.set_service_status(status(
        ServiceState::Stopped,
        if result.is_ok() { 0 } else { 1 },
    ));
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
