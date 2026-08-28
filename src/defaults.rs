use esp_storage::FlashStorage;
use esp_nvs::{
    Nvs,
    Key
};
use chrono::{
    DateTime,
    FixedOffset
};
use embassy_time::Instant;
use crate::co5300::Co5300Display;

const DEFAULTS_NAMESPACE_KEY: &str = "defaults";

const DISPLAY_BRIGHTNESS_DEFAULT_KEY: &str = "dis-bri";
const DISPLAY_BRIGHTNESS_DEFAULT: u8 = 127;
const DISPLAY_TIMEOUT_DEFAULT_KEY: &str = "dis-tim";
const DISPLAY_TIMEOUT_DEFAULT: u8 = 5;
const TIMEZONE_OFFSET_DEFAULT_KEY: &str = "tz-offset";
const TIMEZONE_OFFSET_DEFAULT: i32 = 0;
const TIMESTAMP_DEFAULT_KEY: &str = "timest";
const TIMESTAMP_DEFAULT: i64 = 0;

pub fn set_display_brightness(brightness: u8, display: &mut Co5300Display<'static>, storage: &mut Nvs<FlashStorage<'static>>) {
    display.set_brightness(brightness);

    let _ = storage.set(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(DISPLAY_BRIGHTNESS_DEFAULT_KEY), brightness);
}

pub fn get_display_brightness(storage: &mut Nvs<FlashStorage<'static>>) -> u8 {
    storage.get(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(DISPLAY_BRIGHTNESS_DEFAULT_KEY))
        .unwrap_or(DISPLAY_BRIGHTNESS_DEFAULT)
}

pub fn set_display_timeout(timeout: u8, storage: &mut Nvs<FlashStorage<'static>>) {
    let _ = storage.set(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(DISPLAY_TIMEOUT_DEFAULT_KEY), timeout);
}

pub fn get_display_timeout(storage: &mut Nvs<FlashStorage<'static>>) -> u8 {
    storage.get(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(DISPLAY_TIMEOUT_DEFAULT_KEY))
        .unwrap_or(DISPLAY_TIMEOUT_DEFAULT)
}

pub fn set_timezone_offset(offset: i32, storage: &mut Nvs<FlashStorage<'static>>) {
    let _ = storage.set(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(TIMEZONE_OFFSET_DEFAULT_KEY), offset);
}

pub fn get_timezone_offset(storage: &mut Nvs<FlashStorage<'static>>) -> i32 {
    storage.get(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(TIMEZONE_OFFSET_DEFAULT_KEY))
        .unwrap_or(TIMEZONE_OFFSET_DEFAULT)
}

pub fn set_time(timestamp: i64, storage: &mut Nvs<FlashStorage<'static>>) {
    let _ = storage.set(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(TIMESTAMP_DEFAULT_KEY), timestamp);
}

pub fn get_timestamp(storage: &mut Nvs<FlashStorage<'static>>) -> i64 {
    storage.get(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(TIMESTAMP_DEFAULT_KEY))
        .unwrap_or(TIMESTAMP_DEFAULT)
}

pub fn get_date_time(storage: &mut Nvs<FlashStorage<'static>>) -> DateTime<FixedOffset> {
    let timestamp = get_timestamp(storage);
    let timezone_offset = get_timezone_offset(storage);
    let now_ticks = Instant::now().as_micros();

    DateTime::from_timestamp_micros(timestamp + (now_ticks as i64))
        .unwrap()
        .with_timezone(&FixedOffset::east_opt(3600 * timezone_offset).unwrap())
}