use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const OTP_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
struct CachedOtp {
    code: String,
    received_at: Instant,
}

#[derive(Default)]
struct OtpCache {
    value: Option<CachedOtp>,
}

impl OtpCache {
    fn store(&mut self, code: &str, now: Instant) {
        self.value = Some(CachedOtp {
            code: code.to_string(),
            received_at: now,
        });
    }

    fn active_code(&mut self, now: Instant) -> Option<String> {
        let cached = self.value.as_ref()?;
        if now.saturating_duration_since(cached.received_at) > OTP_TTL {
            self.value = None;
            return None;
        }
        Some(cached.code.clone())
    }

    fn consume_if_matches(&mut self, code: &str) {
        if self
            .value
            .as_ref()
            .is_some_and(|cached| cached.code == code)
        {
            self.value = None;
        }
    }
}

fn cache() -> &'static Mutex<OtpCache> {
    static CACHE: OnceLock<Mutex<OtpCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(OtpCache::default()))
}

pub fn cache_code(code: &str) {
    if code.is_empty() {
        return;
    }

    if let Ok(mut cache) = cache().lock() {
        cache.store(code, Instant::now());
        log::debug!("Cached verification code for double-click fill");
    }
}

fn active_code() -> Option<String> {
    cache().lock().ok()?.active_code(Instant::now())
}

fn consume_code(code: &str) {
    if let Ok(mut cache) = cache().lock() {
        cache.consume_if_matches(code);
    }
}

#[cfg(target_os = "macos")]
pub fn start_listener() {
    macos::start_listener();
}

#[cfg(not(target_os = "macos"))]
pub fn start_listener() {}

#[cfg(target_os = "macos")]
mod macos {
    use super::{active_code, consume_code};
    use crate::{clipboard, config::Config};
    use core_foundation::base::{Boolean, CFType, CFTypeID, CFTypeRef, TCFType};
    use core_foundation::runloop::CFRunLoop;
    use core_foundation::string::{CFString, CFStringRef};
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        CallbackResult, EventField,
    };
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::thread;
    use std::time::Duration;

    const FOCUS_SETTLE_DELAY: Duration = Duration::from_millis(80);
    const LISTENER_RETRY_DELAY: Duration = Duration::from_secs(10);
    const AX_ERROR_SUCCESS: i32 = 0;

    type AXUIElementRef = *const c_void;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCreateSystemWide() -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXUIElementIsAttributeSettable(
            element: AXUIElementRef,
            attribute: CFStringRef,
            settable: *mut Boolean,
        ) -> i32;
        fn AXUIElementGetTypeID() -> CFTypeID;
    }

    pub fn start_listener() {
        static STARTED: AtomicBool = AtomicBool::new(false);
        if STARTED.swap(true, Ordering::AcqRel) {
            return;
        }

        let (trigger_sender, trigger_receiver) = sync_channel(1);
        if let Err(error) = thread::Builder::new()
            .name("quick-fill-worker".to_string())
            .spawn(move || run_fill_worker(trigger_receiver))
        {
            log::error!("Failed to start quick-fill worker: {}", error);
            return;
        }

        if let Err(error) = thread::Builder::new()
            .name("quick-fill-listener".to_string())
            .spawn(move || run_event_listener(trigger_sender))
        {
            log::error!("Failed to start quick-fill listener: {}", error);
        }
    }

    fn run_event_listener(trigger_sender: SyncSender<()>) {
        loop {
            let sender = trigger_sender.clone();
            let result = CGEventTap::with_enabled(
                CGEventTapLocation::Session,
                CGEventTapPlacement::TailAppendEventTap,
                CGEventTapOptions::ListenOnly,
                vec![CGEventType::LeftMouseDown],
                move |_proxy, event_type, event| {
                    if matches!(event_type, CGEventType::LeftMouseDown)
                        && event.get_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE) == 2
                        && active_code().is_some()
                    {
                        let _ = sender.try_send(());
                    }
                    CallbackResult::Keep
                },
                || {
                    log::info!("Double-click input listener started");
                    CFRunLoop::run_current();
                },
            );

            if result.is_err() {
                log::warn!(
                    "Unable to start double-click listener; retrying after Accessibility permission is available"
                );
            } else {
                log::warn!("Double-click listener stopped unexpectedly; restarting");
            }
            thread::sleep(LISTENER_RETRY_DELAY);
        }
    }

    fn run_fill_worker(trigger_receiver: Receiver<()>) {
        while trigger_receiver.recv().is_ok() {
            thread::sleep(FOCUS_SETTLE_DELAY);

            let config = Config::load().unwrap_or_default();
            if !config.double_click_fill || !focused_element_is_empty_input() {
                continue;
            }

            let Some(code) = active_code() else {
                continue;
            };

            match clipboard::auto_paste(true, &code) {
                Ok(()) => {
                    consume_code(&code);
                    log::info!("Filled verification code by double-click");
                }
                Err(error) => log::error!(
                    "Failed to fill verification code by double-click: {}",
                    error
                ),
            }
        }
    }

    fn focused_element_is_empty_input() -> bool {
        unsafe {
            let system_wide = AXUIElementCreateSystemWide();
            if system_wide.is_null() {
                return false;
            }
            let system_wide = CFType::wrap_under_create_rule(system_wide as CFTypeRef);

            let Some(focused) = copy_attribute(
                system_wide.as_CFTypeRef() as AXUIElementRef,
                "AXFocusedUIElement",
            ) else {
                return false;
            };
            if focused.type_of() != AXUIElementGetTypeID() {
                return false;
            }

            let element = focused.as_CFTypeRef() as AXUIElementRef;
            let Some(role) = copy_string_attribute(element, "AXRole") else {
                return false;
            };
            if !matches!(role.as_str(), "AXTextField" | "AXTextArea" | "AXComboBox") {
                return false;
            }

            let value_attribute = CFString::new("AXValue");
            let mut settable: Boolean = 0;
            if AXUIElementIsAttributeSettable(
                element,
                value_attribute.as_concrete_TypeRef(),
                &mut settable,
            ) != AX_ERROR_SUCCESS
                || settable == 0
            {
                return false;
            }

            copy_string_attribute(element, "AXValue").is_some_and(|value| value.is_empty())
        }
    }

    unsafe fn copy_attribute(element: AXUIElementRef, name: &str) -> Option<CFType> {
        let attribute = CFString::new(name);
        let mut value: CFTypeRef = ptr::null();
        if unsafe {
            AXUIElementCopyAttributeValue(element, attribute.as_concrete_TypeRef(), &mut value)
        } != AX_ERROR_SUCCESS
            || value.is_null()
        {
            return None;
        }
        Some(unsafe { CFType::wrap_under_create_rule(value) })
    }

    unsafe fn copy_string_attribute(element: AXUIElementRef, name: &str) -> Option<String> {
        let value = unsafe { copy_attribute(element, name) }?;
        value
            .downcast::<CFString>()
            .map(|string| string.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{OTP_TTL, OtpCache};
    use std::time::{Duration, Instant};

    #[test]
    fn returns_fresh_code_and_consumes_it_once() {
        let now = Instant::now();
        let mut cache = OtpCache::default();
        cache.store("123456", now);

        assert_eq!(cache.active_code(now), Some("123456".to_string()));
        cache.consume_if_matches("123456");
        assert_eq!(cache.active_code(now), None);
    }

    #[test]
    fn keeps_newer_code_when_consuming_an_older_one() {
        let now = Instant::now();
        let mut cache = OtpCache::default();
        cache.store("654321", now);

        cache.consume_if_matches("123456");
        assert_eq!(cache.active_code(now), Some("654321".to_string()));
    }

    #[test]
    fn expires_code_after_five_minutes() {
        let now = Instant::now();
        let mut cache = OtpCache::default();
        cache.store("123456", now);

        assert_eq!(
            cache.active_code(now + OTP_TTL + Duration::from_millis(1)),
            None
        );
    }
}
