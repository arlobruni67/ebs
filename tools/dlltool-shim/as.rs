// Minimal GNU `as` stand-in for dlltool: `as [--64] [-o out] in.s` -> zig cc -c.
use std::process::{exit, Command};
fn main() {
    let (mut out, mut input) = (None, None);
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "-o" { out = it.next(); } else if a.starts_with('-') { } else { input = Some(a); }
    }
    let (Some(o), Some(i)) = (out, input) else { eprintln!("as-shim: need -o and input"); exit(1) };
    let zig = r"D:\tools\zig-x86_64-windows-0.17.0\zig.exe";
    let st = Command::new(zig).args(["cc", "-target", "x86_64-windows-gnu", "-c", "-x", "assembler", &i, "-o", &o]).status().unwrap_or_else(|e| { eprintln!("zig: {e}"); exit(1) });
    exit(st.code().unwrap_or(1));
}
