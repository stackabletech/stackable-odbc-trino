//! The driver's DSN setup dialog, behind
//! [`Backend::configure_dsn`](stackable_odbc_core::backend::Backend::configure_dsn).
//!
//! Core owns all of `ConfigDSN`: validating *fRequest*, rejecting `DRIVER=`,
//! merging the data source's stored keywords in, calling `SQLValidDSN` and
//! writing through `SQLWriteDSNToIni`. This module supplies the one thing that
//! varies per driver, which is asking a person which keywords the data source
//! needs.
//!
//! The dialog itself is `packaging/windows/configure-dsn.ps1`, run with
//! `-Emit`, which prints the keywords it collected instead of writing them.
//! Reusing the script is what keeps one list of keywords: its `$Fields` table
//! names all 34 of them, and `dsn_keys_match_the_connection_string_parser` in
//! `src/lib.rs` fails the build if that table and the parser disagree. A
//! second dialog written in Rust would be a second list, checked by nothing.
//!
//! Only the two OS calls are `#[cfg(windows)]`; every decision is a plain
//! function with unit tests that run on Linux.

use std::collections::HashMap;

use stackable_odbc_core::setup::{ConfigRequest, SetupError};

/// The dialog script, looked for beside the driver's own DLL.
///
/// `install.bat` must copy it there, as a hard requirement rather than
/// best-effort: without it the Administrator's **Add…** and **Configure…**
/// buttons have no dialog to run, and every such request fails.
///
/// The `cfg_attr`s below, here and on the three functions that follow, are
/// core's own idiom for the parts of `ConfigDSN` that only Windows reaches:
/// they stay compiled and unit-tested everywhere, so a change breaks the build
/// on the platform this is developed on rather than on the one it ships to.
#[cfg_attr(not(windows), allow(dead_code))]
const DIALOG_SCRIPT: &str = "configure-dsn.ps1";

/// The dialog collected keywords: its stdout is the JSON map.
#[cfg_attr(not(windows), allow(dead_code))]
const EXIT_ACCEPTED: i32 = 0;
/// The user cancelled. Not a failure, so `ConfigDSN` posts no installer error.
#[cfg_attr(not(windows), allow(dead_code))]
const EXIT_CANCELLED: i32 = 2;

/// Whether this call is allowed to put a dialog on the screen.
///
/// Two things say no:
///
/// - **A null `hwndParent`.** The spec is explicit: "The function will not
///   display any dialog boxes if the handle is null." It is also what keeps
///   this from recursing. `configure-dsn.ps1`, run standalone, writes its data
///   source through `SQLConfigDataSourceW` with a null *hwndParent*, which
///   re-enters this hook. This rule makes that re-entry headless, so the
///   script is not asked to launch itself.
/// - **`Remove`.** The Administrator has already asked the user to confirm the
///   deletion, and this driver keeps nothing outside `ODBC.INI` that a removal
///   would need to clean up: no cached token, no keytab. A second confirmation
///   would only be a second chance to answer the same question differently.
///
/// `Add` and `Config` prompt. Everything else passes the attributes through
/// unchanged, which is exactly core's defaulted behaviour.
fn dialog_needed(hwnd_is_null: bool, request: ConfigRequest) -> bool {
    if hwnd_is_null {
        return false;
    }
    match request {
        ConfigRequest::Add | ConfigRequest::Config => true,
        ConfigRequest::Remove => false,
    }
}

/// The attribute map, as the dialog reads it on stdin.
///
/// A pipe rather than a temp file, because a `Config` request arrives with the
/// data source's whole stored section merged in by core, including `PWD` and
/// whatever else `sensitive_connect_keywords` names. Those are already in the
/// registry unencrypted; putting them in a second place, with a filename any
/// other process on the machine can guess, would be a new exposure rather than
/// the same one.
#[cfg_attr(not(windows), allow(dead_code))]
fn encode_attributes(attributes: &HashMap<String, String>) -> Result<String, SetupError> {
    serde_json::to_string(attributes).map_err(|e| {
        // No value in the message: it goes to the installer error buffer and
        // the ODBC Administrator displays it.
        SetupError::request_failed(format!(
            "could not encode the data source's keywords for the setup dialog: {e}"
        ))
    })
}

/// What the dialog decided, read back from its exit code and stdout.
///
/// The exit code carries the verdict because stdout carries the payload. A
/// dialog cannot report "cancelled" in-band without inventing a sentinel that
/// some future keyword value could collide with.
#[cfg_attr(not(windows), allow(dead_code))]
fn interpret_outcome(
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<Option<HashMap<String, String>>, SetupError> {
    match code {
        Some(EXIT_ACCEPTED) => {
            let attrs: HashMap<String, String> =
                serde_json::from_str(stdout.trim()).map_err(|e| {
                    SetupError::request_failed(format!(
                        "the setup dialog returned something that is not a keyword list: {e}"
                    ))
                })?;
            Ok(Some(attrs))
        }
        Some(EXIT_CANCELLED) => Ok(None),
        other => {
            // PowerShell writes a terminating error to stderr and exits 1. Pass
            // it on: it is the only account of what went wrong, and without it
            // the Administrator shows a bare "could not perform the operation".
            let detail = stderr.trim();
            let detail = if detail.is_empty() {
                "it reported no reason".to_string()
            } else {
                detail.to_string()
            };
            let how = match other {
                Some(c) => format!("exited with code {c}"),
                None => "was terminated by a signal".to_string(),
            };
            Err(SetupError::request_failed(format!(
                "the setup dialog {how}: {detail}"
            )))
        }
    }
}

/// Present the dialog and return the keywords it collected.
///
/// See [`Backend::configure_dsn`](stackable_odbc_core::backend::Backend::configure_dsn)
/// for the contract this satisfies.
pub(super) fn configure_dsn(
    hwnd_parent: *mut std::ffi::c_void,
    request: ConfigRequest,
    attributes: HashMap<String, String>,
) -> Result<Option<HashMap<String, String>>, SetupError> {
    // Keyword names only, never values: this map routinely carries `PWD`.
    tracing::debug!(
        ?request,
        headless = hwnd_parent.is_null(),
        keywords = attributes.len(),
        "TrinoBackend::configure_dsn"
    );

    if !dialog_needed(hwnd_parent.is_null(), request) {
        return Ok(Some(attributes));
    }

    #[cfg(windows)]
    {
        let payload = encode_attributes(&attributes)?;
        let (code, stdout, stderr) = windows::run_dialog(&payload)?;
        interpret_outcome(code, &stdout, &stderr)
    }
    #[cfg(not(windows))]
    {
        // `ConfigDSNW` is a Windows export and core does not build it
        // elsewhere, so this is unreachable rather than a gap. Passing the
        // attributes through is what the caller would have got anyway.
        tracing::warn!(
            "ConfigDSN asked for a setup dialog, which this driver only has on \
             Windows; proceeding with the keywords as supplied"
        );
        Ok(Some(attributes))
    }
}

#[cfg(windows)]
mod windows {
    //! Finding the dialog and running it. The only two OS calls in the module.

    use std::ffi::{OsString, c_void};
    use std::io::Write as _;
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::process::CommandExt as _;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use stackable_odbc_core::setup::SetupError;

    use super::DIALOG_SCRIPT;

    /// `GetModuleHandleExW` flags, from `libloaderapi.h`. Together they mean
    /// "the module containing this address, without taking a reference".
    /// Taking a reference here would pin the driver DLL in the
    /// Administrator's process for its lifetime.
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x0000_0002;
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;

    /// `CreateProcess`'s flag from `winbase.h`, so PowerShell does not flash a
    /// console window over the Administrator for as long as the dialog is up.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // Two functions from kernel32, which is the operating system rather than a
    // dependency, the same reason the Windows SBOM declares no import of it.
    // Declaring them here rather than taking `windows-sys` keeps the driver's
    // dependency graph, and so its SBOM, unchanged by a setup dialog.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleExW(
            dw_flags: u32,
            lp_module_name: *const u16,
            ph_module: *mut *mut c_void,
        ) -> i32;
        fn GetModuleFileNameW(h_module: *mut c_void, lp_filename: *mut u16, n_size: u32) -> u32;
    }

    /// The directory holding this DLL, identified from an address inside the
    /// module. This function's own address is one.
    ///
    /// `std::env::current_exe()` answers with the Administrator's path, since
    /// `ConfigDSN` runs inside `odbcad32.exe`.
    fn driver_directory() -> Result<PathBuf, SetupError> {
        let mut module: *mut c_void = std::ptr::null_mut();
        // SAFETY: the flags are the documented pair for an address lookup, the
        // address is this function's own and so certainly inside the module,
        // and `module` is a live out-pointer for the duration of the call.
        let ok = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                driver_directory as *const u16,
                &raw mut module,
            )
        };
        if ok == 0 {
            return Err(SetupError::request_failed(
                "could not identify the driver DLL's own module".to_string(),
            ));
        }

        // MAX_PATH is not a limit on a path, only on the buffer most callers
        // pass, so grow until the name fits rather than truncating it. A
        // truncated path would name a directory that does not exist, and the
        // failure would read as a missing script.
        let mut buf = vec![0u16; 260];
        loop {
            // SAFETY: `buf` is a live allocation of `buf.len()` u16s, which is
            // exactly what the length argument claims.
            let len = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) };
            if len == 0 {
                return Err(SetupError::request_failed(
                    "could not read the driver DLL's own path".to_string(),
                ));
            }
            if (len as usize) < buf.len() {
                buf.truncate(len as usize);
                break;
            }
            buf.resize(buf.len() * 2, 0);
        }

        let path = PathBuf::from(OsString::from_wide(&buf));
        path.parent().map(PathBuf::from).ok_or_else(|| {
            SetupError::request_failed(format!(
                "the driver DLL's path has no directory: {}",
                path.display()
            ))
        })
    }

    /// Run the dialog, feeding it `payload` on stdin.
    ///
    /// Returns the exit code and both output streams; deciding what they mean
    /// is [`super::interpret_outcome`]'s job.
    pub(super) fn run_dialog(payload: &str) -> Result<(Option<i32>, String, String), SetupError> {
        let script = driver_directory()?.join(DIALOG_SCRIPT);
        if !script.is_file() {
            // Naming the path is the whole value of this error: the usual
            // cause is a DLL registered from wherever it was unzipped, with
            // the rest of the archive left behind.
            return Err(SetupError::request_failed(format!(
                "the setup dialog {DIALOG_SCRIPT} was not found beside the driver \
                 (looked for {}). Reinstall with install.bat, which places both together.",
                script.display()
            )));
        }

        let mut child = Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .arg("-Emit")
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                SetupError::request_failed(format!("could not run the setup dialog: {e}"))
            })?;

        // Scoped so the pipe closes before the wait below. PowerShell's
        // `[Console]::In.ReadToEnd()` does not return until it does, and this
        // process does not read stdout until the wait, so leaving it open
        // deadlocks both sides.
        {
            let mut stdin = child.stdin.take().ok_or_else(|| {
                SetupError::request_failed("the setup dialog's stdin was not available".to_string())
            })?;
            stdin.write_all(payload.as_bytes()).map_err(|e| {
                SetupError::request_failed(format!(
                    "could not send the data source's keywords to the setup dialog: {e}"
                ))
            })?;
        }

        let out = child.wait_with_output().map_err(|e| {
            SetupError::request_failed(format!("the setup dialog could not be waited on: {e}"))
        })?;

        Ok((
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A null `hwndParent` must never prompt, for every request. The spec says
    /// so, and it is also what stops `configure-dsn.ps1`'s own
    /// `SQLConfigDataSourceW` write, which passes a null handle, from
    /// re-entering this hook and launching the script a second time.
    #[test]
    fn a_null_parent_window_never_prompts() {
        for request in [
            ConfigRequest::Add,
            ConfigRequest::Config,
            ConfigRequest::Remove,
        ] {
            assert!(
                !dialog_needed(true, request),
                "{request:?} with a null hwndParent must not display a dialog"
            );
        }
    }

    /// Add and Configure prompt; Remove does not. The Administrator confirms a
    /// removal itself, and this driver has nothing outside `ODBC.INI` to clean.
    #[test]
    fn only_add_and_config_prompt() {
        assert!(dialog_needed(false, ConfigRequest::Add));
        assert!(dialog_needed(false, ConfigRequest::Config));
        assert!(!dialog_needed(false, ConfigRequest::Remove));
    }

    #[test]
    fn attributes_survive_the_exchange() {
        let mut attrs = HashMap::new();
        attrs.insert("DSN".to_string(), "trino_prod".to_string());
        attrs.insert("Host".to_string(), "trino.example.com".to_string());
        // A value carrying the characters that would break a command line or a
        // `key=value` line, which is why the exchange is JSON over a pipe.
        attrs.insert(
            "SessionProperties".to_string(),
            "query_max_run_time:10m;example.foo:bar \"quoted\"".to_string(),
        );

        let encoded = encode_attributes(&attrs).expect("a string map encodes");
        let decoded = interpret_outcome(Some(EXIT_ACCEPTED), &encoded, "")
            .expect("exit 0 with a keyword list is an acceptance")
            .expect("an acceptance carries a map");
        assert_eq!(decoded, attrs);
    }

    /// Cancelling is `Ok(None)`, which core turns into FALSE with no installer
    /// error posted. An `Err` here would put "could not perform the operation"
    /// in front of a user who changed their mind.
    #[test]
    fn cancelling_is_not_a_failure() {
        let outcome = interpret_outcome(Some(EXIT_CANCELLED), "", "")
            .expect("a cancelled dialog is not an error");
        assert_eq!(outcome, None);
    }

    /// PowerShell exits 1 on a terminating error, having written the reason to
    /// stderr. That reason is the only account of what went wrong, so it has to
    /// reach the message core posts.
    #[test]
    fn a_failing_dialog_reports_its_stderr() {
        let err = interpret_outcome(Some(1), "", "Set-StrictMode: variable is not set\n")
            .expect_err("a non-zero, non-cancel exit is a failure");
        assert!(
            err.message.contains("variable is not set"),
            "the dialog's own reason must survive into the installer error: {}",
            err.message
        );
        assert!(
            err.message.contains("exited with code 1"),
            "the exit code belongs in the message too: {}",
            err.message
        );
    }

    /// A dialog that exits 0 but prints something else is a failure, not an
    /// empty data source. Accepting it would write a data source with no
    /// keywords at all, which fails much later and much less clearly.
    #[test]
    fn an_unreadable_reply_is_a_failure() {
        let err = interpret_outcome(Some(EXIT_ACCEPTED), "not json at all", "")
            .expect_err("a reply that is not a keyword list cannot be written");
        assert!(
            err.message.contains("not a keyword list"),
            "unexpected message: {}",
            err.message
        );
    }

    /// A dialog killed by a signal has no exit code, and must not be mistaken
    /// for either an acceptance or a cancellation.
    #[test]
    fn a_killed_dialog_is_a_failure() {
        let err =
            interpret_outcome(None, "", "").expect_err("no exit code cannot be read as a verdict");
        assert!(
            err.message.contains("terminated by a signal"),
            "unexpected message: {}",
            err.message
        );
    }
}
