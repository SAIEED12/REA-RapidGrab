//! Compiles `assets/app.rc` into a resource object and links it into the
//! binary, so Explorer, the taskbar and the Desktop shortcut show the RapidGrab
//! logo instead of the generic executable icon.
//!
//! Only runs on Windows targets; if `rc.exe` cannot be found the build still
//! succeeds (the window and header logo come from `src/icon.rs` regardless).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../assets/app.rc");
    println!("cargo:rerun-if-changed=../../assets/logo.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let assets = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let res_path = out_dir.join("rapidgrab.res");

    let Some(rc_exe) = find_rc() else {
        println!(
            "cargo:warning=rc.exe not found; rapidgrab-gui will build without an embedded \
             executable icon (window and header icons are unaffected)"
        );
        return;
    };

    let status = Command::new(&rc_exe)
        .current_dir(&assets)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg("app.rc")
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg={}", res_path.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe failed with {s}; no executable icon embedded"),
        Err(e) => println!("cargo:warning=could not run {}: {e}", rc_exe.display()),
    }
}

/// Locates `rc.exe`, preferring `RC_EXE`, then the newest Windows SDK x64 kit.
fn find_rc() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RC_EXE") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }

    let mut kits: Vec<PathBuf> = Vec::new();
    for root in [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ] {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let candidate = entry.path().join("x64").join("rc.exe");
            if candidate.is_file() {
                kits.push(candidate);
            }
        }
    }

    // Version directories sort correctly as strings (10.0.26100.0 > 10.0.22621.0).
    kits.sort();
    kits.pop()
}
