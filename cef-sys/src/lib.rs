#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::empty_docs,
    clippy::type_complexity
)]

// Two CEF versions live in this repo:
// * 32-bit: CEF 89 inside gta_sa.exe (client.dll). Only CefListValue & co. are used there
//   (the external plugin API passes `cef_list_value_t`), browsers run out of process.
// * 64-bit: CEF 156 in cef_host.exe and renderer.exe, which run the actual browsers.
#[cfg(target_arch = "x86")]
include!("bindings.rs");

#[cfg(target_arch = "x86_64")]
include!("bindings_156.rs");

/// CEF 89 names for types/functions renamed since (`v8value` -> `v8_value`, ...), so the
/// `cef` wrapper compiles against both versions.
#[cfg(target_arch = "x86_64")]
mod compat {
    pub type cef_v8value_t = super::cef_v8_value_t;
    pub type cef_v8context_t = super::cef_v8_context_t;
    pub type cef_v8handler_t = super::cef_v8_handler_t;

    pub use super::cef_v8_context_get_current_context as cef_v8context_get_current_context;
    pub use super::cef_v8_value_create_array as cef_v8value_create_array;
    pub use super::cef_v8_value_create_bool as cef_v8value_create_bool;
    pub use super::cef_v8_value_create_double as cef_v8value_create_double;
    pub use super::cef_v8_value_create_function as cef_v8value_create_function;
    pub use super::cef_v8_value_create_int as cef_v8value_create_int;
    pub use super::cef_v8_value_create_object as cef_v8value_create_object;
    pub use super::cef_v8_value_create_string as cef_v8value_create_string;
    pub use super::cef_v8_value_create_undefined as cef_v8value_create_undefined;
}

#[cfg(target_arch = "x86_64")]
pub use compat::*;

/// CEF API version the 64-bit bindings were generated for (`-DCEF_API_VERSION`). Pass it to
/// `cef_api_hash` before any other CEF call so libcef uses the matching struct layouts.
#[cfg(target_arch = "x86_64")]
pub const CEF_API_VERSION: i32 = 15600;
