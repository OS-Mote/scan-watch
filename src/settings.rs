use static_cell::StaticCell;
use core::cell::RefCell;
use chrono::{
    DateTime,
    FixedOffset
};
use esp_storage::FlashStorage;
use esp_nvs::{
    Nvs,
    Key
};
use embassy_time::Instant;
use embassy_sync::blocking_mutex::CriticalSectionMutex;

const SETTINGS_NAMESPACE_KEY: &str = "settings";

const DISPLAY_BRIGHTNESS_SETTINGS_KEY: &str = "dis-bri";
const DISPLAY_BRIGHTNESS_DEFAULT: u8 = 127;
const DISPLAY_TIMEOUT_SETTINGS_KEY: &str = "dis-tim";
const DISPLAY_TIMEOUT_DEFAULT: u8 = 5;
const TIMEZONE_OFFSET_SETTINGS_KEY: &str = "tz-offset";
const TIMEZONE_OFFSET_DEFAULT: i32 = 0;
const TIMESTAMP_SETTINGS_KEY: &str = "timest";
const TIMESTAMP_DEFAULT: i64 = 0;
const TIMESTAMP_OFFSET_SETTINGS_KEY: &str = "timest-off";
const TIMESTAMP_OFFSET_DEFAULT: u64 = 0;
const SMART_GLASSES_SCAN_DURATION_SETTINGS_KEY: &str = "smrt-dur";
const SMART_GLASSES_SCAN_DURATION_DEFAULT: u8 = 10;
const REMOTE_ID_SCAN_DURATION_SETTINGS_KEY: &str = "rmtid-dur";
const REMOTE_ID_SCAN_DURATION_DEFAULT: u8 = 10;

pub struct Settings<S: 'static> {
    storage: &'static S,
    display_brightness: u8,
    display_timeout: u8,
    timezone_offset: i32,
    timestamp: i64,
    timestamp_offset: u64,
    smart_glasses_scan_duration: u8,
    remote_id_scan_duration: u8,
}

impl Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>> {
    pub const fn new(storage: &'static CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>) -> Self {
        Settings {
            storage,
            display_brightness: DISPLAY_BRIGHTNESS_DEFAULT,
            display_timeout: DISPLAY_TIMEOUT_DEFAULT,
            timezone_offset: TIMEZONE_OFFSET_DEFAULT,
            timestamp: TIMESTAMP_DEFAULT,
            timestamp_offset: TIMESTAMP_OFFSET_DEFAULT,
            smart_glasses_scan_duration: SMART_GLASSES_SCAN_DURATION_DEFAULT,
            remote_id_scan_duration: REMOTE_ID_SCAN_DURATION_DEFAULT,
        }
    }

    pub fn init(mut self) -> Self {
        critical_section::with(|cs| {
            self.display_brightness = self.storage.borrow(cs).borrow_mut().get(&Key::from_str(SETTINGS_NAMESPACE_KEY), &Key::from_str(DISPLAY_BRIGHTNESS_SETTINGS_KEY))
                .unwrap_or(DISPLAY_BRIGHTNESS_DEFAULT);

            self.display_timeout = self.storage.borrow(cs).borrow_mut().get(&Key::from_str(SETTINGS_NAMESPACE_KEY), &Key::from_str(DISPLAY_TIMEOUT_SETTINGS_KEY))
                .unwrap_or(DISPLAY_TIMEOUT_DEFAULT);

            self.timezone_offset = self.storage.borrow(cs).borrow_mut().get(&Key::from_str(SETTINGS_NAMESPACE_KEY), &Key::from_str(TIMEZONE_OFFSET_SETTINGS_KEY))
                .unwrap_or(TIMEZONE_OFFSET_DEFAULT);

            self.timestamp = self.storage.borrow(cs).borrow_mut().get(&Key::from_str(SETTINGS_NAMESPACE_KEY), &Key::from_str(TIMESTAMP_SETTINGS_KEY))
                .unwrap_or(TIMESTAMP_DEFAULT);

            self.timestamp_offset = TIMESTAMP_OFFSET_DEFAULT;
        });

        self
    }

    pub fn get_display_brightness(&self) -> u8 {
        self.display_brightness
    }

    pub fn set_display_brightness(&mut self, brightness: u8) {
        critical_section::with(|cs| {
            if self.storage.borrow(cs).borrow_mut().set(&Key::from_str(SETTINGS_NAMESPACE_KEY), &Key::from_str(DISPLAY_BRIGHTNESS_SETTINGS_KEY), brightness).is_ok() {
                self.display_brightness = brightness;
            }
        });
    }

    pub fn get_display_timeout(&self) -> u8 {
        self.display_timeout
    }

    pub fn set_display_timeout(&mut self, timeout: u8) {
        critical_section::with(|cs| {
            if self.storage.borrow(cs).borrow_mut().set(&Key::from_str(SETTINGS_NAMESPACE_KEY), &Key::from_str(DISPLAY_TIMEOUT_SETTINGS_KEY), timeout).is_ok() {
                self.display_timeout = timeout;
            }
        });
    }

    pub fn get_timezone_offset(&self) -> i32 {
        self.timezone_offset
    }

    pub fn set_timezone_offset(&mut self, offset: i32) {
        critical_section::with(|cs| {
            if self.storage.borrow(cs).borrow_mut().set(&Key::from_str(SETTINGS_NAMESPACE_KEY), &Key::from_str(TIMEZONE_OFFSET_SETTINGS_KEY), offset).is_ok() {
                self.timezone_offset = offset;
            }
        });
    }

    pub fn get_timestamp(&self) -> i64 {
        self.timestamp
    }

    pub fn set_timestamp(&mut self, timestamp: i64) {
        critical_section::with(|cs| {
            if self.storage.borrow(cs).borrow_mut().set(&Key::from_str(SETTINGS_NAMESPACE_KEY), &Key::from_str(TIMESTAMP_SETTINGS_KEY), timestamp).is_ok() {
                self.timestamp = timestamp;
            }
        });
    }

    pub fn set_timestamp_offset(&mut self, timestamp_offset: u64) {
        critical_section::with(|cs| {
            if self.storage.borrow(cs).borrow_mut().set(&Key::from_str(SETTINGS_NAMESPACE_KEY), &Key::from_str(TIMESTAMP_OFFSET_SETTINGS_KEY), timestamp_offset).is_ok() {
                self.timestamp_offset = timestamp_offset;
            }
        });
    }

    pub fn get_timestamp_offset(&self) -> u64 {
        self.timestamp_offset
    }

    pub fn set_smart_glasses_scan_duration(&mut self, scan_duration: u8) {
        critical_section::with(|cs| {
            if self.storage.borrow(cs).borrow_mut().set(&Key::from_str(SETTINGS_NAMESPACE_KEY), &Key::from_str(SMART_GLASSES_SCAN_DURATION_SETTINGS_KEY), scan_duration).is_ok() {
                self.smart_glasses_scan_duration = scan_duration;
            }
        });
    }

    pub fn get_smart_glasses_scan_duration(&self) -> u8 {
        self.smart_glasses_scan_duration
    }

    pub fn set_remote_id_scan_duration(&mut self, scan_duration: u8) {
        critical_section::with(|cs| {
            if self.storage.borrow(cs).borrow_mut().set(&Key::from_str(SETTINGS_NAMESPACE_KEY), &Key::from_str(REMOTE_ID_SCAN_DURATION_SETTINGS_KEY), scan_duration).is_ok() {
                self.remote_id_scan_duration = scan_duration;
            }
        });
    }

    pub fn get_remote_id_scan_duration(&self) -> u8 {
        self.remote_id_scan_duration
    }
}