//! Prints the call stack of every thread of a running process (diagnosing hangs).
//! Usage: stackdump <pid> [exe-path-for-dwarf]
//! System DLL frames are named from their exports (dbghelp); frames in the given exe are resolved
//! to Rust function + file:line from its DWARF line tables (GNU toolchain).

use std::ffi::c_void;
use windows::Win32::Foundation::*;
use windows::Win32::System::Diagnostics::Debug::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Threading::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pid: u32 = args.get(1).and_then(|a| a.parse().ok()).expect("usage: stackdump <pid> [exe]");
    let exe = args.get(2).cloned();
    let loader = exe.as_ref().and_then(|p| addr2line::Loader::new(p).ok());
    let image_base = exe
        .as_ref()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|d| object::File::parse(&*d).ok().map(|f| object::Object::relative_address_base(&f)))
        .unwrap_or(0);
    unsafe {
        let proc = OpenProcess(PROCESS_ALL_ACCESS, false, pid).expect("OpenProcess");
        SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS);
        SymInitializeW(proc, None, true).expect("SymInitialize");
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).expect("snapshot");
        let mut te = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut ok = Thread32First(snap, &mut te).is_ok();
        while ok {
            if te.th32OwnerProcessID == pid {
                dump_thread(proc, te.th32ThreadID, loader.as_ref(), image_base);
            }
            ok = Thread32Next(snap, &mut te).is_ok();
        }
        let _ = CloseHandle(snap);
    }
}

unsafe fn dump_thread(proc: HANDLE, tid: u32, loader: Option<&addr2line::Loader>, image_base: u64) {
    let Ok(th) = OpenThread(THREAD_ALL_ACCESS, false, tid) else { return };
    let name = GetThreadDescription(th).ok().and_then(|p| p.to_string().ok()).unwrap_or_default();
    println!("=== thread {tid} {name}");
    SuspendThread(th);
    let mut ctx: CONTEXT = std::mem::zeroed();
    ctx.ContextFlags = CONTEXT_FULL_AMD64;
    if GetThreadContext(th, &mut ctx).is_ok() {
        let mut frame: STACKFRAME64 = std::mem::zeroed();
        frame.AddrPC = ADDRESS64 { Offset: ctx.Rip, Segment: 0, Mode: AddrModeFlat };
        frame.AddrFrame = ADDRESS64 { Offset: ctx.Rbp, Segment: 0, Mode: AddrModeFlat };
        frame.AddrStack = ADDRESS64 { Offset: ctx.Rsp, Segment: 0, Mode: AddrModeFlat };
        for _ in 0..48 {
            let more = StackWalk64(
                0x8664,
                proc,
                th,
                &mut frame,
                &mut ctx as *mut _ as *mut c_void,
                None,
                Some(fta),
                Some(gmb),
                None,
            );
            if !more.as_bool() || frame.AddrPC.Offset == 0 {
                break;
            }
            println!("  {}", describe(proc, frame.AddrPC.Offset, loader, image_base));
        }
    }
    ResumeThread(th);
    let _ = CloseHandle(th);
}

unsafe fn describe(proc: HANDLE, addr: u64, loader: Option<&addr2line::Loader>, image_base: u64) -> String {
    let base = SymGetModuleBase64(proc, addr);
    let mut mi = IMAGEHLP_MODULEW64 { SizeOfStruct: std::mem::size_of::<IMAGEHLP_MODULEW64>() as u32, ..Default::default() };
    let module = if SymGetModuleInfoW64(proc, addr, &mut mi).is_ok() {
        String::from_utf16_lossy(&mi.ModuleName).trim_end_matches('\0').to_string()
    } else {
        "?".into()
    };
    if module.eq_ignore_ascii_case("EBS") || module.eq_ignore_ascii_case("BETTERBS") {
        if let Some(l) = loader {
            let probe = addr - base + image_base;
            let mut out = Vec::new();
            if let Ok(mut frames) = l.find_frames(probe) {
                while let Ok(Some(f)) = frames.next() {
                    let func = f.function.as_ref().and_then(|n| n.demangle().ok()).map(|c| c.into_owned()).unwrap_or_default();
                    let loc = f.location.map(|l| format!("{}:{}", l.file.unwrap_or("?").rsplit(['/', '\\']).next().unwrap_or("?"), l.line.unwrap_or(0))).unwrap_or_default();
                    out.push(format!("{func} ({loc})"));
                }
            }
            if !out.is_empty() {
                return format!("{module}!{}", out.join(" <- inlined in "));
            }
        }
        return format!("{module}+0x{:x}", addr - base);
    }
    let mut buf = vec![0u8; std::mem::size_of::<SYMBOL_INFOW>() + 512];
    let sym = buf.as_mut_ptr() as *mut SYMBOL_INFOW;
    (*sym).SizeOfStruct = std::mem::size_of::<SYMBOL_INFOW>() as u32;
    (*sym).MaxNameLen = 250;
    let mut disp = 0u64;
    if SymFromAddrW(proc, addr, Some(&mut disp), sym).is_ok() {
        let n = std::slice::from_raw_parts((*sym).Name.as_ptr(), (*sym).NameLen as usize);
        format!("{module}!{}+0x{disp:x}", String::from_utf16_lossy(n))
    } else {
        format!("{module}+0x{:x}", addr - base)
    }
}

unsafe extern "system" fn fta(p: HANDLE, a: u64) -> *mut c_void {
    SymFunctionTableAccess64(p, a)
}

unsafe extern "system" fn gmb(p: HANDLE, a: u64) -> u64 {
    SymGetModuleBase64(p, a)
}
