// rustc's raw-dylib import libraries via GNU dlltool (long-format libs GNU ld handles correctly), with
// `as` replaced by as-shim.exe (zig's integrated assembler), since this toolchain ships no assembler.
use std::process::{exit, Command};
fn main() {
    let dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let gnu = r"D:\tools\rustup\toolchains\stable-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained\dlltool.exe";
    let st = Command::new(gnu).arg("-S").arg(dir.join("as-shim.exe")).args(std::env::args().skip(1)).status().unwrap_or_else(|e| { eprintln!("dlltool: {e}"); exit(1) });
    exit(st.code().unwrap_or(1));
}
