use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use cef::types::list::{List, ValueType};
use messages::packets::EventValue;
use winapi::shared::minwindef::{DWORD, HKEY, LPBYTE};
use winapi::shared::winerror::ERROR_SUCCESS;
use winapi::um::winnt::{KEY_READ, REG_DWORD, REG_EXPAND_SZ, REG_SZ};
use winapi::um::winreg::{HKEY_CURRENT_USER, RegCloseKey, RegOpenKeyExW, RegQueryValueExW};

const REGISTRY_PATH: &str = "SOFTWARE\\OnlineRPLauncher";
const AUTOLOGIN_ENABLED_VALUE: &str = "AutologinEnabled";
const AUTOLOGIN_PASSWORD_VALUE: &str = "AutologinPassword";
const SHOW_PLAYER_DIALOG_ARG_COUNT: usize = 2;
const DIALOG_STYLE_PASSWORD: i32 = 3;
pub const REQUEST_EVENT: &str = "orp:autologin:request";
pub const DATA_EVENT: &str = "orp:autologin:data";

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
}

pub fn try_build_login_response(arguments: &[EventValue]) -> Option<String> {
    let password = read_autologin_password()?;
    build_login_response(arguments, &password)
}

pub fn try_build_login_response_from_cef_list(list: &List) -> Option<String> {
    let password = read_autologin_password()?;
    build_login_response_from_cef_list(list, &password)
}

fn build_login_response(arguments: &[EventValue], password: &str) -> Option<String> {
    if arguments.len() < SHOW_PLAYER_DIALOG_ARG_COUNT {
        return None;
    }

    let dialog_id = event_value_as_i32(arguments.first()?)?;
    let dialog_style = event_value_as_i32(arguments.get(1)?)?;

    if dialog_style != DIALOG_STYLE_PASSWORD {
        return None;
    }

    Some(format!("{dialog_id} 1 -1 {password}"))
}

fn build_login_response_from_cef_list(list: &List, password: &str) -> Option<String> {
    if list.len() < SHOW_PLAYER_DIALOG_ARG_COUNT {
        return None;
    }

    let dialog_id = cef_list_value_as_i32(list, 0)?;
    let dialog_style = cef_list_value_as_i32(list, 1)?;

    if dialog_style != DIALOG_STYLE_PASSWORD {
        return None;
    }

    Some(format!("{dialog_id} 1 -1 {password}"))
}

fn event_value_as_i32(value: &EventValue) -> Option<i32> {
    value
        .integer_value
        .or_else(|| value.float_value.map(|value| value as i32))
        .or_else(|| value.string_value.as_ref()?.parse().ok())
}

fn cef_list_value_as_i32(list: &List, index: usize) -> Option<i32> {
    match list.get_type(index) {
        ValueType::Integer => Some(list.integer(index)),
        ValueType::Double => Some(list.double(index) as i32),
        ValueType::String => list.string(index).to_string().parse().ok(),
        _ => None,
    }
}

pub fn read_autologin_password() -> Option<String> {
    let key = open_launcher_key()?;

    if read_dword_value(&key, AUTOLOGIN_ENABLED_VALUE)? == 0 {
        return None;
    }

    let password = read_string_value(&key, AUTOLOGIN_PASSWORD_VALUE)?;

    if password.is_empty() {
        None
    } else {
        Some(password)
    }
}

fn open_launcher_key() -> Option<RegistryKey> {
    let path = wide_null(REGISTRY_PATH);
    let mut key: HKEY = ptr::null_mut();

    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut key) };

    (status == ERROR_SUCCESS as i32).then_some(RegistryKey(key))
}

fn read_dword_value(key: &RegistryKey, name: &str) -> Option<DWORD> {
    let name = wide_null(name);
    let mut value_type: DWORD = 0;
    let mut value: DWORD = 0;
    let mut value_size = std::mem::size_of::<DWORD>() as DWORD;

    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null_mut(),
            &mut value_type,
            &mut value as *mut DWORD as LPBYTE,
            &mut value_size,
        )
    };

    (status == ERROR_SUCCESS as i32 && value_type == REG_DWORD).then_some(value)
}

fn read_string_value(key: &RegistryKey, name: &str) -> Option<String> {
    let name = wide_null(name);
    let mut value_type: DWORD = 0;
    let mut buffer = [0u16; 256];
    let mut value_size = (buffer.len() * std::mem::size_of::<u16>()) as DWORD;

    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null_mut(),
            &mut value_type,
            buffer.as_mut_ptr() as LPBYTE,
            &mut value_size,
        )
    };

    if status != ERROR_SUCCESS as i32 || !matches!(value_type, REG_SZ | REG_EXPAND_SZ) {
        return None;
    }

    let wchar_count = (value_size as usize / std::mem::size_of::<u16>()).min(buffer.len());
    let terminator = buffer
        .iter()
        .take(wchar_count)
        .position(|&ch| ch == 0)
        .unwrap_or(wchar_count);

    Some(String::from_utf16_lossy(&buffer[..terminator]))
}

fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    fn int_arg(value: i32) -> EventValue<'static> {
        EventValue {
            integer_value: Some(value),
            ..Default::default()
        }
    }

    fn string_arg(value: &'static str) -> EventValue<'static> {
        EventValue {
            string_value: Some(Cow::Borrowed(value)),
            ..Default::default()
        }
    }

    #[test]
    fn builds_response_for_login_password_dialog() {
        let args = [
            int_arg(12),
            int_arg(3),
            string_arg("auth"),
            string_arg("any text"),
        ];

        assert_eq!(
            build_login_response(&args, "secret"),
            Some("12 1 -1 secret".to_string())
        );
    }

    #[test]
    fn ignores_non_password_dialog() {
        let args = [
            int_arg(12),
            int_arg(0),
            string_arg("auth"),
            string_arg("any text"),
        ];

        assert_eq!(build_login_response(&args, "secret"), None);
    }
}
