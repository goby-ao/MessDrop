use std::process::Child;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const OTP_TTL: Duration = Duration::from_secs(5 * 60);

#[cfg(any(target_os = "macos", test))]
const DOUBLE_CLICK_MAX_INTERVAL: Duration = Duration::from_millis(500);
#[cfg(any(target_os = "macos", test))]
const DOUBLE_CLICK_MAX_DISTANCE: f64 = 8.0;

#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
struct DoubleClickDetector {
    previous_click: Option<(Instant, f64, f64)>,
}

#[cfg(any(target_os = "macos", test))]
impl DoubleClickDetector {
    fn register_click(&mut self, click_state: i64, x: f64, y: f64, now: Instant) -> bool {
        if click_state >= 2 {
            self.previous_click = None;
            return true;
        }

        let is_double_click =
            self.previous_click
                .is_some_and(|(previous_at, previous_x, previous_y)| {
                    let dx = x - previous_x;
                    let dy = y - previous_y;
                    now.saturating_duration_since(previous_at) <= DOUBLE_CLICK_MAX_INTERVAL
                        && dx * dx + dy * dy
                            <= DOUBLE_CLICK_MAX_DISTANCE * DOUBLE_CLICK_MAX_DISTANCE
                });

        self.previous_click = if is_double_click {
            None
        } else {
            Some((now, x, y))
        };
        is_double_click
    }
}

struct CachedOtp<P> {
    code: String,
    received_at: Instant,
    popup: Option<P>,
}

struct OtpCache<P> {
    value: Option<CachedOtp<P>>,
}

impl<P> Default for OtpCache<P> {
    fn default() -> Self {
        Self { value: None }
    }
}

struct ConsumedOtp<P> {
    popup: Option<P>,
}

impl<P> OtpCache<P> {
    fn store(&mut self, code: &str, now: Instant) {
        self.value = Some(CachedOtp {
            code: code.to_string(),
            received_at: now,
            popup: None,
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

    fn attach_popup(&mut self, code: &str, popup: P, now: Instant) -> Result<(), P> {
        let is_active_match = self.value.as_ref().is_some_and(|cached| {
            cached.code == code && now.saturating_duration_since(cached.received_at) <= OTP_TTL
        });
        if !is_active_match {
            return Err(popup);
        }

        if let Some(cached) = self.value.as_mut() {
            cached.popup = Some(popup);
        }
        Ok(())
    }

    fn consume_if_matches(&mut self, code: &str) -> Option<ConsumedOtp<P>> {
        if !self
            .value
            .as_ref()
            .is_some_and(|cached| cached.code == code)
        {
            return None;
        }

        self.value.take().map(|cached| ConsumedOtp {
            popup: cached.popup,
        })
    }

    fn consume_if_confirmed(
        &mut self,
        code: &str,
        observed_value: Option<&str>,
    ) -> Option<ConsumedOtp<P>> {
        if !value_confirms_fill(observed_value, code) {
            return None;
        }
        self.consume_if_matches(code)
    }
}

fn cache() -> &'static Mutex<OtpCache<Child>> {
    static CACHE: OnceLock<Mutex<OtpCache<Child>>> = OnceLock::new();
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

pub fn register_popup(code: &str, popup: Child) {
    if let Ok(mut cache) = cache().lock() {
        let _ = cache.attach_popup(code, popup, Instant::now());
    }
}

fn active_code() -> Option<String> {
    cache().lock().ok()?.active_code(Instant::now())
}

fn consume_confirmed_code(code: &str, observed_value: Option<&str>) -> Option<ConsumedOtp<Child>> {
    cache()
        .lock()
        .ok()?
        .consume_if_confirmed(code, observed_value)
}

fn dismiss_popup(mut popup: Child) {
    match popup.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) => {
            if let Err(error) = popup.kill() {
                log::error!("Failed to close verification popup: {}", error);
                return;
            }
            let _ = popup.wait();
            log::debug!("Closed verification popup after confirmed fill");
        }
        Err(error) => log::error!("Failed to inspect verification popup: {}", error),
    }
}

fn value_confirms_fill(value: Option<&str>, code: &str) -> bool {
    value.is_some_and(|value| value == code)
}

#[cfg(target_os = "macos")]
pub fn start_listener() {
    macos::start_listener();
}

#[cfg(not(target_os = "macos"))]
pub fn start_listener() {}

#[cfg(target_os = "macos")]
mod macos {
    use super::{DoubleClickDetector, active_code, consume_confirmed_code, dismiss_popup};
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
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::thread;
    use std::time::{Duration, Instant};

    const FOCUS_SETTLE_DELAY: Duration = Duration::from_millis(80);
    const FILL_VERIFICATION_DELAY: Duration = Duration::from_millis(200);
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
            let double_click_detector = Mutex::new(DoubleClickDetector::default());
            let result = CGEventTap::with_enabled(
                CGEventTapLocation::Session,
                CGEventTapPlacement::TailAppendEventTap,
                CGEventTapOptions::ListenOnly,
                vec![CGEventType::LeftMouseDown],
                move |_proxy, event_type, event| {
                    let click_state =
                        event.get_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE);
                    let location = event.location();
                    let is_double_click = double_click_detector
                        .lock()
                        .map(|mut detector| {
                            detector.register_click(
                                click_state,
                                location.x,
                                location.y,
                                Instant::now(),
                            )
                        })
                        .unwrap_or(false);
                    if matches!(event_type, CGEventType::LeftMouseDown)
                        && is_double_click
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
            if !config.double_click_fill {
                continue;
            }

            let Some(input) = focused_empty_input() else {
                continue;
            };
            let Some(code) = active_code() else {
                continue;
            };

            match clipboard::auto_paste(true, &code) {
                Ok(()) => {
                    thread::sleep(FILL_VERIFICATION_DELAY);
                    let observed_value = input.value();
                    if let Some(consumed) = consume_confirmed_code(&code, observed_value.as_deref())
                    {
                        if let Some(popup) = consumed.popup {
                            dismiss_popup(popup);
                        }
                        log::info!("Confirmed verification code fill after double-click");
                    } else {
                        log::warn!(
                            "Could not confirm verification code fill; keeping popup visible"
                        );
                    }
                }
                Err(error) => log::error!(
                    "Failed to fill verification code by double-click: {}",
                    error
                ),
            }
        }
    }

    struct FocusedInput {
        element: CFType,
    }

    impl FocusedInput {
        fn value(&self) -> Option<String> {
            unsafe {
                copy_string_attribute(self.element.as_CFTypeRef() as AXUIElementRef, "AXValue")
            }
        }
    }

    fn focused_empty_input() -> Option<FocusedInput> {
        unsafe {
            let system_wide = AXUIElementCreateSystemWide();
            if system_wide.is_null() {
                return None;
            }
            let system_wide = CFType::wrap_under_create_rule(system_wide as CFTypeRef);

            let Some(focused) = copy_attribute(
                system_wide.as_CFTypeRef() as AXUIElementRef,
                "AXFocusedUIElement",
            ) else {
                return None;
            };
            if focused.type_of() != AXUIElementGetTypeID() {
                return None;
            }

            let element = focused.as_CFTypeRef() as AXUIElementRef;
            let Some(role) = copy_string_attribute(element, "AXRole") else {
                return None;
            };
            if !matches!(role.as_str(), "AXTextField" | "AXTextArea" | "AXComboBox") {
                return None;
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
                return None;
            }

            if !copy_string_attribute(element, "AXValue").is_some_and(|value| value.is_empty()) {
                return None;
            }

            Some(FocusedInput { element: focused })
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
    use super::{DoubleClickDetector, OTP_TTL, OtpCache, value_confirms_fill};
    use std::time::{Duration, Instant};

    #[test]
    fn returns_fresh_code_and_consumes_it_once() {
        let now = Instant::now();
        let mut cache = OtpCache::<u32>::default();
        cache.store("123456", now);

        assert_eq!(cache.active_code(now), Some("123456".to_string()));
        assert!(cache.consume_if_matches("123456").is_some());
        assert_eq!(cache.active_code(now), None);
    }

    #[test]
    fn keeps_newer_code_when_consuming_an_older_one() {
        let now = Instant::now();
        let mut cache = OtpCache::<u32>::default();
        cache.store("654321", now);

        assert!(cache.consume_if_matches("123456").is_none());
        assert_eq!(cache.active_code(now), Some("654321".to_string()));
    }

    #[test]
    fn expires_code_after_five_minutes() {
        let now = Instant::now();
        let mut cache = OtpCache::<u32>::default();
        cache.store("123456", now);

        assert_eq!(
            cache.active_code(now + OTP_TTL + Duration::from_millis(1)),
            None
        );
    }

    #[test]
    fn returns_popup_only_for_matching_code() {
        let now = Instant::now();
        let mut cache = OtpCache::<u32>::default();
        cache.store("123456", now);
        assert!(cache.attach_popup("123456", 42, now).is_ok());

        let consumed = cache.consume_if_matches("123456").unwrap();
        assert_eq!(consumed.popup, Some(42));
    }

    #[test]
    fn confirms_only_an_exact_observed_fill() {
        assert!(value_confirms_fill(Some("123456"), "123456"));
        assert!(!value_confirms_fill(None, "123456"));
        assert!(!value_confirms_fill(Some(""), "123456"));
        assert!(!value_confirms_fill(Some("123"), "123456"));
        assert!(!value_confirms_fill(Some("••••••"), "123456"));
        assert!(!value_confirms_fill(Some("654321"), "123456"));
    }

    #[test]
    fn keeps_code_and_popup_when_fill_is_not_confirmed() {
        let now = Instant::now();
        let mut cache = OtpCache::<u32>::default();
        cache.store("123456", now);
        assert!(cache.attach_popup("123456", 42, now).is_ok());

        assert!(cache.consume_if_confirmed("123456", Some("123")).is_none());
        assert_eq!(cache.active_code(now), Some("123456".to_string()));

        let consumed = cache
            .consume_if_confirmed("123456", Some("123456"))
            .unwrap();
        assert_eq!(consumed.popup, Some(42));
    }

    #[test]
    fn detects_two_nearby_clicks_when_macos_click_state_stays_one() {
        let now = Instant::now();
        let mut detector = DoubleClickDetector::default();

        assert!(!detector.register_click(1, 100.0, 100.0, now));
        assert!(detector.register_click(1, 103.0, 104.0, now + Duration::from_millis(200)));
    }

    #[test]
    fn rejects_clicks_that_are_too_slow_or_too_far_apart() {
        let now = Instant::now();
        let mut detector = DoubleClickDetector::default();

        assert!(!detector.register_click(1, 100.0, 100.0, now));
        assert!(!detector.register_click(1, 100.0, 100.0, now + Duration::from_millis(501)));
        assert!(!detector.register_click(1, 120.0, 120.0, now + Duration::from_millis(600)));
    }

    #[test]
    fn accepts_the_native_macos_double_click_state() {
        let mut detector = DoubleClickDetector::default();

        assert!(detector.register_click(2, 100.0, 100.0, Instant::now()));
    }
}
