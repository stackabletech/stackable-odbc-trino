//! Embeds a Windows `VERSIONINFO` resource in the driver DLL.
//!
//! The ODBC Data Source Administrator reads its **Version** and **Company**
//! columns from the driver file's version resource, and prints `Not marked`
//! for a file that carries none — which is every Rust `cdylib`, since rustc
//! emits no such resource. Measured on Windows Server 2022: `sqlsrv32.dll`
//! lists as `10.00.20348.01` / `Microsoft Corporation` and carries exactly
//! those two strings.
//!
//! Nothing here is hand-maintained. Every string comes from `Cargo.toml`
//! through cargo's own environment, so the resource cannot disagree with the
//! package — in particular the version, which follows `CARGO_PKG_VERSION` and
//! therefore follows whatever `cargo-release` wrote into `Cargo.toml`. That is
//! why `release.toml` has no rule for this file, unlike
//! `connector/StackableTrinoODBC.pq`, which carries its own version literal
//! and has nothing deriving it.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    println!("cargo:rerun-if-env-changed=WINDRES");

    // `CARGO_CFG_TARGET_OS`, never `cfg!(windows)`: a build script is compiled
    // for and run on the *host*, so `cfg!(windows)` describes the machine doing
    // the building. The release DLL is cross-compiled from Linux, where it is
    // false — it would skip the resource on precisely the build that ships.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Only the GNU toolchain is wired up. An MSVC target needs `rc.exe` from a
    // Visual Studio installation, which this repo never builds with:
    // `.github/workflows/release.yaml` installs `gcc-mingw-w64-x86-64` and
    // builds `x86_64-pc-windows-gnu`. Warn rather than fail, so a local MSVC
    // build still works — it just produces a DLL the Administrator lists as
    // `Not marked`, which is what every such build did before this existed.
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env != "gnu" {
        println!(
            "cargo:warning=no version resource embedded: the {target_env} Windows toolchain \
             needs rc.exe, and only the gnu toolchain's windres is wired up in build.rs. \
             The ODBC Data Source Administrator will list this driver as \"Not marked\"."
        );
        return;
    }

    let out_dir = PathBuf::from(
        std::env::var("OUT_DIR").expect("cargo always sets OUT_DIR for a build script"),
    );
    let rc_path = out_dir.join("version.rc");
    let obj_path = out_dir.join("version.o");

    if let Err(e) = std::fs::write(&rc_path, version_rc()) {
        fail(&format!("could not write {}: {e}", rc_path.display()));
    }

    let windres = windres_command();
    let status = Command::new(&windres)
        .arg("--input")
        .arg(&rc_path)
        .arg("--output")
        .arg(&obj_path)
        // COFF, so the result is an object file the linker takes like any
        // other. windres defaults to emitting an `.rc` back out.
        .arg("--output-format=coff")
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => fail(&format!(
            "`{windres}` failed with {s} on {}",
            rc_path.display()
        )),
        Err(e) => fail(&format!(
            "could not run `{windres}`: {e}\n\
             A Windows build needs windres to embed the driver's version resource. \
             On Debian and Ubuntu it is in binutils-mingw-w64-x86-64, which \
             gcc-mingw-w64-x86-64 already depends on. Set WINDRES to override the name."
        )),
    }

    // `-cdylib`, not the unsuffixed form: the resource belongs to the shipped
    // DLL, and the unsuffixed flag would also be handed to the linker for every
    // test and benchmark binary.
    println!("cargo:rustc-link-arg-cdylib={}", obj_path.display());
}

/// Abort the build with a message.
///
/// `exit` rather than `panic!`, which the crate's clippy configuration denies:
/// cargo renders a build script's stderr and its exit status as a build error
/// either way, and this one carries no backtrace nobody would read.
fn fail(message: &str) -> ! {
    eprintln!("error: {message}");
    std::process::exit(1);
}

/// The `windres` to invoke.
///
/// The cross-prefixed name first, because that is what a Linux host has: the
/// bare `windres` there, if it exists at all, targets the host. `WINDRES`
/// overrides both, for a toolchain under a different prefix.
fn windres_command() -> String {
    if let Ok(explicit) = std::env::var("WINDRES") {
        return explicit;
    }
    let prefixed = "x86_64-w64-mingw32-windres";
    if Command::new(prefixed).arg("--version").output().is_ok() {
        return prefixed.to_string();
    }
    "windres".to_string()
}

/// The resource script, built entirely from cargo's environment.
fn version_rc() -> String {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let major = env_num("CARGO_PKG_VERSION_MAJOR");
    let minor = env_num("CARGO_PKG_VERSION_MINOR");
    let patch = env_num("CARGO_PKG_VERSION_PATCH");
    let description = std::env::var("CARGO_PKG_DESCRIPTION").unwrap_or_default();
    let license = std::env::var("CARGO_PKG_LICENSE").unwrap_or_default();
    let company = company_name();

    // The lib name is the package name with hyphens replaced, which is what
    // both the `.so` and the `.dll` are named after.
    let file_name = format!(
        "{}.dll",
        std::env::var("CARGO_PKG_NAME")
            .unwrap_or_default()
            .replace('-', "_")
    );

    // Literal numeric constants rather than `#include <windows.h>`, so the
    // script does not depend on the mingw headers being on windres's include
    // path: VOS_NT_WINDOWS32 (0x40004) and VFT_DLL (0x2).
    //
    // The 040904b0 block is US English, Unicode, and the VarFileInfo
    // translation below must name the same pair or the strings are ignored.
    format!(
        r#"1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004L
FILETYPE 0x2L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName",      "{company}"
            VALUE "FileDescription",  "{description}"
            VALUE "FileVersion",      "{version}"
            VALUE "InternalName",     "{file_name}"
            VALUE "LegalCopyright",   "Copyright the {company} authors. Licensed under {license}."
            VALUE "OriginalFilename", "{file_name}"
            VALUE "ProductName",      "{description}"
            VALUE "ProductVersion",   "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#,
        company = rc_escape(&company),
        description = rc_escape(&description),
        license = rc_escape(&license),
        version = rc_escape(&version),
        file_name = rc_escape(&file_name),
    )
}

/// `CARGO_PKG_AUTHORS` without the address, so `Cargo.toml`'s
/// `Stackable GmbH <info@stackable.tech>` becomes the company the ODBC
/// Administrator shows. Only the first author: the column holds one name.
fn company_name() -> String {
    let authors = std::env::var("CARGO_PKG_AUTHORS").unwrap_or_default();
    let first = authors.split(':').next().unwrap_or_default();
    first
        .split('<')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn env_num(key: &str) -> u16 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Quote and backslash are the two characters an `.rc` string cannot carry
/// raw. None of the values used here contains either today; escaping them
/// anyway keeps a future `description` from producing an unparseable script.
fn rc_escape(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', r#"\""#)
}
