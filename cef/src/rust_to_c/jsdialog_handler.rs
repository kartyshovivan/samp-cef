use crate::rust_to_c::Wrapper;
use crate::types::string::CefString;

use cef_sys::{
    cef_browser_t, cef_jsdialog_callback_t, cef_jsdialog_handler_t, cef_jsdialog_type_t,
    cef_string_t,
};

unsafe fn continue_dialog(
    callback: *mut cef_jsdialog_callback_t, success: bool, input: &CefString,
) {
    if callback.is_null() {
        return;
    }

    if let Some(cont) = unsafe { (*callback).cont } {
        unsafe {
            cont(
                callback,
                if success { 1 } else { 0 },
                input.as_cef_string() as *const cef_string_t,
            );
        }
    }
}

unsafe extern "system" fn on_jsdialog(
    _this: *mut cef_jsdialog_handler_t, _browser: *mut cef_browser_t,
    _origin_url: *const cef_string_t, dialog_type: cef_jsdialog_type_t::Type,
    _message_text: *const cef_string_t, _default_prompt_text: *const cef_string_t,
    callback: *mut cef_jsdialog_callback_t, suppress_message: *mut ::std::os::raw::c_int,
) -> ::std::os::raw::c_int {
    if !suppress_message.is_null() {
        unsafe {
            *suppress_message = 1;
        }
    }

    let input = CefString::new_empty();
    let success = dialog_type != cef_jsdialog_type_t::JSDIALOGTYPE_CONFIRM;
    unsafe {
        continue_dialog(callback, success, &input);
    }

    1
}

unsafe extern "system" fn on_before_unload_dialog(
    _this: *mut cef_jsdialog_handler_t, _browser: *mut cef_browser_t,
    _message_text: *const cef_string_t, _is_reload: ::std::os::raw::c_int,
    callback: *mut cef_jsdialog_callback_t,
) -> ::std::os::raw::c_int {
    let input = CefString::new_empty();
    unsafe {
        continue_dialog(callback, true, &input);
    }

    1
}

unsafe extern "system" fn on_reset_dialog_state(
    _this: *mut cef_jsdialog_handler_t, _browser: *mut cef_browser_t,
) {
}

unsafe extern "system" fn on_dialog_closed(
    _this: *mut cef_jsdialog_handler_t, _browser: *mut cef_browser_t,
) {
}

pub fn wrap() -> *mut cef_jsdialog_handler_t {
    let mut object: cef_jsdialog_handler_t = unsafe { std::mem::zeroed() };

    object.on_jsdialog = Some(on_jsdialog);
    object.on_before_unload_dialog = Some(on_before_unload_dialog);
    object.on_reset_dialog_state = Some(on_reset_dialog_state);
    object.on_dialog_closed = Some(on_dialog_closed);

    let wrapper = Wrapper::new(object, ());

    Box::into_raw(Box::new(wrapper)) as *mut _
}
