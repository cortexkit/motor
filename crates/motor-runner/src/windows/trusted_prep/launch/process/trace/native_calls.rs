//! Diagnostic x64 entry/return breakpoints on selected ntdll object APIs.
//! Only the disposable debuggee's private code pages are changed. No hook is
//! installed in the SYSTEM broker or on disk, and no syscall result is altered.
use super::super::super::{check, step, Handle};
use crate::windows::native_call_trace;
use serde_json::{json, Value};
use std::{collections::BTreeMap, ffi::c_void, io, mem::zeroed, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{
        DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, STATUS_BREAKPOINT, STATUS_SINGLE_STEP,
    },
    System::{
        Diagnostics::Debug::*,
        Memory::{VirtualProtectEx, PAGE_EXECUTE_READWRITE},
        Threading::*,
    },
};

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryObject(
        handle: HANDLE,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        needed: *mut u32,
    ) -> i32;
}

#[derive(Clone)]
struct Api {
    name: &'static str,
    object_attributes: bool,
}
struct Breakpoint {
    original: u8,
    api: Option<Api>,
    returns: usize,
}
struct Call {
    api: Api,
    address: u64,
    stack: u64,
    access: Option<u32>,
    target: Value,
    site: Value,
    attach: String,
}
struct Step {
    address: u64,
    suspended: Paused,
}
pub(super) struct NativeCalls {
    process: HANDLE,
    points: BTreeMap<u64, Breakpoint>,
    calls: BTreeMap<u32, Vec<Call>>,
    steps: BTreeMap<u32, Step>,
    threads: BTreeMap<u32, ()>,
    modules: BTreeMap<u64, String>,
    attach: String,
    armed: bool,
}
impl NativeCalls {
    pub(super) fn new(process: HANDLE) -> Self {
        Self {
            process,
            points: BTreeMap::new(),
            calls: BTreeMap::new(),
            steps: BTreeMap::new(),
            threads: BTreeMap::new(),
            modules: BTreeMap::new(),
            attach: String::new(),
            armed: false,
        }
    }
    pub(super) fn thread_created(&mut self, tid: u32) -> io::Result<()> {
        self.threads.insert(tid, ());
        if let Some((owner, step)) = self.steps.iter_mut().next() {
            if *owner != tid {
                step.suspended.hold(tid)?;
            }
        }
        Ok(())
    }
    pub(super) fn thread_exited(&mut self, tid: u32) -> io::Result<()> {
        self.threads.remove(&tid);
        for saved in self.steps.values_mut() {
            saved.suspended.0.remove(&tid);
        }
        let pending = self
            .calls
            .remove(&tid)
            .is_some_and(|calls| !calls.is_empty());
        if let Some(mut step) = self.steps.remove(&tid) {
            step.suspended.release()?;
            return Err(step_error(
                "thread exited during an instrumented single step",
            ));
        }
        if pending {
            return Err(step_error("thread exited with an unfinished native call"));
        }
        Ok(())
    }
    pub(super) fn loader_message(&mut self, text: &str) {
        if text.contains("Calling init routine")
            || text.contains("failed during DLL_PROCESS_ATTACH")
            || text.starts_with("Motor RNG probe ")
        {
            self.attach = text.chars().take(1024).collect();
        }
    }
    fn read(&self, address: u64, length: usize) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; length];
        let mut read = 0;
        check("native-call-read", unsafe {
            ReadProcessMemory(
                self.process,
                address as *const c_void,
                bytes.as_mut_ptr().cast(),
                length,
                &mut read,
            )
        })?;
        if read != length {
            return Err(step("native-call-read", "short read"));
        }
        Ok(bytes)
    }
    fn word(&self, address: u64) -> io::Result<u64> {
        Ok(u64::from_le_bytes(
            self.read(address, 8)?.try_into().unwrap(),
        ))
    }
    fn patch(&self, address: u64, byte: u8) -> io::Result<()> {
        let mut old = 0;
        check("native-call-code-protection", unsafe {
            VirtualProtectEx(
                self.process,
                address as *const c_void,
                1,
                PAGE_EXECUTE_READWRITE,
                &mut old,
            )
        })?;
        let mut wrote = 0;
        let write = check("native-call-breakpoint-write", unsafe {
            WriteProcessMemory(
                self.process,
                address as *const c_void,
                (&raw const byte).cast(),
                1,
                &mut wrote,
            )
        });
        let mut ignored = 0;
        let restore = check("native-call-code-protection-restore", unsafe {
            VirtualProtectEx(self.process, address as *const c_void, 1, old, &mut ignored)
        });
        write?;
        restore?;
        if wrote != 1 {
            return Err(step("native-call-breakpoint-write", "short write"));
        }
        check("native-call-instruction-cache", unsafe {
            FlushInstructionCache(self.process, address as *const c_void, 1)
        })
    }
    fn point(&mut self, address: u64, api: Option<Api>) -> io::Result<()> {
        if let Some(point) = self.points.get_mut(&address) {
            if api.is_none() {
                point.returns += 1;
            }
            return Ok(());
        }
        let original = self.read(address, 1)?[0];
        if original == 0xcc {
            return Err(step("native-call-breakpoint", "refusing an existing INT3"));
        }
        self.points.insert(
            address,
            Breakpoint {
                original,
                returns: usize::from(api.is_none()),
                api,
            },
        );
        // Track the original byte before a write/flush can fail, so abort can
        // restore a partially installed breakpoint while the debuggee is stopped.
        self.patch(address, 0xcc)?;
        Ok(())
    }
    fn site(&self, address: u64) -> Value {
        if let Some((base, name)) = self.modules.range(..=address).next_back() {
            json!({"address":format!("0x{address:X}"),"nearest_module":name,"module_offset":format!("0x{:X}",address-base)})
        } else {
            json!({"address":format!("0x{address:X}")})
        }
    }
    pub(super) fn module_loaded(&mut self, base: u64, name: &str) -> io::Result<Option<Value>> {
        self.modules.insert(base, name.into());
        if self.armed || !name.to_lowercase().ends_with("\\ntdll.dll") {
            return Ok(None);
        }
        let exports = self.remote_exports(base)?;
        let apis = [
            ("NtCreateFile", true),
            ("NtOpenFile", true),
            ("NtCreateSection", true),
            ("NtOpenSection", true),
            ("NtCreateKey", true),
            ("NtOpenKey", true),
            ("NtOpenKeyEx", true),
            ("NtCreateEvent", true),
            ("NtOpenEvent", true),
            ("NtCreateMutant", true),
            ("NtOpenMutant", true),
            ("NtCreateSemaphore", true),
            ("NtOpenSemaphore", true),
            ("NtOpenProcess", true),
            ("NtOpenThread", true),
            ("NtOpenProcessToken", false),
            ("NtOpenProcessTokenEx", false),
            ("NtOpenThreadToken", false),
            ("NtOpenThreadTokenEx", false),
            ("NtDeviceIoControlFile", false),
            ("NtFsControlFile", false),
            ("NtAlpcConnectPort", false),
            ("NtConnectPort", false),
        ];
        let mut candidates = Vec::new();
        for (name, object_attributes) in apis {
            let address = *exports
                .get(name)
                .ok_or_else(|| step("native-call-exports", name))?;
            if self.read(address, 1)?[0] == 0xcc {
                return Err(step("native-call-exports", "existing export breakpoint"));
            }
            candidates.push((
                address,
                Api {
                    name,
                    object_attributes,
                },
            ));
        }
        let mut installed = Vec::new();
        for (address, api) in candidates {
            let name = api.name;
            self.point(address, Some(api))?;
            installed.push(json!({"api":name,"address":format!("0x{address:X}")}));
        }
        self.armed = true;
        Ok(Some(
            json!({"kind":"native-call-trace-armed","architecture":"x64","mapping":"remote PE export table","apis":installed}),
        ))
    }

    fn remote_exports(&self, base: u64) -> io::Result<BTreeMap<String, u64>> {
        fn word(bytes: &[u8], offset: usize) -> io::Result<u32> {
            let value = bytes
                .get(offset..offset + 4)
                .ok_or_else(|| step("native-call-pe", "header/table outside buffer"))?;
            Ok(u32::from_le_bytes(value.try_into().unwrap()))
        }
        let header = self.read(base, 4096)?;
        if header.get(..2) != Some(b"MZ".as_slice()) {
            return Err(step("native-call-pe", "missing MZ signature"));
        }
        let pe = word(&header, 0x3c)? as usize;
        if header.get(pe..pe + 4) != Some(b"PE\0\0".as_slice())
            || header.get(pe + 4..pe + 6) != Some([0x64, 0x86].as_slice())
            || header.get(pe + 24..pe + 26) != Some([0x0b, 0x02].as_slice())
        {
            return Err(step("native-call-pe", "not an AMD64 PE32+ image"));
        }
        let image_size = word(&header, pe + 24 + 56)?;
        let rva = word(&header, pe + 24 + 112)?;
        let length = word(&header, pe + 24 + 116)?;
        if !(40..=4 * 1024 * 1024).contains(&length)
            || rva.checked_add(length).is_none_or(|end| end > image_size)
        {
            return Err(step("native-call-pe", "invalid export directory extent"));
        }
        let exports = self.read(base + u64::from(rva), length as usize)?;
        let offset = |address: u32| -> io::Result<usize> {
            address
                .checked_sub(rva)
                .filter(|value| *value < length)
                .map(|value| value as usize)
                .ok_or_else(|| step("native-call-pe", "export table outside directory"))
        };
        let functions = word(&exports, 20)?;
        let names = word(&exports, 24)?;
        if functions > 65536 || names > 65536 {
            return Err(step("native-call-pe", "oversized export tables"));
        }
        let function_table = offset(word(&exports, 28)?)?;
        let name_table = offset(word(&exports, 32)?)?;
        let ordinals = offset(word(&exports, 36)?)?;
        let mut result = BTreeMap::new();
        for index in 0..names as usize {
            let name_offset = offset(word(&exports, name_table + index * 4)?)?;
            let bytes = &exports[name_offset..];
            let end = bytes
                .iter()
                .take(256)
                .position(|byte| *byte == 0)
                .ok_or_else(|| step("native-call-pe", "unterminated export name"))?;
            let name = std::str::from_utf8(&bytes[..end])
                .map_err(|error| step("native-call-pe", error))?;
            let ordinal = exports
                .get(ordinals + index * 2..ordinals + index * 2 + 2)
                .ok_or_else(|| step("native-call-pe", "ordinal outside table"))?;
            let ordinal = u16::from_le_bytes(ordinal.try_into().unwrap()) as u32;
            if ordinal >= functions {
                return Err(step("native-call-pe", "export ordinal outside functions"));
            }
            let function = word(&exports, function_table + ordinal as usize * 4)?;
            if function == 0 || (function >= rva && function < rva + length) {
                continue;
            }
            if function >= image_size {
                return Err(step("native-call-pe", "export outside image"));
            }
            result.insert(name.into(), base + u64::from(function));
        }
        Ok(result)
    }

    fn unicode(&self, address: u64) -> io::Result<String> {
        let string = self.read(address, 16)?;
        let length = u16::from_le_bytes(string[..2].try_into().unwrap()) as usize;
        let buffer = u64::from_le_bytes(string[8..16].try_into().unwrap());
        if length > 32768 || !length.is_multiple_of(2) {
            return Err(step("native-call-name", "invalid UNICODE_STRING length"));
        }
        if length == 0 {
            return Ok(String::new());
        }
        let bytes = self.read(buffer, length)?;
        Ok(String::from_utf16_lossy(
            &bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect::<Vec<_>>(),
        ))
    }
    fn handle_name(&self, handle: u64) -> io::Result<String> {
        let mut duplicate = null_mut();
        check("native-call-root-handle", unsafe {
            DuplicateHandle(
                self.process,
                handle as HANDLE,
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        })?;
        let duplicate = Handle(duplicate);
        let mut needed = 0;
        unsafe {
            NtQueryObject(duplicate.0, 1, null_mut(), 0, &mut needed);
        }
        if !(16..=1024 * 1024).contains(&needed) {
            return Err(step("native-call-root-name", "invalid object-name size"));
        }
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        let status = unsafe {
            NtQueryObject(
                duplicate.0,
                1,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if status < 0 {
            return Err(step(
                "native-call-root-name",
                format!("0x{:08X}", status as u32),
            ));
        }
        let raw =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8) };
        let length = u16::from_le_bytes(raw[..2].try_into().unwrap()) as usize;
        let pointer = u64::from_le_bytes(raw[8..16].try_into().unwrap()) as usize;
        let start = buffer.as_ptr() as usize;
        if pointer < start
            || pointer
                .checked_add(length)
                .is_none_or(|end| end > start + raw.len())
            || !length.is_multiple_of(2)
        {
            return Err(step(
                "native-call-root-name",
                "invalid returned name buffer",
            ));
        }
        Ok(String::from_utf16_lossy(
            &unsafe { std::slice::from_raw_parts(pointer as *const u8, length) }
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect::<Vec<_>>(),
        ))
    }
    fn target(&self, api: &Api, context: &CONTEXT) -> io::Result<Value> {
        if matches!(api.name, "NtDeviceIoControlFile" | "NtFsControlFile") {
            let control = self.word(context.Rsp + 0x30)? as u32;
            return Ok(
                json!({"file_handle":format!("0x{:X}",context.Rcx),"file_name":self.handle_name(context.Rcx).map_err(|error|error.to_string()),
                "control_code":format!("0x{control:08X}"),"control_access_bits":(control>>14)&3,"buffers_not_recorded":true}),
            );
        }
        if matches!(api.name, "NtAlpcConnectPort" | "NtConnectPort") {
            return Ok(json!({"port_name":self.unicode(context.Rdx)?,"buffers_not_recorded":true}));
        }
        if !api.object_attributes {
            return Ok(json!({"source_handle":format!("0x{:X}",context.Rcx),"object_name":null}));
        }
        if matches!(api.name, "NtOpenProcess" | "NtOpenThread") {
            let client = self.read(context.R9, 16)?;
            return Ok(
                json!({"process_id":u64::from_le_bytes(client[..8].try_into().unwrap()),"thread_id":u64::from_le_bytes(client[8..16].try_into().unwrap())}),
            );
        }
        if context.R8 == 0 {
            return Ok(json!({"object_name":null,"anonymous":true}));
        }
        let attributes = self.read(context.R8, 48)?;
        let length = u32::from_le_bytes(attributes[..4].try_into().unwrap());
        if length < 48 {
            return Err(step("native-call-target", "invalid OBJECT_ATTRIBUTES"));
        }
        let root = u64::from_le_bytes(attributes[8..16].try_into().unwrap());
        let name = u64::from_le_bytes(attributes[16..24].try_into().unwrap());
        let name = if name == 0 {
            None
        } else {
            Some(self.unicode(name)?)
        };
        let root_name = if root == 0 {
            None
        } else {
            Some(self.handle_name(root).map_err(|error| error.to_string()))
        };
        Ok(
            json!({"object_name":name,"root_handle":format!("0x{root:X}"),"root_name":root_name,
            "attributes_hex":format!("0x{:08X}",u32::from_le_bytes(attributes[24..28].try_into().unwrap()))}),
        )
    }
    fn pause_others(&self, current: u32) -> io::Result<Paused> {
        let mut paused = Paused(BTreeMap::new());
        for tid in self.threads.keys().filter(|tid| **tid != current) {
            paused.hold(*tid)?;
        }
        Ok(paused)
    }
    pub(super) fn exception(
        &mut self,
        tid: u32,
        status: i32,
        address: u64,
    ) -> io::Result<Option<Value>> {
        if status == STATUS_SINGLE_STEP {
            if let Some(mut step) = self.steps.remove(&tid) {
                let result = (|| {
                    let thread = thread(tid)?;
                    let mut context = context(thread.0)?;
                    context.0.EFlags &= !0x100;
                    if let Some(point) = self.points.get(&step.address) {
                        if point.api.is_some() || point.returns > 0 {
                            self.patch(step.address, 0xcc)?;
                        } else {
                            self.points.remove(&step.address);
                        }
                    }
                    check("native-call-single-step-finish", unsafe {
                        SetThreadContext(thread.0, &context.0)
                    })?;
                    step.suspended.release()
                })();
                if let Err(error) = result {
                    self.steps.insert(tid, step);
                    return Err(error);
                }
                return Ok(Some(json!({"kind":"native-call-single-step"})));
            }
            return Ok(None);
        }
        if status != STATUS_BREAKPOINT || !self.points.contains_key(&address) {
            if self.steps.contains_key(&tid)
                || self.calls.get(&tid).is_some_and(|calls| !calls.is_empty())
            {
                return Err(step_error(format!(
                    "exception 0x{:08X} at 0x{address:X} interrupted an instrumented call/step",
                    status as u32
                )));
            }
            return Ok(None);
        }
        let owner = self.steps.keys().next().copied();
        if native_call_trace::defer_breakpoint(owner, tid) {
            // A breakpoint from another thread can already be queued before we
            // single-step the owner. Rewind and hold that thread for replay;
            // starting a second step would let the threads suspend each other.
            self.steps
                .get_mut(&owner.unwrap())
                .unwrap()
                .suspended
                .hold(tid)?;
            let thread = thread(tid)?;
            let mut context = context(thread.0)?;
            context.0.Rip = address;
            check("native-call-defer", unsafe {
                SetThreadContext(thread.0, &context.0)
            })?;
            return Ok(Some(
                json!({"kind":"native-call-deferred-breakpoint","thread":tid,"owner":owner,"address":format!("0x{address:X}")}),
            ));
        }
        if owner == Some(tid) {
            return Err(step_error(
                "another breakpoint arrived before the single step completed",
            ));
        }
        let thread = thread(tid)?;
        let mut context = context(thread.0)?;
        let expected = self.calls.get_mut(&tid).and_then(|calls| {
            calls
                .iter()
                .rposition(|call| {
                    native_call_trace::matches_return(
                        call.stack,
                        call.address,
                        context.0.Rsp,
                        address,
                    )
                })
                .map(|index| calls.remove(index))
        });
        let result = if let Some(call) = expected {
            if let Some(point) = self.points.get_mut(&address) {
                point.returns = point.returns.saturating_sub(1);
            }
            let (code, failed) = native_call_trace::ntstatus(context.0.Rax);
            json!({"kind":"native-api-return","api":call.api.name,"status":format!("0x{code:08X}"),"failed":failed,
                "desired_access":access_text(call.access),"target":call.target,"return_site":call.site,"phase_hint":call.attach,
                "pending_completion_not_observed":code==0x103})
        } else if let Some(api) = self.points[&address].api.clone() {
            let return_address = self.word(context.0.Rsp)?;
            if self.calls.values().map(Vec::len).sum::<usize>() >= 1024 {
                return Err(step("native-call-capacity", "too many pending calls"));
            }
            let target=self.target(&api,&context.0).unwrap_or_else(|error|json!({"decode_error":error.to_string(),"object_attributes_pointer":format!("0x{:X}",context.0.R8)}));
            self.point(return_address, None)?;
            let call = Call {
                api: api.clone(),
                address: return_address,
                stack: context.0.Rsp,
                access: desired_access(&api, &context.0),
                target: target.clone(),
                site: self.site(return_address),
                attach: self.attach.clone(),
            };
            self.calls.entry(tid).or_default().push(call);
            json!({"kind":"native-api-entry","api":api.name,"desired_access":access_text(desired_access(&api,&context.0)),"target":target,"return_site":self.site(return_address)})
        } else {
            json!({"kind":"native-call-unmatched-return","address":format!("0x{address:X}")})
        };
        // Keep other debuggee threads paused for exactly one instruction while
        // our INT3 byte is absent. Preserve their prior suspend counts.
        let paused = self.pause_others(tid)?;
        self.patch(address, self.points[&address].original)?;
        context.0.Rip = address;
        context.0.EFlags |= 0x100;
        check("native-call-single-step-start", unsafe {
            SetThreadContext(thread.0, &context.0)
        })?;
        self.steps.insert(
            tid,
            Step {
                address,
                suspended: paused,
            },
        );
        Ok(Some(result))
    }

    /// Abort while a debug event still stops the entire child: restore tracked
    /// bytes/contexts and release our suspend counts before the caller kills it.
    pub(super) fn abort(&mut self, current: u32, stopped_address: Option<u64>) -> io::Result<()> {
        let mut errors = Vec::new();
        if let Some(address) = stopped_address.filter(|address| self.points.contains_key(address)) {
            let result = (|| {
                let thread = thread(current)?;
                let mut context = context(thread.0)?;
                if context.0.Rip == address + 1 {
                    context.0.Rip = address;
                }
                context.0.EFlags &= !0x100;
                check("native-call-abort-context", unsafe {
                    SetThreadContext(thread.0, &context.0)
                })
            })();
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        for (address, point) in &self.points {
            if let Err(error) = self.patch(*address, point.original) {
                errors.push(error.to_string());
            }
        }
        for (tid, mut saved) in std::mem::take(&mut self.steps) {
            let result = (|| {
                let thread = thread(tid)?;
                let mut context = context(thread.0)?;
                context.0.EFlags &= !0x100;
                check("native-call-abort-step", unsafe {
                    SetThreadContext(thread.0, &context.0)
                })
            })();
            if let Err(error) = result {
                errors.push(error.to_string());
            }
            if let Err(error) = saved.suspended.release() {
                errors.push(error.to_string());
            }
        }
        self.calls.clear();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(step("native-call-abort", errors.join("; ")))
        }
    }
}
struct Paused(BTreeMap<u32, Handle>);
impl Paused {
    fn hold(&mut self, tid: u32) -> io::Result<()> {
        if self.0.contains_key(&tid) {
            return Ok(());
        }
        let handle = unsafe {
            OpenThread(
                THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                0,
                tid,
            )
        };
        if handle.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(87) {
                return Ok(());
            }
            return Err(step("native-call-pause", error));
        }
        let handle = Handle(handle);
        if unsafe { SuspendThread(handle.0) } == u32::MAX {
            return Err(step("native-call-pause", io::Error::last_os_error()));
        }
        self.0.insert(tid, handle);
        Ok(())
    }
    fn release(&mut self) -> io::Result<()> {
        let mut errors = Vec::new();
        self.0.retain(|_, thread| {
            if unsafe { ResumeThread(thread.0) } == u32::MAX {
                let failure = io::Error::last_os_error();
                let mut code = 259;
                if unsafe { GetExitCodeThread(thread.0, &mut code) } != 0 && code != 259 {
                    return false;
                }
                errors.push(failure.to_string());
                true
            } else {
                false
            }
        });
        if errors.is_empty() {
            Ok(())
        } else {
            Err(step("native-call-resume", errors.join("; ")))
        }
    }
}
impl Drop for Paused {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            eprintln!("{error}");
        }
    }
}
fn step_error(error: impl std::fmt::Display) -> io::Error {
    step("native-call-step", error)
}
fn desired_access(api: &Api, context: &CONTEXT) -> Option<u32> {
    if matches!(
        api.name,
        "NtDeviceIoControlFile" | "NtFsControlFile" | "NtAlpcConnectPort" | "NtConnectPort"
    ) {
        None
    } else {
        Some(context.Rdx as u32)
    }
}
fn access_text(access: Option<u32>) -> Option<String> {
    access.map(|value| format!("0x{value:08X}"))
}
#[repr(align(16))]
struct Context(CONTEXT);
fn context(thread: HANDLE) -> io::Result<Context> {
    let mut context = Context(unsafe { zeroed() });
    context.0.ContextFlags = CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64;
    check("native-call-context", unsafe {
        GetThreadContext(thread, &mut context.0)
    })?;
    Ok(context)
}
fn thread(tid: u32) -> io::Result<Handle> {
    let handle = unsafe { OpenThread(THREAD_GET_CONTEXT | THREAD_SET_CONTEXT, 0, tid) };
    if handle.is_null() {
        Err(step("native-call-thread", io::Error::last_os_error()))
    } else {
        Ok(Handle(handle))
    }
}
