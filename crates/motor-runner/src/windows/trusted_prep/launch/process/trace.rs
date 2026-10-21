//! Consume native Windows debugger events and loader snaps without external tools.
use super::super::{check, step, Plan};
use serde_json::{json, Value};
use std::{
    fs::OpenOptions,
    io::{self, Write},
    mem::zeroed,
    path::PathBuf,
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, DBG_CONTINUE, DBG_EXCEPTION_NOT_HANDLED, ERROR_SEM_TIMEOUT, HANDLE,
        STATUS_BREAKPOINT,
    },
    Storage::FileSystem::GetFinalPathNameByHandleW,
    System::Diagnostics::Debug::*,
};
#[cfg(target_arch = "x86_64")]
mod native_calls;

pub(super) fn path(plan: &Plan) -> PathBuf {
    let stem = plan.stdout.file_stem().unwrap().to_string_lossy();
    let report = plan.report.file_stem().unwrap().to_string_lossy();
    plan.root
        .join("evidence")
        .join(format!("{report}-{stem}.loader.jsonl"))
}
fn module_path(file: HANDLE) -> Value {
    if file.is_null() || file as isize == -1 {
        return json!({"path":null,"reason":"no debug-event file handle"});
    }
    let mut text = vec![0u16; 32768];
    let n = unsafe { GetFinalPathNameByHandleW(file, text.as_mut_ptr(), text.len() as u32, 0) };
    if n == 0 || n as usize >= text.len() {
        return json!({"path":null,"error":io::Error::last_os_error().to_string()});
    }
    json!({"path":String::from_utf16_lossy(&text[..n as usize])})
}
fn debug_string(process: HANDLE, info: OUTPUT_DEBUG_STRING_INFO) -> Value {
    let bytes = usize::from(info.nDebugStringLength) * if info.fUnicode != 0 { 2 } else { 1 };
    let mut data = vec![0u8; bytes.min(128 * 1024)];
    let mut read = 0;
    let success = unsafe {
        ReadProcessMemory(
            process,
            info.lpDebugStringData.cast(),
            data.as_mut_ptr().cast(),
            data.len(),
            &mut read,
        )
    };
    let read_error = if success == 0 {
        Some(io::Error::last_os_error().to_string())
    } else {
        None
    };
    data.truncate(read.min(data.len()));
    let text = if info.fUnicode != 0 {
        String::from_utf16_lossy(
            &data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(&data).into_owned()
    };
    json!({"text":text.trim_end_matches('\0'),"unicode":info.fUnicode != 0,"read_complete":success != 0,
        "read_error":read_error})
}

pub(super) fn wait(plan: &Plan, process: HANDLE, deadline: Instant) -> io::Result<u32> {
    let path = path(plan);
    super::super::session::write_private(&path, b"", true)?;
    let mut log = OpenOptions::new().append(true).open(&path)?;
    let started = Instant::now();
    let mut seen_breakpoint = false;
    let mut total_bytes = 0;
    let mut truncated = false;
    #[cfg(target_arch = "x86_64")]
    let mut calls = plan
        .diagnostic_native_calls
        .then(|| native_calls::NativeCalls::new(process));
    #[cfg(not(target_arch = "x86_64"))]
    if plan.diagnostic_native_calls {
        return Err(step("native-call-trace", "x64 diagnostic only"));
    }
    loop {
        if Instant::now() >= deadline {
            return Err(step(
                "loader-trace",
                "diagnostic child exceeded its deadline",
            ));
        }
        let mut event: DEBUG_EVENT = unsafe { zeroed() };
        if unsafe { WaitForDebugEventEx(&mut event, 100) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_SEM_TIMEOUT as i32) {
                continue;
            }
            return Err(step("WaitForDebugEventEx", error));
        }
        let mut disposition = DBG_CONTINUE;
        let mut terminal = None;
        let mut instrumentation_error = None;
        let detail = unsafe {
            match event.dwDebugEventCode {
                CREATE_PROCESS_DEBUG_EVENT => {
                    #[cfg(target_arch = "x86_64")]
                    if let Some(calls) = &mut calls {
                        if let Err(error) = calls.thread_created(event.dwThreadId) {
                            instrumentation_error = Some(error);
                        }
                    }
                    let data = event.u.CreateProcessInfo;
                    let image = module_path(data.hFile);
                    if !data.hFile.is_null() && data.hFile as isize != -1 {
                        CloseHandle(data.hFile);
                    }
                    json!({"kind":"create-process","image":image,"base":format!("0x{:X}",data.lpBaseOfImage as usize)})
                }
                LOAD_DLL_DEBUG_EVENT => {
                    let data = event.u.LoadDll;
                    let image = module_path(data.hFile);
                    let mut armed = None;
                    #[cfg(target_arch = "x86_64")]
                    if let Some(calls) = &mut calls {
                        match calls.module_loaded(
                            data.lpBaseOfDll as u64,
                            image["path"].as_str().unwrap_or(""),
                        ) {
                            Ok(value) => armed = value,
                            Err(error) => instrumentation_error = Some(error),
                        }
                    }
                    if !data.hFile.is_null() && data.hFile as isize != -1 {
                        CloseHandle(data.hFile);
                    }
                    json!({"kind":"load-dll","image":image,"base":format!("0x{:X}",data.lpBaseOfDll as usize),"native_call_trace":armed})
                }
                OUTPUT_DEBUG_STRING_EVENT => {
                    let message = debug_string(process, event.u.DebugString);
                    #[cfg(target_arch = "x86_64")]
                    if let Some(calls) = &mut calls {
                        calls.loader_message(message["text"].as_str().unwrap_or(""));
                    }
                    json!({"kind":"loader-debug-string","message":message})
                }
                EXCEPTION_DEBUG_EVENT => {
                    let data = event.u.Exception;
                    let code = data.ExceptionRecord.ExceptionCode;
                    let mut intercepted = None;
                    #[cfg(target_arch = "x86_64")]
                    if let Some(calls) = &mut calls {
                        match calls.exception(
                            event.dwThreadId,
                            code,
                            data.ExceptionRecord.ExceptionAddress as u64,
                        ) {
                            Ok(value) => intercepted = value,
                            Err(error) => {
                                instrumentation_error = Some(error);
                                disposition = DBG_EXCEPTION_NOT_HANDLED;
                            }
                        }
                    }
                    if let Some(record) = intercepted {
                        record
                    } else {
                        if code == STATUS_BREAKPOINT
                            && !seen_breakpoint
                            && instrumentation_error.is_none()
                        {
                            seen_breakpoint = true;
                        } else {
                            disposition = DBG_EXCEPTION_NOT_HANDLED;
                        }
                        json!({"kind":"exception","status":format!("0x{:08X}",code as u32),"first_chance":data.dwFirstChance,
                    "address":format!("0x{:X}",data.ExceptionRecord.ExceptionAddress as usize),
                    "parameters":data.ExceptionRecord.ExceptionInformation[..(data.ExceptionRecord.NumberParameters as usize).min(15)]})
                    }
                }
                CREATE_THREAD_DEBUG_EVENT => {
                    #[cfg(target_arch = "x86_64")]
                    if let Some(calls) = &mut calls {
                        if let Err(error) = calls.thread_created(event.dwThreadId) {
                            instrumentation_error = Some(error);
                        }
                    }
                    json!({"kind":"create-thread"})
                }
                EXIT_THREAD_DEBUG_EVENT => {
                    #[cfg(target_arch = "x86_64")]
                    if let Some(calls) = &mut calls {
                        if let Err(error) = calls.thread_exited(event.dwThreadId) {
                            instrumentation_error = Some(error);
                        }
                    }
                    json!({"kind":"exit-thread"})
                }
                EXIT_PROCESS_DEBUG_EVENT => {
                    let code = event.u.ExitProcess.dwExitCode;
                    terminal = Some(code);
                    json!({"kind":"exit-process","exit_code":code,"exit_status":super::super::process_status::describe(code)})
                }
                other => json!({"kind":"debug-event","event_code":other}),
            }
        };
        // Release every debug stop before file I/O can fail. Process/thread debug
        // handles are managed by Windows after exit events; hFile is ours to close.
        #[cfg(target_arch = "x86_64")]
        if instrumentation_error.is_some() {
            if let Some(calls) = &mut calls {
                let address = if event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT {
                    Some(unsafe { event.u.Exception.ExceptionRecord.ExceptionAddress } as u64)
                } else {
                    None
                };
                if let Err(error) = calls.abort(event.dwThreadId, address) {
                    instrumentation_error = Some(step(
                        "native-call-abort",
                        format!(
                            "{}; rollback: {error}",
                            instrumentation_error.as_ref().unwrap()
                        ),
                    ));
                }
            }
        }
        check("ContinueDebugEvent", unsafe {
            ContinueDebugEvent(event.dwProcessId, event.dwThreadId, disposition)
        })?;
        if let Some(error) = instrumentation_error {
            let record =
                json!({"kind":"native-call-instrumentation-error","error":error.to_string()});
            serde_json::to_writer(&mut log, &record).map_err(io::Error::other)?;
            log.write_all(b"\n")?;
            log.sync_all()?;
            return Err(error);
        }
        if detail["kind"] == "native-call-single-step" {
            continue;
        }
        if total_bytes < 4 * 1024 * 1024 {
            let record = json!({"since_start_ms":started.elapsed().as_millis(),"pid":event.dwProcessId,"tid":event.dwThreadId,"event":detail});
            let bytes = serde_json::to_vec(&record).map_err(io::Error::other)?;
            log.write_all(&bytes)?;
            log.write_all(b"\n")?;
            log.flush()?;
            total_bytes += bytes.len() + 1;
        } else if !truncated {
            log.write_all(b"{\"trace_truncated\":true,\"limit_bytes\":4194304}\n")?;
            log.flush()?;
            truncated = true;
        }
        if let Some(code) = terminal {
            log.sync_all()?;
            return Ok(code);
        }
    }
}
