//! Load the RNG DLL after executable startup so DLL initialization failures can
//! be recorded as LoadLibraryExW errors instead of preventing main from running.
use super::{session, step, wide};
use serde_json::{json, Value};
use std::{fs, io, path::Path, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, GetLastError, SetLastError, HMODULE},
    System::{
        Diagnostics::Debug::OutputDebugStringW,
        LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32},
    },
};
struct Library(HMODULE);
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            FreeLibrary(self.0);
        }
    }
}

pub(super) fn probe(mode: &str, output: &Path) -> io::Result<()> {
    let (dll, symbol) = match mode {
        "rng-process-prng" => ("bcryptprimitives.dll", b"ProcessPrng\0".as_slice()),
        "rng-bcrypt-gen-random" => ("bcrypt.dll", b"BCryptGenRandom\0".as_slice()),
        _ => return Err(step("rng-probe", "unknown RNG mode")),
    };
    let mut proof = json!({"diagnostic_only":true,"mode":mode,"library":dll,"api":String::from_utf8_lossy(&symbol[..symbol.len()-1]),
        "bytes_requested":32,"random_bytes_logged":false,"cwd":std::env::current_dir()?,"context":session::probe_context()?});
    marker(&format!("{mode}: begin LoadLibraryExW({dll})"));
    let library =
        unsafe { LoadLibraryExW(wide(dll).as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if library.is_null() {
        let code = unsafe { GetLastError() };
        marker(&format!("{mode}: LoadLibraryExW failed Win32 0x{code:08X}"));
        proof["phase"] = json!("LoadLibraryExW(System32)");
        proof["success"] = json!(false);
        proof["win32_error"] = json!(format!(
            "0x{code:08X} ({code}): {}",
            io::Error::from_raw_os_error(code as i32)
        ));
    } else {
        let library = Library(library);
        let function = unsafe { GetProcAddress(library.0, symbol.as_ptr()) };
        if let Some(function) = function {
            let mut bytes = [0u8; 32];
            proof["phase"] = json!("rng-call");
            marker(&format!("{mode}: begin direct RNG call"));
            if mode == "rng-process-prng" {
                let call: unsafe extern "system" fn(*mut u8, usize) -> i32 =
                    unsafe { std::mem::transmute(function) };
                unsafe {
                    SetLastError(0);
                }
                let result = unsafe { call(bytes.as_mut_ptr(), bytes.len()) };
                let last_error = unsafe { GetLastError() };
                marker(&format!("{mode}: ProcessPrng BOOL={result}"));
                proof["return_type"] = json!("BOOL");
                proof["bool_result"] = json!(result);
                proof["success"] = json!(result != 0);
                // ProcessPrng reports BOOL, not NTSTATUS. Preserve LastError as
                // supplementary data without claiming the API promises to set it.
                proof["last_error_unpromised"] = json!(format!("0x{last_error:08X}"));
            } else {
                let call: unsafe extern "system" fn(
                    *mut std::ffi::c_void,
                    *mut u8,
                    u32,
                    u32,
                ) -> i32 = unsafe { std::mem::transmute(function) };
                let status = unsafe {
                    call(
                        null_mut(),
                        bytes.as_mut_ptr(),
                        bytes.len() as u32,
                        0x00000002,
                    )
                };
                marker(&format!(
                    "{mode}: BCryptGenRandom NTSTATUS=0x{:08X}",
                    status as u32
                ));
                proof["return_type"] = json!("NTSTATUS");
                proof["ntstatus"] = json!(format!("0x{:08X}", status as u32));
                proof["flags"] = json!("0x00000002 BCRYPT_USE_SYSTEM_PREFERRED_RNG");
                proof["success"] = json!(status >= 0);
            }
            for byte in &mut bytes {
                unsafe {
                    std::ptr::write_volatile(byte, 0);
                }
            }
        } else {
            let code = unsafe { GetLastError() };
            proof["phase"] = json!("GetProcAddress");
            proof["success"] = json!(false);
            proof["win32_error"] = json!(format!("0x{code:08X} ({code})"));
        }
    }
    fs::write(
        output,
        serde_json::to_vec_pretty::<Value>(&proof).map_err(io::Error::other)?,
    )
}
fn marker(text: &str) {
    unsafe {
        OutputDebugStringW(wide(&format!("Motor RNG probe {text}")).as_ptr());
    }
}
