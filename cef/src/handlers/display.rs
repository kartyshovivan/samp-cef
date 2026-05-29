use crate::browser::Browser;
use cef_sys::HCURSOR;

pub trait DisplayHandler {
    fn on_cursor_change(&self, _browser: Browser, _cursor: HCURSOR) -> bool {
        false
    }
}
