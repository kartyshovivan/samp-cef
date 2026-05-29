use crate::browser::Browser;
use crate::handlers::display::DisplayHandler;
use crate::rust_to_c::Wrapper;

use cef_sys::{
    HCURSOR, cef_browser_t, cef_cursor_info_t, cef_cursor_type_t, cef_display_handler_t,
};

unsafe extern "system" fn on_cursor_change<I: DisplayHandler>(
    this: *mut cef_display_handler_t, browser: *mut cef_browser_t, cursor: HCURSOR,
    _type_: cef_cursor_type_t::Type, _custom_cursor_info: *const cef_cursor_info_t,
) -> ::std::os::raw::c_int {
    if browser.is_null() {
        return 0;
    }

    let obj: &mut Wrapper<_, I> = Wrapper::unwrap(this);
    let browser = Browser::from_raw_borrowed(browser);
    if obj.interface.on_cursor_change(browser, cursor) {
        1
    } else {
        0
    }
}

pub fn wrap<T: DisplayHandler>(handler: T) -> *mut cef_display_handler_t {
    let mut object: cef_display_handler_t = unsafe { std::mem::zeroed() };

    object.on_cursor_change = Some(on_cursor_change::<T>);

    let wrapper = Wrapper::new(object, handler);

    Box::into_raw(Box::new(wrapper)) as *mut _
}
