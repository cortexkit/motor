//! Local WMI/COM adapter for MSFT_Volume.Format (Storagewmi.mof).

use super::{FormatOptions, FormatResult, FormatValue, StorageProvider, Volume, remaining_ms};
use std::{io, sync::OnceLock, time::Instant};
use windows::{
    Win32::{
        Foundation::RPC_E_TOO_LATE,
        System::{
            Com::*,
            Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE},
            Variant::{
                VARIANT, VT_BSTR, VT_DISPATCH, VT_EMPTY, VT_I4, VT_NULL, VT_UI4, VT_UNKNOWN,
            },
            Wmi::*,
        },
    },
    core::{BSTR, HRESULT, IUnknown, Interface, PCWSTR, w},
};

fn com<T>(operation: &str, result: windows::core::Result<T>) -> io::Result<T> {
    result.map_err(|error| {
        io::Error::other(format!(
            "cache WMI {operation}: HRESULT 0x{:08x}: {error}",
            error.code().0 as u32,
        ))
    })
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn secure(proxy: impl windows::core::Param<IUnknown>) -> io::Result<()> {
    com("proxy security", unsafe {
        CoSetProxyBlanket(
            proxy,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            None,
            RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
    })
}

fn property(object: &IWbemClassObject, name: PCWSTR) -> io::Result<VARIANT> {
    let mut value = VARIANT::default();
    com("read property", unsafe {
        object.Get(name, 0, &mut value, None, None)
    })?;
    Ok(value)
}

fn string(object: &IWbemClassObject, name: PCWSTR) -> io::Result<String> {
    let value = property(object, name)?;
    match value.vt() {
        VT_BSTR => Ok(unsafe { value.Anonymous.Anonymous.Anonymous.bstrVal.to_string() }),
        VT_NULL | VT_EMPTY => Ok(String::new()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache WMI expected a string property",
        )),
    }
}

fn return_value(object: &IWbemClassObject) -> io::Result<u32> {
    let value = property(object, w!("ReturnValue"))?;
    // CIM uint32 properties may be represented as VT_I4 by WMI. Do not coerce
    // null, missing or string values to success.
    match value.vt() {
        VT_I4 => Ok(unsafe { value.Anonymous.Anonymous.Anonymous.lVal as u32 }),
        VT_UI4 => Ok(unsafe { value.Anonymous.Anonymous.Anonymous.ulVal }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache WMI Format has no numeric ReturnValue",
        )),
    }
}

fn extended_status(object: &IWbemClassObject) -> io::Result<String> {
    let value = property(object, w!("ExtendedStatus"))?;
    let embedded: IWbemClassObject = match value.vt() {
        VT_BSTR => return Ok(unsafe { value.Anonymous.Anonymous.Anonymous.bstrVal.to_string() }),
        VT_NULL | VT_EMPTY => return Ok(String::new()),
        VT_UNKNOWN => {
            let unknown = unsafe { value.Anonymous.Anonymous.Anonymous.punkVal.as_ref() }
                .ok_or_else(|| io::Error::other("null embedded status object"))?;
            com("read embedded status", unknown.cast())?
        }
        VT_DISPATCH => {
            let dispatch = unsafe { value.Anonymous.Anonymous.Anonymous.pdispVal.as_ref() }
                .ok_or_else(|| io::Error::other("null embedded status object"))?;
            com("read embedded status", dispatch.cast())?
        }
        _ => {
            return Err(io::Error::other(format!(
                "unexpected ExtendedStatus variant {}",
                value.vt().0
            )));
        }
    };
    com("render embedded status", unsafe {
        embedded.GetObjectText(0)
    })
    .map(|text| text.to_string())
}

pub(super) struct Provider {
    services: IWbemServices,
    call: Option<IWbemCallResult>,
    // COM interfaces must be released before this thread's apartment is closed.
    _apartment: Apartment,
}

impl Provider {
    pub(super) fn connect() -> io::Result<Self> {
        com("initialize COM", unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()
        })?;
        let apartment = Apartment;
        static SECURITY: OnceLock<Result<(), i32>> = OnceLock::new();
        let security = SECURITY.get_or_init(|| unsafe {
            CoInitializeSecurity(
                None,
                -1,
                None,
                None,
                RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
                None,
            )
            .or_else(|error| {
                if error.code() == RPC_E_TOO_LATE {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|error| error.code().0)
        });
        com(
            "initialize security",
            security.map_err(|code| windows::core::Error::from_hresult(HRESULT(code))),
        )?;
        let locator: IWbemLocator = com("create locator", unsafe {
            CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)
        })?;
        let empty = BSTR::new();
        let services = com("connect storage namespace", unsafe {
            locator.ConnectServer(
                &BSTR::from("ROOT\\Microsoft\\Windows\\Storage"),
                &empty,
                &empty,
                &empty,
                WBEM_FLAG_CONNECT_USE_MAX_WAIT.0,
                &empty,
                None,
            )
        })?;
        secure(&services)?;
        Ok(Self {
            services,
            call: None,
            _apartment: apartment,
        })
    }

    fn get_object(&self, path: &str, deadline: Instant) -> io::Result<IWbemClassObject> {
        remaining_ms(deadline)?;
        let mut call = None;
        com("get object", unsafe {
            self.services.GetObject(
                &BSTR::from(path),
                WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
                None,
                Some(&mut call),
            )
        })?;
        let call = call.ok_or_else(|| io::Error::other("cache WMI GetObject returned no call"))?;
        secure(&call)?;
        loop {
            if complete(&call, remaining_ms(deadline)?)? {
                return com("get object result", unsafe { call.GetResultObject(0) });
            }
        }
    }
}

fn complete(call: &IWbemCallResult, timeout_ms: i32) -> io::Result<bool> {
    let mut status = 0;
    // The generated GetCallStatus wrapper discards success HRESULTs, including
    // WBEM_S_TIMEDOUT. Preserve that HRESULT instead of treating its zeroed
    // output status as a completed operation.
    let hr = unsafe { (call.vtable().GetCallStatus)(call.as_raw(), timeout_ms, &mut status) };
    super::call_complete(hr.0, status)
}

impl StorageProvider for Provider {
    fn volumes(&mut self, deadline: Instant) -> io::Result<Vec<Volume>> {
        remaining_ms(deadline)?;
        let enumerator = com("query volumes", unsafe {
            self.services.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from("SELECT * FROM MSFT_Volume"),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            )
        })?;
        secure(&enumerator)?;
        let mut volumes = Vec::new();
        loop {
            let mut objects = [None];
            let mut returned = 0;
            let hr =
                unsafe { enumerator.Next(remaining_ms(deadline)?, &mut objects, &mut returned) };
            com("enumerate volumes", hr.ok())?;
            if returned == 0 {
                if hr.0 == WBEM_S_FALSE.0 {
                    return Ok(volumes);
                }
                if hr.0 == WBEM_S_TIMEDOUT.0 {
                    continue;
                }
                return Err(io::Error::other(
                    "cache WMI volume enumeration made no progress",
                ));
            }
            let object = objects[0]
                .take()
                .ok_or_else(|| io::Error::other("cache WMI returned no volume object"))?;
            volumes.push(Volume {
                path: string(&object, w!("Path"))?,
                object_path: string(&object, w!("__PATH"))?,
            });
            if volumes.len() > 128 {
                return Err(io::Error::other("cache WMI returned too many volumes"));
            }
        }
    }

    fn start(
        &mut self,
        object_path: &str,
        options: &FormatOptions,
        deadline: Instant,
    ) -> io::Result<()> {
        let class = self.get_object("MSFT_Volume", deadline)?;
        let mut signature = None;
        com("get Format signature", unsafe {
            class.GetMethod(w!("Format"), 0, &mut signature, std::ptr::null_mut())
        })?;
        let signature =
            signature.ok_or_else(|| io::Error::other("cache WMI Format has no input signature"))?;
        let input = com("create Format parameters", unsafe {
            signature.SpawnInstance(0)
        })?;
        for (name, value) in options.parameters() {
            let name = BSTR::from(name);
            let value = match value {
                FormatValue::Text(value) => VARIANT::from(value),
                FormatValue::Number(value) => VARIANT::from(value),
                FormatValue::Boolean(value) => VARIANT::from(value),
            };
            com("set Format parameter", unsafe {
                input.Put(PCWSTR(name.as_ptr()), 0, &value, 0)
            })?;
        }
        remaining_ms(deadline)?;
        com("begin Format", unsafe {
            self.services.ExecMethod(
                &BSTR::from(object_path),
                &BSTR::from("Format"),
                WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
                &input,
                None,
                Some(&mut self.call),
            )
        })?;
        let call = self
            .call
            .as_ref()
            .ok_or_else(|| io::Error::other("cache WMI Format returned no call"))?;
        secure(call)
    }

    fn poll(&mut self, timeout_ms: i32) -> io::Result<Option<FormatResult>> {
        let call = self
            .call
            .as_ref()
            .ok_or_else(|| io::Error::other("cache WMI Format was not started"))?;
        if !complete(call, timeout_ms)? {
            return Ok(None);
        }
        let output = com("Format output", unsafe { call.GetResultObject(0) })?;
        Ok(Some(FormatResult {
            return_value: return_value(&output)?,
            // The embedded diagnostic may be a serialized string or a COM
            // object. A diagnostic decoding failure must not hide ReturnValue.
            extended_status: extended_status(&output)
                .unwrap_or_else(|error| format!("unavailable: {error}")),
        }))
    }
}
