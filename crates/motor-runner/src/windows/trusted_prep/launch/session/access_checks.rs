//! Read-only descriptor/token screening. This module never sets an ACL or
//! impersonates a service thread; AccessCheck receives an explicit token handle.
use super::*;
use crate::windows::{access_screen,VmConfig};
use serde_json::{json,Value};
use std::{collections::BTreeSet,fs,io,mem::{size_of,zeroed},os::windows::fs::MetadataExt,path::{Path,PathBuf},ptr::{null,null_mut}};
use windows_sys::Win32::{Foundation::*,Security::{Authorization::*,*},Storage::FileSystem::*,UI::Shell::GetUserProfileDirectoryW};

const SD_PARTS:u32=OWNER_SECURITY_INFORMATION|GROUP_SECURITY_INFORMATION|DACL_SECURITY_INFORMATION;
const DEVICE_ACCESS:u32=0x00100003;
#[repr(C)] struct Unicode {length:u16,maximum:u16,buffer:*mut u16}
#[repr(C)] struct Attributes {length:u32,root:HANDLE,name:*mut Unicode,attributes:u32,security:*mut std::ffi::c_void,qos:*mut std::ffi::c_void}
#[repr(C)] struct IoStatus {status:usize,information:usize}
#[link(name="ntdll")]
unsafe extern "system" {
    fn NtOpenFile(handle:*mut HANDLE,access:u32,attributes:*mut Attributes,status:*mut IoStatus,share:u32,options:u32)->i32;
    fn NtQuerySecurityObject(handle:HANDLE,parts:u32,descriptor:*mut std::ffi::c_void,length:u32,needed:*mut u32)->i32;
    fn NtQueryObject(handle:HANDLE,class:u32,buffer:*mut std::ffi::c_void,length:u32,needed:*mut u32)->i32;
}
struct Descriptor {words:Vec<u64>,label_error:Option<String>}
impl Descriptor {
    fn pointer(&self)->PSECURITY_DESCRIPTOR {self.words.as_ptr().cast_mut().cast()}
    fn read(handle:HANDLE)->io::Result<Self> {
        fn query(handle:HANDLE,parts:u32)->io::Result<Vec<u64>> {
            let mut needed=0; let status=unsafe {NtQuerySecurityObject(handle,parts,null_mut(),0,&mut needed)};
            if !(20..=1024*1024).contains(&needed) {return Err(step("query-security",format!("invalid size {needed}; NTSTATUS0x{:08X}",status as u32)));}
            let mut words=vec![0u64;(needed as usize).div_ceil(8)];
            let status=unsafe {NtQuerySecurityObject(handle,parts,words.as_mut_ptr().cast(),needed,&mut needed)};
            if status<0 {return Err(step("query-security",format!("NTSTATUS0x{:08X}",status as u32)));} Ok(words)
        }
        match query(handle,SD_PARTS|LABEL_SECURITY_INFORMATION) {
            Ok(words)=>Ok(Self{words,label_error:None}),
            Err(error)=>Ok(Self{words:query(handle,SD_PARTS)?,label_error:Some(error.to_string())}),
        }
    }
    fn description(&self)->io::Result<Value> {
        let mut owner=null_mut(); let mut group=null_mut(); let mut defaulted=0;
        check("descriptor-owner",unsafe {GetSecurityDescriptorOwner(self.pointer(),&mut owner,&mut defaulted)})?;
        check("descriptor-group",unsafe {GetSecurityDescriptorGroup(self.pointer(),&mut group,&mut defaulted)})?;
        let mut present=0; let mut dacl=null_mut();
        check("descriptor-dacl",unsafe {GetSecurityDescriptorDacl(self.pointer(),&mut present,&mut dacl,&mut defaulted)})?;
        let mut entries=Vec::new(); let mut complete=true;
        if !dacl.is_null() {
            for index in 0..unsafe {(*dacl).AceCount} {
                let mut pointer=null_mut(); check("descriptor-ace",unsafe {GetAce(dacl,index as u32,&mut pointer)})?;
                let ace=unsafe {&*pointer.cast::<ACCESS_ALLOWED_ACE>()};
                if ace.Header.AceType<=1 {
                    let mut mask=ace.Mask; let mapping=mapping(); unsafe {MapGenericMask(&mut mask,&mapping);}
                    entries.push(json!({"ace_type":ace.Header.AceType,"sid":sid_string((&raw const ace.SidStart).cast_mut().cast())?,
                        "raw_mask":format!("0x{:08X}",ace.Mask),"mapped_mask":format!("0x{mask:08X}"),
                        "allow_write_data":ace.Header.AceType==0 && mask&2!=0,"inherit_only":ace.Header.AceFlags&8!=0}));
                } else {complete=false;entries.push(json!({"unsupported_ace_type":ace.Header.AceType}));}
            }
        }
        let mut labels=Vec::new(); let mut sacl=null_mut();
        if self.label_error.is_none() {
            check("descriptor-label",unsafe {GetSecurityDescriptorSacl(self.pointer(),&mut present,&mut sacl,&mut defaulted)})?;
            if !sacl.is_null() {
                for index in 0..unsafe {(*sacl).AceCount} {
                    let mut pointer=null_mut(); check("descriptor-label-ace",unsafe {GetAce(sacl,index as u32,&mut pointer)})?;
                    let ace=unsafe {&*pointer.cast::<SYSTEM_MANDATORY_LABEL_ACE>()};
                    if ace.Header.AceType==17 {labels.push(json!({"sid":sid_string((&raw const ace.SidStart).cast_mut().cast())?,"policy_hex":format!("0x{:08X}",ace.Mask)}));}
                }
            }
        }
        let mut text=null_mut();
        check("descriptor-sddl",unsafe {ConvertSecurityDescriptorToStringSecurityDescriptorW(self.pointer(),1,SD_PARTS|if self.label_error.is_none(){SACL_SECURITY_INFORMATION}else{0},&mut text,null_mut())})?;
        let owned=Local(text.cast()); let sddl=unsafe {read_wide(owned.0.cast())}?;
        Ok(json!({"owner":sid_string(owner)?,"group":sid_string(group)?,"sddl":sddl,"dacl_present":!dacl.is_null(),"aces":entries,"ace_decode_complete":complete,
            "integrity_labels":labels,"integrity_label_error":self.label_error,"unlabelled_means_medium":self.label_error.is_none() && labels.is_empty(),
            "audit_sacl_not_requested":true}))
    }
}
fn mapping()->GENERIC_MAPPING {GENERIC_MAPPING{GenericRead:FILE_GENERIC_READ,GenericWrite:FILE_GENERIC_WRITE,GenericExecute:FILE_GENERIC_EXECUTE,GenericAll:FILE_ALL_ACCESS}}
fn open_device()->io::Result<Handle> {
    let mut name=wide(r"\Device\KsecDD"); let mut unicode=Unicode{length:((name.len()-1)*2) as u16,maximum:(name.len()*2) as u16,buffer:name.as_mut_ptr()};
    let mut attributes=Attributes{length:size_of::<Attributes>() as u32,root:null_mut(),name:&mut unicode,attributes:0x40,security:null_mut(),qos:null_mut()};
    let mut status:IoStatus=unsafe {zeroed()}; let mut handle=null_mut();
    let result=unsafe {NtOpenFile(&mut handle,READ_CONTROL|SYNCHRONIZE,&mut attributes,&mut status,3,0x20)};
    if result<0 {return Err(step("open-ksecdd-read-control",format!("NTSTATUS0x{:08X}",result as u32)));}
    Ok(Handle(handle))
}
fn open_path(path:&Path)->io::Result<Handle> {
    let raw=unsafe {CreateFileW(wide(&path.to_string_lossy()).as_ptr(),READ_CONTROL|FILE_READ_ATTRIBUTES,FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,null(),OPEN_EXISTING,FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OPEN_REPARSE_POINT,null_mut())};
    if raw==INVALID_HANDLE_VALUE {return Err(step("open-protected-descriptor",format!("{}: {}",path.display(),io::Error::last_os_error())));}
    Ok(Handle(raw))
}
fn impersonation(token:HANDLE)->io::Result<Handle> {
    let mut result=null_mut();
    check("access-token-duplicate",unsafe {DuplicateTokenEx(token,TOKEN_QUERY,null(),SecurityImpersonation,TokenImpersonation,&mut result)})?;
    Ok(Handle(result))
}
fn access(sd:PSECURITY_DESCRIPTOR,token:HANDLE,mask:u32)->Value {
    let mut privileges=vec![0u64;32]; let mut length=(privileges.len()*8) as u32;
    let mut granted=0; let mut status=0;
    let mut ok=unsafe {AccessCheck(sd,token,mask,&mapping(),privileges.as_mut_ptr().cast(),&mut length,&mut granted,&mut status)};
    let mut error=if ok==0 {unsafe {GetLastError()}}else{0};
    if ok==0 && error==ERROR_INSUFFICIENT_BUFFER && length<=65536 {
        privileges.resize((length as usize).div_ceil(8),0);
        ok=unsafe {AccessCheck(sd,token,mask,&mapping(),privileges.as_mut_ptr().cast(),&mut length,&mut granted,&mut status)};
        error=if ok==0 {unsafe {GetLastError()}}else{0};
    }
    let decision=access_screen::decision(ok!=0,status!=0,granted,mask);
    json!({"requested":format!("0x{mask:08X}"),"api_succeeded":ok!=0,"access_status":status!=0,
        "granted_mask":format!("0x{granted:08X}"),"win32_error":format!("0x{error:08X}"),"decision":decision,
        "privilege_count":if ok!=0 {Some(unsafe {*privileges.as_ptr().cast::<u32>()})}else{None}})
}

struct Protected {name:String,path:PathBuf,directory:bool,descriptor:Option<Descriptor>,description:Value}
impl Protected {
    fn read(name:&str,path:PathBuf)->Self {
        let result=(|| {
            let handle=open_path(&path)?;
            let mut info:BY_HANDLE_FILE_INFORMATION=unsafe {zeroed()};
            check("protected-file-attributes",unsafe {GetFileInformationByHandle(handle.0,&mut info)})?;
            if info.dwFileAttributes&FILE_ATTRIBUTE_REPARSE_POINT!=0 {return Err(step("protected-path","leaf reparse point is not an authoritative target"));}
            let descriptor=Descriptor::read(handle.0)?;
            let description=descriptor.description()?;
            Ok((descriptor,description,info.dwFileAttributes&FILE_ATTRIBUTE_DIRECTORY!=0,info.dwFileAttributes))
        })();
        match result {
            Ok((descriptor,description,directory,attributes))=>Self{name:name.into(),path,directory,descriptor:Some(descriptor),description:json!({"security":description,"attributes_hex":format!("0x{attributes:08X}")})},
            Err(error)=>Self{name:name.into(),path,directory:false,descriptor:None,description:json!({"error":error.to_string()})},
        }
    }
}
fn profile_path(token:HANDLE)->io::Result<PathBuf> {
    let mut length=32768; let mut path=vec![0u16;length as usize];
    check("profile-directory-read-only",unsafe {GetUserProfileDirectoryW(token,path.as_mut_ptr(),&mut length)})?;
    Ok(PathBuf::from(unsafe {read_wide(path.as_ptr())}?))
}
fn protected_paths(vm:&VmConfig,fixture:&Path,profile:&Path)->(Vec<Protected>,Vec<String>) {
    let root=&vm.layout_root; let mut rows=Vec::new(); let mut keys=BTreeSet::new(); let mut gaps=Vec::new();
    let mut add=|name:&str,path:PathBuf| {
        let key=path.to_string_lossy().replace('/',"\\").to_ascii_lowercase();
        if keys.insert(key) {rows.push(Protected::read(name,path));}
    };
    for (name,path) in [
        ("windows_temp",PathBuf::from(r"C:\Windows\Temp")),("profile",profile.to_owned()),
        ("profile_local_temp",profile.join("AppData/Local/Temp")),("persistent_prep_cache",root.join("state/ally/prep-cargo")),
        ("service_home",root.join("service-home")),("fixture_stage_root",fixture.to_owned()),
        ("fixture_manifests",fixture.join("fetch-manifests")),("fixture_manifest_file",fixture.join("fetch-manifests/Cargo.toml")),
        ("toolchain",root.join("prep-toolchain")),("toolchain_bin",root.join("prep-toolchain/bin")),
        ("cargo_executable",root.join("prep-toolchain/bin/cargo.exe")),("rustc_executable",root.join("prep-toolchain/bin/rustc.exe")),
        ("runner_root_outside_staging",root.to_owned()),("baselines_directory",root.join("baselines")),
        ("configured_parent",vm.baseline_path()),
    ] {add(name,path);}
    let mut image_roots=vec![(root.join("baselines"),0)];
    if let Ok(bytes)=fs::read(root.join("installation.json")) {
        if let Ok(value)=serde_json::from_slice::<Value>(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes)) {
            if let Some(path)=value["provision_root"].as_str() {
                let path=PathBuf::from(path);
                if matches!(path.components().next(),Some(std::path::Component::Prefix(prefix)) if matches!(prefix.kind(),std::path::Prefix::Disk(_)|std::path::Prefix::VerbatimDisk(_))) && path.is_absolute() {image_roots.push((path,0));}
                else {gaps.push("provision_root is not a local absolute disk path".into());}
            }
        }
    }
    let mut scanned=0;
    while let Some((directory,depth))=image_roots.pop() {
        let entries=match fs::read_dir(&directory) {Ok(entries)=>entries,Err(error)=>{gaps.push(format!("image discovery {}: {error}",directory.display()));continue;}};
        for entry in entries {
            scanned+=1;
            if scanned>1000 {gaps.push("bounded image discovery exceeded 1000 entries".into());break;}
            let entry=match entry {Ok(entry)=>entry,Err(error)=>{gaps.push(error.to_string());continue;}};
            let path=entry.path(); let metadata=match fs::symlink_metadata(&path) {Ok(metadata)=>metadata,Err(error)=>{gaps.push(error.to_string());continue;}};
            if metadata.file_attributes()&FILE_ATTRIBUTE_REPARSE_POINT!=0 {gaps.push(format!("image discovery skipped reparse {}",path.display()));continue;}
            if metadata.is_dir() && depth<2 {image_roots.push((path,depth+1));}
            else if metadata.is_file() && path.extension().is_some_and(|extension|extension.eq_ignore_ascii_case("vhdx")||extension.eq_ignore_ascii_case("vhd")) {
                add("discovered_baseline_image",path.clone());
                if let Some(parent)=path.parent() {add("baseline_parent_directory",parent.to_owned());}
            }
        }
        if scanned>1000 {break;}
    }
    (rows,gaps)
}
const WRITE_RIGHTS:[(&str,u32);7]=[("write_data_or_add_file",2),("append_or_add_subdirectory",4),("write_extended_attributes",0x10),("write_attributes",0x100),("delete",0x10000),("write_dacl",0x40000),("write_owner",0x80000)];
fn path_checks(paths:&[Protected],token:HANDLE)->(Vec<Value>,Vec<Option<bool>>) {
    let mut rows=Vec::new(); let mut decisions=Vec::new();
    for path in paths {
        let mut checks=Vec::new();
        if let Some(sd)=&path.descriptor {
            for (name,mask) in WRITE_RIGHTS.into_iter().chain(path.directory.then_some(("delete_child",0x40))) {
                let result=access(sd.pointer(),token,mask);
                decisions.push(result["decision"].as_bool());
                checks.push(json!({"right":name,"result":result}));
            }
        } else {decisions.push(None);}
        rows.push(json!({"name":path.name,"path":path.path,"directory":path.directory,"checks":checks}));
    }
    (rows,decisions)
}
fn group_name(sid:&str)->&'static str {
    match sid {"S-1-1-0"=>"Everyone","S-1-5-11"=>"Authenticated Users","S-1-5-4"=>"INTERACTIVE","S-1-5-32-545"=>"Users","S-1-5-3"=>"BATCH","S-1-2-0"=>"LOCAL",_=>"account group (SID is authoritative)"}
}
fn enabled_groups(token:HANDLE,logon:&str)->io::Result<Vec<(String,u32)>> {
    let bytes=token_info(token,TokenGroups)?; let header=unsafe {&*bytes.as_ptr().cast::<TOKEN_GROUPS>()};
    let groups=unsafe {std::slice::from_raw_parts(header.Groups.as_ptr(),header.GroupCount as usize)};
    let mut result=Vec::new();
    for group in groups {
        let sid=sid_string(group.Sid)?;
        if group.Attributes&4!=0 && group.Attributes&0x70==0 && sid!=logon {result.push((sid,group.Attributes));}
    }
    Ok(result)
}
fn restrict(base:HANDLE,sids:&[String])->io::Result<Handle> {
    let mut owned=Vec::new();
    for sid in sids {let mut pointer=null_mut();check("candidate-sid",unsafe {ConvertStringSidToSidW(wide(sid).as_ptr(),&mut pointer)})?;owned.push(Local(pointer));}
    let restrictions=owned.iter().map(|sid|SID_AND_ATTRIBUTES{Sid:sid.0,Attributes:0}).collect::<Vec<_>>();
    let mut token=null_mut();
    check("candidate-write-restricted-token",unsafe {CreateRestrictedToken(base,FETCH_TOKEN_FLAGS,0,null(),0,null(),restrictions.len() as u32,restrictions.as_ptr(),&mut token)})?;
    Ok(Handle(token))
}
fn object_type(handle:HANDLE)->Value {
    let mut length=0;unsafe {NtQueryObject(handle,2,null_mut(),0,&mut length);}
    if !(16..=65536).contains(&length) {return json!({"error":"invalid object type size"});}
    let mut buffer=vec![0u64;(length as usize).div_ceil(8)];
    let status=unsafe {NtQueryObject(handle,2,buffer.as_mut_ptr().cast(),length,&mut length)};
    if status<0 {return json!({"error":format!("0x{:08X}",status as u32)});}
    let string=unsafe {&*buffer.as_ptr().cast::<Unicode>()};
    let pointer=string.buffer as usize; let start=buffer.as_ptr() as usize;
    if !string.length.is_multiple_of(2) || string.length>4096 || pointer<start || pointer.checked_add(string.length as usize).is_none_or(|end|end>start+buffer.len()*8) {return json!({"error":"invalid type name"});}
    json!({"name":String::from_utf16_lossy(unsafe {std::slice::from_raw_parts(string.buffer,string.length as usize/2)})})
}

pub(super) fn run(vm:&VmConfig,fixture:&Path,output:&Path)->io::Result<()> {
    let base=super::super::super::token(&vm.layout_root)?;
    let (user,logon)=identity(base.0)?;
    let profile=profile_path(base.0)?;
    let (paths,discovery_gaps)=protected_paths(vm,fixture,&profile);
    let mut report=json!({"diagnostic_only":true,"existing_acl_changes":false,"system_settings_changes":false,"fixture_acls_assigned_at_creation":true,
        "method":"read-only NtQuerySecurityObject and AccessCheck; no low-privilege child or device data I/O workload launched",
        "device":"\\Device\\KsecDD","desired_device_access":"0x00100003",
        "generic_mapping":{"read":format!("0x{:08X}",FILE_GENERIC_READ),"write":format!("0x{:08X}",FILE_GENERIC_WRITE),"execute":format!("0x{:08X}",FILE_GENERIC_EXECUTE),"all":format!("0x{:08X}",FILE_ALL_ACCESS)},
        "user_sid":user,"logon_sid":logon,"profile_loaded":false,"profile_path":profile,
        "protected_descriptors":paths.iter().map(|path|json!({"name":path.name,"path":path.path,"directory":path.directory,"descriptor":path.description})).collect::<Vec<_>>(),
        "image_discovery_gaps":discovery_gaps,"candidates":[],"screen_complete":false,
        "limitations":["AccessCheck evaluates the supplied descriptor, not driver-specific create policy or a real device open",
            "Mandatory integrity labels and readonly attributes are reported separately; descriptor write grants are conservatively rejected",
            "Passing all listed paths is not a path-independent staging-only guarantee when an ambient group SID is added"]});
    let collection=(|| {
        let device=open_device()?;
        let sd=Descriptor::read(device.0)?; let description=sd.description()?;
        report["device_security"]=description.clone(); report["device_handle_type"]=object_type(device.0);
        let writer_sids=description["aces"].as_array().unwrap().iter().filter(|ace|ace["allow_write_data"]==true && ace["inherit_only"]==false).filter_map(|ace|ace["sid"].as_str().map(str::to_owned)).collect::<BTreeSet<_>>();
        report["device_allow_write_data_ace_sids"]=json!(writer_sids);
        let groups=enabled_groups(base.0,&logon)?;
        report["account_enabled_groups"]=json!(groups.iter().map(|(sid,attributes)|json!({"sid":sid,"name":group_name(sid),"attributes_hex":format!("0x{attributes:08X}")})).collect::<Vec<_>>());
        let base_imp=impersonation(base.0)?;
        report["base_device_access_check"]=access(sd.pointer(),base_imp.0,DEVICE_ACCESS);
        let (base_paths,_)=path_checks(&paths,base_imp.0); report["base_path_checks"]=json!(base_paths);
        let manifest=paths.iter().find(|path|path.name=="fixture_manifest_file").and_then(|path|path.descriptor.as_ref());
        let mut candidates=vec![(None,vec![logon.clone()])];
        for (sid,_) in groups {
            if description["dacl_present"]==false || writer_sids.contains(&sid) {candidates.push((Some(sid.clone()),vec![logon.clone(),sid]));}
        }
        let descriptor_complete=description["ace_decode_complete"]==true && description["integrity_label_error"].is_null();
        let path_complete=paths.iter().all(|path|path.descriptor.is_some() && path.description["security"]["ace_decode_complete"]==true && path.description["security"]["integrity_label_error"].is_null()) && discovery_gaps.is_empty();
        let mut rows=Vec::new(); let mut acceptable=Vec::new();
        let mut all_controls=report["base_device_access_check"]["decision"]==true;
        let mut baseline_matches=false;
        for (extra,sids) in candidates {
            let restricted=restrict(base.0,&sids)?; let check_token=impersonation(restricted.0)?;
            let device_check=access(sd.pointer(),check_token.0,DEVICE_ACCESS);
            let (checks,decisions)=path_checks(&paths,check_token.0);
            let read_control=manifest.map(|descriptor|access(descriptor.pointer(),check_token.0,FILE_READ_DATA));
            let controls=read_control.as_ref().is_some_and(|value|value["decision"]==true) && report["base_device_access_check"]["decision"]==true;
            all_controls &= controls;
            if extra.is_none() {baseline_matches=device_check["decision"]==false;}
            let passing=access_screen::passes_screen(device_check["decision"].as_bool(),&decisions,controls && descriptor_complete && path_complete);
            let ambient=if let Some(sid)=&extra {
                // An entirely in-memory outside-directory descriptor demonstrates
                // the cost of an ambient group SID without changing any live ACL.
                let hypothetical=descriptor(&format!("O:SYG:SYD:P(A;;0x001301bf;;;{sid})"))?;
                Some(json!({"kind":"hypothetical outside staging, not a live path","sddl":format!("O:SYG:SYD:P(A;;0x001301bf;;;{sid})"),
                    "create_file_check":access(hypothetical.0,check_token.0,FILE_WRITE_DATA)}))
            } else {None};
            if passing {if let Some(sid)=&extra {acceptable.push(sid.clone());}}
            rows.push(json!({"extra_restricting_sid":extra,"group_name":extra.as_deref().map(group_name),"restricting_sids":sids,
                "token":token_details(restricted.0,FETCH_TOKEN_FLAGS)?,"device_check":device_check,"protected_checks":checks,"positive_manifest_read_control":read_control,
                "passes_requested_descriptor_screen":passing,"ambient_scope_counterexample":ambient}));
        }
        report["candidates"]=json!(rows); report["positive_controls_ok"]=json!(all_controls);
        report["baseline_device_model_matches_prior_native_denial"]=json!(baseline_matches);
        report["screen_complete"]=json!(descriptor_complete && path_complete && all_controls && baseline_matches);
        if !all_controls || !baseline_matches {acceptable.clear();}
        report["candidates_passing_listed_paths"]=json!(acceptable);
        report["recommendation"]=if !descriptor_complete || !path_complete || !all_controls || !baseline_matches {json!("Incomplete/inconsistent descriptor screen: do not accept a candidate or conclude no SID exists; compare query scope, controls and driver policy")}
            else if report["candidates_passing_listed_paths"].as_array().unwrap().is_empty() {json!("No tested enabled account-group SID both grants KsecDD access and denies every listed write right; retain policy and compare AppContainer/lowbox or a documented pre-opened-handle broker design")}
            else {json!("Listed-path candidates exist; see table. Adding an ambient group is NOT a path-independent staging-only boundary. Review the in-memory counterexample and obtain explicit policy approval before any production change")};
        Ok(())
    })();
    if let Err(error)=&collection {report["collection_error"]=json!(error.to_string());report["recommendation"]=json!("Device/descriptor study incomplete; no candidate accepted");}
    write_private(output,&serde_json::to_vec_pretty(&report).map_err(io::Error::other)?,true)?;
    println!("DIAG read-only KsecDD access screen complete={} report={}; this is not a qualification pass",report["screen_complete"],output.display());
    collection
}
