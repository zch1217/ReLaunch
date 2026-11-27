//! Windows login auto-start management via HKCU\Software\Microsoft\Windows\CurrentVersion\Run.

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const VALUE_NAME: &str = "ReLaunch";

/// Format the command line stored in the Run registry value.
pub fn format_command(exe_path: &str) -> String {
    let clean = strip_verbatim_prefix(exe_path.trim());
    format!("\"{clean}\" --minimized")
}

/// Extract the executable path from a Run registry command string.
pub fn extract_exe_path(cmd: &str) -> Option<&str> {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest.find('"')?;
        let inner = &rest[..end];
        if inner.is_empty() {
            None
        } else {
            Some(inner)
        }
    } else {
        let first = trimmed.split(char::is_whitespace).next().unwrap_or("");
        if first.is_empty() {
            None
        } else {
            Some(first)
        }
    }
}

fn strip_verbatim_prefix(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

fn same_exe_path(a: &str, b: &str) -> bool {
    strip_verbatim_prefix(a).eq_ignore_ascii_case(strip_verbatim_prefix(b))
}

/// Path to the currently running executable (without `\\?\` prefix).
pub fn current_exe_path() -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    let s = exe.to_string_lossy();
    Ok(strip_verbatim_prefix(&s).to_string())
}

/// Read the raw command string from HKCU\...\Run\ReLaunch, if present.
#[cfg(target_os = "windows")]
fn query_run_value() -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
        REG_VALUE_TYPE,
    };

    unsafe {
        let wide_key: Vec<u16> = RUN_SUBKEY.encode_utf16().chain([0]).collect();
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_key.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        )
        .is_err()
        {
            return None;
        }

        let wide_name: Vec<u16> = VALUE_NAME.encode_utf16().chain([0]).collect();
        let mut data = vec![0u16; 1024];
        let mut data_size = (data.len() * 2) as u32;
        let mut vtype = REG_VALUE_TYPE::default();
        let res = RegQueryValueExW(
            hkey,
            PCWSTR(wide_name.as_ptr()),
            None,
            Some(&mut vtype),
            Some(data.as_mut_ptr() as _),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);

        if res.is_err() || data_size == 0 {
            return None;
        }

        let len = (data_size as usize) / 2;
        let slice = &data[..len.min(data.len())];
        let text = String::from_utf16_lossy(slice)
            .trim_end_matches('\0')
            .to_string();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn query_run_value() -> Option<String> {
    None
}

/// Return true if auto-start is configured for this executable path.
pub fn is_enabled() -> bool {
    let Some(cmd) = query_run_value() else {
        return false;
    };
    let Some(reg_exe) = extract_exe_path(&cmd) else {
        return false;
    };
    let Ok(cur_exe) = current_exe_path() else {
        return false;
    };
    same_exe_path(reg_exe, &cur_exe)
}

/// Enable or disable HKCU Run auto-start for the current executable.
#[cfg(target_os = "windows")]
pub fn set_enabled(enable: bool) -> anyhow::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let wide_key: Vec<u16> = RUN_SUBKEY.encode_utf16().chain([0]).collect();
    let wide_name: Vec<u16> = VALUE_NAME.encode_utf16().chain([0]).collect();

    if enable {
        let exe = current_exe_path()?;
        let cmd = format_command(&exe);
        let wide_cmd: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
        let bytes = unsafe {
            std::slice::from_raw_parts(wide_cmd.as_ptr() as *const u8, wide_cmd.len() * 2)
        };

        unsafe {
            let mut hkey = HKEY::default();
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(wide_key.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut hkey,
                None,
            )
            .ok()?;

            let set_res = RegSetValueExW(
                hkey,
                PCWSTR(wide_name.as_ptr()),
                0,
                REG_SZ,
                Some(bytes),
            );
            let _ = RegCloseKey(hkey);
            set_res.ok()?;

            // Clear any Task Manager disabled override so auto-start actually takes effect.
            let wide_approved: Vec<u16> = APPROVED_SUBKEY.encode_utf16().chain([0]).collect();
            let mut approved_key = HKEY::default();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(wide_approved.as_ptr()),
                0,
                KEY_SET_VALUE,
                &mut approved_key,
            )
            .is_ok()
            {
                let _ = RegDeleteValueW(approved_key, PCWSTR(wide_name.as_ptr()));
                let _ = RegCloseKey(approved_key);
            }
        }
        Ok(())
    } else {
        unsafe {
            let mut hkey = HKEY::default();
            let open_res = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(wide_key.as_ptr()),
                0,
                KEY_SET_VALUE,
                &mut hkey,
            );
            if open_res == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            open_res.ok()?;

            let del_res = RegDeleteValueW(hkey, PCWSTR(wide_name.as_ptr()));
            let _ = RegCloseKey(hkey);
            if del_res == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            del_res.ok()?;
        }
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
pub fn set_enabled(_enable: bool) -> anyhow::Result<()> {
    anyhow::bail!("Auto-start is only supported on Windows")
}

#[cfg(test)]
mod tests {
    use super::{extract_exe_path, format_command, same_exe_path};

    #[test]
    fn format_command_quotes_path_and_adds_flag() {
        assert_eq!(
            format_command(r"C:\Program Files\ReLaunch\ReLaunch.exe"),
            "\"C:\\Program Files\\ReLaunch\\ReLaunch.exe\" --minimized"
        );
        assert_eq!(
            format_command(r"\\?\E:\AI\ReLaunch\ReLaunch.exe"),
            "\"E:\\AI\\ReLaunch\\ReLaunch.exe\" --minimized"
        );
    }

    #[test]
    fn extract_exe_path_handles_quoted_and_unquoted() {
        assert_eq!(
            extract_exe_path("\"C:\\Program Files\\ReLaunch\\ReLaunch.exe\" --minimized"),
            Some(r"C:\Program Files\ReLaunch\ReLaunch.exe")
        );
        assert_eq!(
            extract_exe_path("\"C:\\ReLaunch.exe\""),
            Some(r"C:\ReLaunch.exe")
        );
        assert_eq!(
            extract_exe_path(r"C:\ReLaunch.exe --minimized"),
            Some(r"C:\ReLaunch.exe")
        );
        assert_eq!(extract_exe_path("   "), None);
        assert_eq!(extract_exe_path("\"\""), None);
    }

    #[test]
    fn same_exe_path_is_case_insensitive_and_strips_prefix() {
        assert!(same_exe_path(
            r"\\?\E:\AI\ReLaunch\ReLaunch.exe",
            r"e:\ai\relaunch\relaunch.exe"
        ));
        assert!(!same_exe_path(
            r"E:\AI\ReLaunch\ReLaunch.exe",
            r"C:\Users\me\AppData\Local\Programs\ReLaunch\ReLaunch.exe"
        ));
    }
}
