use cef::app::App;
use cef::command_line::CommandLine;
use cef::handlers::browser_process::BrowserProcessHandler;
use cef::handlers::render_process::RenderProcessHandler;
use cef::types::string::CefString;

#[derive(Clone)]
pub struct HostApp {
    pub gpu: bool,
}

impl RenderProcessHandler for HostApp {}

impl BrowserProcessHandler for HostApp {
    fn on_context_initialized(&self) {
        log::info!("CEF context initialized");
        crate::assets_scheme::register_scheme_handler_factory();
        crate::snapshot_scheme::register();
        crate::ipc::send(cef_ipc::Event::Ready);
    }
}

impl App for HostApp {
    type RenderProcessHandler = Self;
    type BrowserProcessHandler = Self;

    fn browser_process_handler(&self) -> Option<Self::BrowserProcessHandler> {
        Some(self.clone())
    }

    fn on_before_command_line_processing(&self, _process_type: CefString, command_line: CommandLine) {
        if self.gpu {
            command_line.append_switch("enable-gpu-rasterization");
            command_line.append_switch("disable-gpu-vsync");
        } else {
            command_line.append_switch("disable-gpu-compositing");
            command_line.append_switch("disable-gpu");
        }

        command_line.append_switch("enable-begin-frame-scheduling");

        // Off-screen browsers must never be treated as background.
        command_line.append_switch("disable-renderer-backgrounding");
        command_line.append_switch("disable-background-timer-throttling");
        command_line.append_switch("disable-backgrounding-occluded-windows");

        command_line.append_switch_with_value("autoplay-policy", "no-user-gesture-required");
        // Was `cef_settings_t::ignore_certificate_errors` in CEF 89.
        command_line.append_switch("ignore-certificate-errors");
        command_line.append_switch("enable-media-stream");

        // Extra Chromium switches for tuning without a rebuild: `cef/host_switches.txt`,
        // one `name` or `name=value` per line, `#` comments allowed.
        if let Ok(text) = std::fs::read_to_string(crate::paths::cef_dir().join("host_switches.txt")) {
            for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                let line = line.trim_start_matches("--");
                match line.split_once('=') {
                    Some((name, value)) => command_line.append_switch_with_value(name, value),
                    None => command_line.append_switch(line),
                }
                log::info!("extra switch: {}", line);
            }
        }
    }

    fn on_register_custom_schemes(&self, registrar: *mut cef_sys::cef_scheme_registrar_t) {
        crate::assets_scheme::register_custom_scheme(registrar);
    }
}
