#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]
#![allow(clippy::type_complexity)]
#![allow(unused_imports, dead_code)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::manual_range_contains)]

extern crate alloc;

use esp_println::println;
use alloc::{
    boxed::Box,
    rc::Rc,
    vec
};
use static_cell::StaticCell;
use trouble_host::prelude::*;
use chrono::{
    DateTime,
    Datelike,
    FixedOffset,
    NaiveDate,
    Timelike
};
use esp_hal::{
    delay::Delay, dma::{
        DmaRxBuf,
        DmaTxBuf
    }, dma_buffers, gpio::{
        AnyPin,
        Input,
        InputConfig,
        Level,
        Output,
        OutputConfig
    }, i2c::master::{
        Config as I2cConfig,
        I2c
    }, peripherals::{BT, FLASH}, rtc_cntl::{
        Rtc,
        SocResetReason,
        sleep::{
            Ext0WakeupSource,
            TimerWakeupSource,
            WakeupLevel
        },
    }, spi::{
        Mode as SpiMode,
        master::{
            Config as SpiConfig,
            Spi
        }
    }, system::{
        SleepSource,
        reset_reason,
        wakeup_cause
    }, time::Rate, timer::timg::TimerGroup,
};
use esp_storage::FlashStorage;
use esp_nvs::{
    Nvs,
    Key
};
use esp_radio::{
    ble::controller::BleConnector, wifi::{
        self, SecondaryChannel, WifiController, sniffer::Sniffer
    }
};
use embassy_sync::{
    blocking_mutex,
    blocking_mutex::CriticalSectionMutex,
    mutex,
    mutex::Mutex,
    blocking_mutex::raw::CriticalSectionRawMutex,
    signal::Signal,
    channel::Channel
};
use embassy_executor::{
    task,
    Spawner
};
use embassy_time::{
    Timer,
    Instant
};
use embassy_futures::{
    join::join,
    join::join3,
    select::select
};
use ieee80211::{
    match_frames,
    mgmt_frame::{
        BeaconFrame,
        RawActionFrame
    },
    elements::VendorSpecificElement
};
use core::{
    cell::RefCell,
    time::Duration,
};
use embedded_hal_bus::i2c::RefCellDevice;
use embedded_graphics::{
    pixelcolor::Rgb565, prelude::*
};
use slint::{
    VecModel,
    ModelRc,
    LogicalPosition,
    platform::{
        software_renderer::{
            MinimalSoftwareWindow,
            RepaintBufferType
        },
        PointerEventButton,
        WindowEvent
    }
};

use cst92xx::{
    BlockingCST92xx,
    Point as TouchPoint
};

use esp_hal::gpio::Pull;
mod qspi_bus;
mod framebuffer;
mod axp2101;
mod co5300;
mod settings;

use crate::{
    settings::Settings,
    axp2101::Axp2101,
    co5300::{
        Co5300Display,
        LCD_WIDTH,
        LCD_HEIGHT
    },
    framebuffer::Framebuffer,
    qspi_bus::QspiBus
};

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {}", info);
    loop {}
}

esp_bootloader_esp_idf::esp_app_desc!();

slint::include_modules!();

const XL9555_I2C_ADDR: u8 = 0x20;
const CST226_I2C_ADDR: u8 = 0x5A;

struct EmbassySlintPlatform {
    window: alloc::rc::Rc<MinimalSoftwareWindow>,
}

impl EmbassySlintPlatform {
    fn new(window: alloc::rc::Rc<MinimalSoftwareWindow>) -> Self {
        Self { window }
    }
}

impl slint::platform::Platform for EmbassySlintPlatform {
    fn create_window_adapter(&self) -> Result<alloc::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        core::time::Duration::from_micros(Instant::now().as_micros())
    }
}

const SLEEP_BATTERY_PERCENTAGE: u8 = 5;
const SLEEP_SECONDS_FOR_CHARING: u64 = 10;

static RTC_CELL: StaticCell<CriticalSectionMutex<RefCell<Rtc<'static>>>> = StaticCell::new();
static POWER_CELL: StaticCell<CriticalSectionMutex<RefCell<Axp2101<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>>>>> = StaticCell::new();
static FLASH_STORAGE_CELL: StaticCell<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>> = StaticCell::new();
static DISPLAY_CELL: StaticCell<CriticalSectionMutex<RefCell<Co5300Display<'static>>>> = StaticCell::new();
static TOUCH_CELL: StaticCell<CriticalSectionMutex<RefCell<BlockingCST92xx<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>, Delay>>>> = StaticCell::new();
static SETTINGS_CELL: StaticCell<CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>> = StaticCell::new();

static REMOTE_ID_SCAN_TASK_COMMAND: Signal<CriticalSectionRawMutex, RemoteIdScanTaskCommand> = Signal::new();
static REMOTE_ID_SCAN_TASK_STATE: Mutex<CriticalSectionRawMutex, RemoteIdScanTaskState> = Mutex::new(RemoteIdScanTaskState::Stopped);
static REMOTE_ID_DETECTED: Signal<CriticalSectionRawMutex, Instant> = Signal::new();

static SMART_GLASSES_SCAN_TASK_COMMAND: Signal<CriticalSectionRawMutex, SmartGlassesScanTaskCommand> = Signal::new();
static SMART_GLASSES_SCAN_TASK_STATE: Mutex<CriticalSectionRawMutex, SmartGlassesScanTaskState> = Mutex::new(SmartGlassesScanTaskState::Stopped);
static SMART_GLASSES_DETECTED: Signal<CriticalSectionRawMutex, Instant> = Signal::new();

static FLASHLIGHT_ON: Mutex<CriticalSectionRawMutex, bool> = Mutex::new(true);
static DISPLAY_ON: Mutex<CriticalSectionRawMutex, bool> = Mutex::new(true);
static DISPLAY_TOUCHED: Signal<CriticalSectionRawMutex, Instant> = Signal::new();
static DISPLAY_TOUCH_EVENT_UPDATED: Signal<CriticalSectionRawMutex, WindowEvent> = Signal::new();
static BATTERY_STATUS_UPDATED: Signal<CriticalSectionRawMutex, (u8, bool)> = Signal::new();
static DATE_TIME_UPDATED: Signal<CriticalSectionRawMutex, DateTime<FixedOffset>> = Signal::new();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default();
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 128 * 1024);
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt = esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);

    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // Initialize the realtime clock.
    let mut rtc = Rtc::new(peripherals.LPWR);
    
    // Disable the watchdog task.
    rtc.rwdt.disable();

    // Initialize the i2c bus.
    let i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
        .expect("i2c initilization failed.")
        .with_sda(peripherals.GPIO3)
        .with_scl(peripherals.GPIO2);

    // Derive a RefCellDevice using a static reference via leaking a box to a RefCell containing an i2c bus device handle.
    let i2c_ref = RefCell::new(i2c);
    let i2c_ref_boxed = Box::new(i2c_ref);
    let static_i2c_ref: &'static mut RefCell<I2c<'_, esp_hal::Blocking>> = Box::leak(i2c_ref_boxed);

    // Initialize the power management system.
    // trim_adc_channels() to save a tiny bit of power.
    let mut power = Axp2101::new(RefCellDevice::new(static_i2c_ref));
    let _ = power.init();
    let _ = power.trim_adc_channels();

    // Initialize flash storage.
    let flash_storage = Nvs::new(0x9000, 0x14000, FlashStorage::new(peripherals.FLASH))
        .expect("Flash storage initilization failed.");

    let flash_storage_cell = FLASH_STORAGE_CELL.init(CriticalSectionMutex::new(RefCell::new(flash_storage)));

    let mut settings = Settings::new(flash_storage_cell).init();

    // If we woke up on a timer, check the battery charge.
    // If the battery charge is less than SLEEP_BATTERY_PERCENT, go back to sleep.
    // Otherwise set the local timestamp from RTC and reset the timestamp offset.
    if let SleepSource::Timer = wakeup_cause() {
        if power.get_battery_percent().unwrap_or(0) <= SLEEP_BATTERY_PERCENTAGE {
            rtc.sleep_deep(&[&TimerWakeupSource::new(Duration::from_secs(SLEEP_SECONDS_FOR_CHARING))]);
        } else {
            settings.set_timestamp(rtc.current_time_us() as i64);
            settings.set_timestamp_offset(Instant::now().as_micros());
        }
    }

    let rtc_cell = RTC_CELL.init(CriticalSectionMutex::new(RefCell::new(rtc)));
    let power_cell = POWER_CELL.init(CriticalSectionMutex::new(RefCell::new(power)));

    // Initialize SPI bus.
    let spi_config = SpiConfig::default()
        .with_frequency(Rate::from_mhz(80))
        .with_mode(SpiMode::_0);

    let (rx_buf, rx_desc, tx_buf, tx_desc) = dma_buffers!(8000);
    let dma_rx = DmaRxBuf::new(rx_desc, rx_buf).unwrap();
    let dma_tx = DmaTxBuf::new(tx_desc, tx_buf).unwrap();
    let spi = Spi::new(peripherals.SPI2, spi_config)
        .expect("SPI initilization failed.")
        .with_sck(peripherals.GPIO40)
        .with_sio0(peripherals.GPIO38)
        .with_sio1(peripherals.GPIO39)
        .with_sio2(peripherals.GPIO42)
        .with_sio3(peripherals.GPIO45)
        .with_dma(peripherals.DMA_CH0)
        .with_buffers(dma_rx, dma_tx);

    // Initialize the display.
    let cs = Output::new(peripherals.GPIO41, Level::High, OutputConfig::default());
    let reset = Output::new(peripherals.GPIO37, Level::High, OutputConfig::default());
    let mut display = Co5300Display::new(QspiBus::new(spi, cs), reset);

    display.init();

    // Enable Tearing Effect output on CO5300 (TE pin is GPIO13)
    // Using command 0x35 (TEARON) and param 0x00 (VBlank only)
    display.bus_mut().write_c8d8(0x35, 0x00);

    let te_pin = Input::new(peripherals.GPIO13, InputConfig::default());

    // Initilize the framebuffer.
    let mut framebuffer = Framebuffer::new();

    framebuffer.clear_color(Rgb565::BLACK);
    framebuffer.flush(&mut display);

    // Initialize the touch system.
    let mut touch = BlockingCST92xx::new(RefCellDevice::new(static_i2c_ref), 0x1A, Delay::new());
    let _ = touch.init();

    let touch_cell = TOUCH_CELL.init(CriticalSectionMutex::new(RefCell::new(touch)));

    // Initialize the rendering window.
    let software_window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);

    software_window.set_size(slint::PhysicalSize::new(LCD_WIDTH as u32, LCD_HEIGHT as u32));

    // Initialize the Slint platform.
    let platform = EmbassySlintPlatform::new(software_window.clone());

    slint::platform::set_platform(alloc::boxed::Box::new(platform))
        .expect("Slint platform initilization failed.");

    // Initialize the Slint UI window.
    let main_window = MainWindow::new()
        .expect("Could not create window.");

    // Set the display brightness via settings.
    display.set_brightness(settings.get_display_brightness());

    let settings_cell = SETTINGS_CELL.init(CriticalSectionMutex::new(RefCell::new(settings)));
    let display_cell = DISPLAY_CELL.init(CriticalSectionMutex::new(RefCell::new(display)));

    // Set the UTC date while preserving the localized time.
    main_window.on_set_date(|month, day, year| {
        let date_time = critical_section::with(|cs| {
            get_date_time(&settings_cell.borrow(cs).borrow())
        });

        let adjusted_datetime = date_time
            .with_month(month as u32)
            .unwrap()
            .with_day(day as u32)
            .unwrap()
            .with_year(year)
            .unwrap()
            .to_utc();

        settings_cell.lock(|settings| {
            let mut settings = settings.borrow_mut();

            settings.set_timestamp(adjusted_datetime.timestamp_micros());
            settings.set_timestamp_offset(Instant::now().as_micros());
        });
    });

    // Get localized date.
    main_window.on_get_date(|| {
        let date_time = settings_cell.lock(|settings| {
            get_date_time(&settings.borrow())
        });

        let model: Rc<VecModel<i32>> = Rc::new(VecModel::from(vec![
            date_time.day() as i32,
            date_time.month() as i32,
            date_time.year(),
        ]));

        ModelRc::from(model.clone())
    });

    // Set UTC time while preserving the localized date.
    main_window.on_set_time(|hour, minute, second| {
        let date_time = critical_section::with(|cs| {
            get_date_time(&settings_cell.borrow(cs).borrow())
        });

        let adjusted_datetime = date_time
            .with_hour(hour as u32)
            .unwrap_or_default()
            .with_minute(minute as u32)
            .unwrap_or_default()
            .with_second(second as u32)
            .unwrap_or_default()
            .to_utc();

        settings_cell.lock(|settings| {
            let mut settings = settings.borrow_mut();

            settings.set_timestamp(adjusted_datetime.timestamp_micros());
            settings.set_timestamp_offset(Instant::now().as_micros());
        });
    });

    // Get localized time.
    main_window.on_get_time(|| {
        let date_time = settings_cell.lock(|settings| {
            get_date_time(&settings.borrow())
        });

        let model: Rc<VecModel<i32>> = Rc::new(VecModel::from(vec![
            date_time.hour() as i32,
            date_time.minute() as i32,
            date_time.second() as i32,
        ]));

        ModelRc::from(model.clone())
    });

    // Set the timezone offset.
    main_window.on_set_timezone_offset(|offset| {
        settings_cell.lock(|settings| {
            settings.borrow_mut().set_timezone_offset(offset);
        });
    });

    // Get the timezone offset.
    main_window.on_get_timezone_offset(|| {
        settings_cell.lock(|settings| {
            settings.borrow().get_timezone_offset()
        })
    });

    // Set the screen brightness setting and change the display brightness.
    main_window.on_set_screen_brightness(|brightness| {
        settings_cell.lock(|settings| {
            settings.borrow_mut().set_display_brightness(brightness as u8);
        });

        display_cell.lock(|display| {
            display.borrow_mut().set_brightness(brightness as u8);
        });
    });

    // Set the screen brightness setting.
    main_window.on_get_screen_brightness(|| {
        settings_cell.lock(|settings| {
            settings.borrow().get_display_brightness() as i32
        })
    });

    // Set the screen timeout setting.
    main_window.on_set_screen_timeout(|timeout| {
        settings_cell.lock(|settings| {
            settings.borrow_mut().set_display_timeout(timeout as u8);
        });
    });

    // Get the screen timeout setting.
    main_window.on_get_screen_timeout(|| {
        settings_cell.lock(|settings| {
            settings.borrow().get_display_timeout() as i32
        })
    });

    // Set the smart glasses scan duration.
    main_window.on_set_smart_glasses_scan_duration(|duration| {
        settings_cell.lock(|settings| {
            settings.borrow_mut().set_smart_glasses_scan_duration(duration as u8);
        });
    });

    // Get the smart glasses scan duration.
    main_window.on_get_smart_glasses_scan_duration(|| {
        settings_cell.lock(|settings| {
            settings.borrow().get_smart_glasses_scan_duration() as i32
        })
    });

    // Set the Remote Id scan duration.
    main_window.on_set_remote_id_scan_duration(|duration| {
        settings_cell.lock(|settings| {
            settings.borrow_mut().set_remote_id_scan_duration(duration as u8);
        });
    });

    // Get the Remote Id scan duration.
    main_window.on_get_remote_id_scan_duration(|| {
        settings_cell.lock(|settings| {
            settings.borrow().get_remote_id_scan_duration() as i32
        })
    });

    // Get the number of days in month for a year.
    main_window.on_get_days_in_year_month(|year, month| {
        NaiveDate::from_ymd_opt(
            year,
            month as u32,
            1,
        )
            .unwrap_or_default()
            .num_days_in_month() as i32
    });

    // Issue a Remote Id scan task command.
    main_window.on_set_remote_id_scan_task_command(|command| {
        REMOTE_ID_SCAN_TASK_COMMAND.signal(command);
    });

    // Issue a smart glasses scan task command.
    main_window.on_set_smart_glasses_scan_task_command(|command| {
        SMART_GLASSES_SCAN_TASK_COMMAND.signal(command);
    });

    // Set the flashlight status.
    main_window.on_set_flashlight_on(|on| {
        if let Ok(mut flashlight) = FLASHLIGHT_ON.try_lock() {
            *flashlight = on;
        }
    });

    spawner.spawn(battery_status_task(power_cell, rtc_cell, settings_cell).unwrap());
    spawner.spawn(touch_event_update_task(touch_cell).unwrap());
    spawner.spawn(date_time_update_task(settings_cell).unwrap());
    spawner.spawn(display_timeout_countdown_task(display_cell, settings_cell).unwrap());
    spawner.spawn(remote_id_sniffing_task(settings_cell).unwrap());
    spawner.spawn(smart_glasses_scan_task(settings_cell).unwrap());

    let mut last_smart_glasses_detection: Option<Instant> = None;
    let mut last_remote_id_detection: Option<Instant> = None;

    main_window.show().unwrap();

    loop {
        Timer::after_millis(16).await;

        slint::platform::update_timers_and_animations();

        // Don't render a frame if the display is off
        if !*DISPLAY_ON.lock().await { continue; }

        if let Some(touch_event) = DISPLAY_TOUCH_EVENT_UPDATED.try_take() {
            software_window.dispatch_event(touch_event);
        }

        // Fixme

        if let Ok(remote_id_scan_task_state) = REMOTE_ID_SCAN_TASK_STATE.try_lock() {
            main_window.set_remote_id_scan_task_state(*remote_id_scan_task_state);
        }

        if let Some(remote_id_detected) = REMOTE_ID_DETECTED.try_take() {
            last_remote_id_detection = Some(remote_id_detected);

            main_window.set_remote_id_detected(true);
        } else {
            if let Some(detection) = last_remote_id_detection && detection.elapsed().as_secs() >= 30 {
                last_remote_id_detection = None;

                main_window.set_remote_id_detected(false);
            }
        }

        if let Ok(smart_glasses_scan_task_state) = SMART_GLASSES_SCAN_TASK_STATE.try_lock() {
            main_window.set_smart_glasses_scan_task_state(*smart_glasses_scan_task_state);
        }

        if let Some(smart_glasses_detected) = SMART_GLASSES_DETECTED.try_take() {
            last_smart_glasses_detection = Some(smart_glasses_detected);

            main_window.set_smart_glasses_detected(true);
        } else {
            if let Some(detection) = last_smart_glasses_detection && detection.elapsed().as_secs() >= 30 {
                last_smart_glasses_detection = None;

                main_window.set_smart_glasses_detected(false);
            }
        }

        if let Some(date_time) = DATE_TIME_UPDATED.try_take() {
            main_window.invoke_update_datetime(
                date_time.hour() as i32,
                date_time.minute() as i32,
                date_time.second() as i32,
                date_time.day() as i32,
                date_time.month() as i32,
                date_time.year(),
                date_time.weekday() as i32
            );
        }

        if let Some(battery_status) = BATTERY_STATUS_UPDATED.try_take() {
            main_window.invoke_update_battery_status(
                battery_status.0 as i32, // Battery charge percentage
                battery_status.1 // Is charging
            );
        }

        if software_window.draw_if_needed(|renderer| {
            renderer.render(framebuffer.as_rgb565_pixels_mut(), LCD_WIDTH as usize);
        }) {
            display_cell.lock(|display| {
                framebuffer.flush_vsync(&mut display.borrow_mut(), &te_pin);
            });
        }
    }
}

#[task]
async fn touch_event_update_task(touch_cell: &'static CriticalSectionMutex<RefCell<BlockingCST92xx<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>, Delay>>>) {
    let mut last_touch_point: Option<TouchPoint> = None;

    loop {
        if let Ok(touches) = touch_cell.lock(|touch| {
            touch.borrow_mut().touches()
        }) {
            // We only care about one-finger touches.
            if let Some(Some(touch_point)) = touches.first() {
                DISPLAY_TOUCHED.signal(Instant::now());

                DISPLAY_TOUCH_EVENT_UPDATED.signal(
                    if last_touch_point.is_some(){
                        WindowEvent::PointerMoved {
                            position: LogicalPosition::new(touch_point.x as f32, touch_point.y as f32)
                        }
                    } else {
                        WindowEvent::PointerPressed {
                            position: LogicalPosition::new(touch_point.x as f32, touch_point.y as f32),
                            button: PointerEventButton::Left
                        }
                    }  
                );

                last_touch_point = Some(*touch_point);
            } else if let Some(touch_point) = last_touch_point {
                DISPLAY_TOUCH_EVENT_UPDATED.signal(
                    WindowEvent::PointerReleased {
                        position: LogicalPosition::new(touch_point.x as f32, touch_point.y as f32), 
                        button: PointerEventButton::Left 
                    }
                );

                last_touch_point = None;
            }
        }

        Timer::after_millis(16).await;
    }
}

#[task]
async fn date_time_update_task(settings_cell: &'static CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>) {
    let mut last_date_time = DateTime::UNIX_EPOCH.fixed_offset();

    loop {
        let date_time = settings_cell.lock(|settings| {
            get_date_time(&settings.borrow())
        });

        if date_time != last_date_time {
            DATE_TIME_UPDATED.signal(date_time);

            last_date_time = date_time;
        }

        Timer::after_millis(16).await;
    }
}

#[task]
async fn battery_status_task(power_cell: &'static CriticalSectionMutex<RefCell<Axp2101<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>>>>, rtc_cell: &'static CriticalSectionMutex<RefCell<Rtc<'static>>>, settings_cell: &'static CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>) {
    let mut last_battery_status: (u8, bool) = (0, false);

    loop {
        let battery_status = power_cell.lock(|power| {
            let mut power = power.borrow_mut();

            (
                power.get_battery_percent().unwrap_or(0),

                // is_charging() has a bug, use vbus and voltage to determine charging status.
                power.is_vbus_in().unwrap_or(false) && power.get_battery_voltage().unwrap_or(0) < 4150
            )
        });

        // Graceful shutdown if battery has SLEEP_BATTERY_PERCENT charge or less.
        if battery_status.0 <= SLEEP_BATTERY_PERCENTAGE {
            SMART_GLASSES_SCAN_TASK_COMMAND.signal(SmartGlassesScanTaskCommand::Stop);
            REMOTE_ID_SCAN_TASK_COMMAND.signal(RemoteIdScanTaskCommand::Stop);

            let date_time = settings_cell.lock(|settings| {
                get_date_time(&settings.borrow_mut())
            });

            rtc_cell.lock(|rtc| {
                let mut rtc = rtc.borrow_mut();

                // Update the RTC with the current time.
                rtc.set_current_time_us(date_time.timestamp_micros() as u64);
                rtc.sleep_deep(&[&TimerWakeupSource::new(Duration::from_secs(10))]);
            });
        }

        // Signal the UI with the battery level and charge state
        if battery_status != last_battery_status {
            BATTERY_STATUS_UPDATED.signal(battery_status);

            last_battery_status = battery_status;
        }

        Timer::after_secs(10).await
    }
}

#[task]
async fn display_timeout_countdown_task(display_cell: &'static CriticalSectionMutex<RefCell<Co5300Display<'static>>>, settings_cell: &'static CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>) {
    let mut last_touch_instant = Instant::now();

    loop {
        if let Some(touch_instant) = DISPLAY_TOUCHED.try_take() {
            last_touch_instant = touch_instant;

            let mut display_on = DISPLAY_ON.lock().await;

            if !*display_on {
                display_cell.lock(|display| {
                    display.borrow_mut().display_on();
                    *display_on = true
                });
            }
        } else if !*FLASHLIGHT_ON.lock().await {
            let display_timeout = settings_cell.lock(|settings| {
                settings.borrow().get_display_timeout()
            });

            let mut display_on = DISPLAY_ON.lock().await;

            if Instant::now().duration_since(last_touch_instant).as_secs() > display_timeout as u64 && *display_on {
                display_cell.lock(|display| {
                    display.borrow_mut().display_off();
                });

                *display_on = false;
            }
        }

        Timer::after_millis(16).await;
    }
}

const SMART_GLASSES_BLE_COMPANY_IDENTIFIERS: [u16; 3] = [
    0x01AB, // Meta Platforms
    0x058E, // Meta Platforms Technologies
    0x0D53, // Luxottica
];

struct SmartGlassesScanHandler {}

impl EventHandler for SmartGlassesScanHandler {
    fn on_adv_reports(&self, mut it: LeAdvReportsIter<'_>) {
        while let Some(Ok(report)) = it.next() {
            let mut decoder = AdStructure::decode(report.data);

            while let Some(Ok(structure)) = decoder.next() {
                if let AdStructure::ManufacturerSpecificData{ company_identifier, payload: _ } = structure &&
                SMART_GLASSES_BLE_COMPANY_IDENTIFIERS.contains(&company_identifier) {
                    SMART_GLASSES_DETECTED.signal(Instant::now());
                }
            }
        }
    }
}

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 4;

#[task]
async fn smart_glasses_scan_task(settings_cell: &'static CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>) {
    join(
        async {
            loop {
                if SmartGlassesScanTaskCommand::Start == SMART_GLASSES_SCAN_TASK_COMMAND.wait().await {
                    select(
                        async {
                            let bluetooth_peripheral = unsafe { BT::steal() };

                            let ble_connector = BleConnector::new(bluetooth_peripheral, Default::default()).unwrap();
                            let external_controller: ExternalController<_, 1> = ExternalController::new(ble_connector);
                            let address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xff]);

                            let mut host_resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
                            let stack = trouble_host::new(external_controller, &mut host_resources).set_random_address(address);

                            let Host { central, mut runner, .. } = stack.build();
                            let mut scanner = Scanner::new(central);
                            let scan_config = ScanConfig::default();
                            let ble_scan_handler = SmartGlassesScanHandler{};

                            *SMART_GLASSES_SCAN_TASK_STATE.lock().await = SmartGlassesScanTaskState::Running;

                            let _ = join(
                                runner.run_with_handler(&ble_scan_handler),
                                scanner.scan(&scan_config)
                            )
                                .await;
                        },
                        async {
                            loop {
                                if SmartGlassesScanTaskCommand::Stop == SMART_GLASSES_SCAN_TASK_COMMAND.wait().await {
                                    return;
                                }
                            }
                        }
                    )
                        .await;
                    
                    *SMART_GLASSES_SCAN_TASK_STATE.lock().await = SmartGlassesScanTaskState::Stopped;
                }
            }
        },
        async {
            loop {
                if *SMART_GLASSES_SCAN_TASK_STATE.lock().await == SmartGlassesScanTaskState::Running {
                    let scan_duration = settings_cell.lock(|settings| {
                        settings.borrow().get_smart_glasses_scan_duration()
                    }) as u64;

                    Timer::after_secs(scan_duration).await;

                    if *SMART_GLASSES_SCAN_TASK_STATE.lock().await == SmartGlassesScanTaskState::Running {
                        SMART_GLASSES_SCAN_TASK_COMMAND.signal(SmartGlassesScanTaskCommand::Stop);
                    }
                }

                Timer::after_millis(16).await;
            }
        }
    ).await;
}

static REMOTE_ID_PACKET_CHANNEL: Channel<CriticalSectionRawMutex, &[u8], 8> = Channel::new();

#[task]
async fn remote_id_sniffing_task(settings_cell: &'static CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>) {
    let last_wifi_controller_mutex: Mutex<CriticalSectionRawMutex, Option<WifiController>> = Mutex::new(None);
    let wifi_channel_mutex: Mutex<CriticalSectionRawMutex, u8> = Mutex::new(1);

    join3(
        async {
            loop {
                match REMOTE_ID_SCAN_TASK_COMMAND.wait().await {
                    RemoteIdScanTaskCommand::Start => {
                        let wifi_peripheral = unsafe { esp_hal::peripherals::WIFI::steal() };

                        let (wifi_controller, wifi_interfaces) = esp_radio::wifi::new(
                            wifi_peripheral, 
                            Default::default()
                        )
                            .unwrap();

                        let mut wifi_sniffer = wifi_interfaces.sniffer;

                        wifi_sniffer.set_receive_cb(|packet| {
                            let _ = match_frames! {
                                packet.data,
                                beacon = BeaconFrame => {
                                    for element in beacon.body.elements.get_matching_elements::<VendorSpecificElement>() {
                                        if element.get_payload_if_prefix_matches(&[0xFA, 0x0B, 0xBC]).is_some() {
                                            REMOTE_ID_DETECTED.signal(Instant::now());
                                        }
                                    }
                                }
                                action = RawActionFrame => {
                                    if action.body.is_vendor_and_matches([0xFA, 0x0B, 0xBC]) {
                                        REMOTE_ID_DETECTED.signal(Instant::now());
                                    }
                                }
                            };
                        });

                        let _ = wifi_sniffer.set_promiscuous_mode(true);

                        last_wifi_controller_mutex.lock().await.replace(wifi_controller);

                        *REMOTE_ID_SCAN_TASK_STATE.lock().await = RemoteIdScanTaskState::Running;
                    }
                    RemoteIdScanTaskCommand::Stop => {
                        *last_wifi_controller_mutex.lock().await = None;
                        *wifi_channel_mutex.lock().await = 1;
                        *REMOTE_ID_SCAN_TASK_STATE.lock().await = RemoteIdScanTaskState::Stopped;
                    }
                }
            }
        },
        async {
            loop {
                if *REMOTE_ID_SCAN_TASK_STATE.lock().await == RemoteIdScanTaskState::Running {
                    let scan_duration = settings_cell.lock(|settings| {
                        settings.borrow().get_remote_id_scan_duration()
                    }) as u64;

                    Timer::after_secs(scan_duration).await;

                    if *REMOTE_ID_SCAN_TASK_STATE.lock().await == RemoteIdScanTaskState::Running {
                        REMOTE_ID_SCAN_TASK_COMMAND.signal(RemoteIdScanTaskCommand::Stop);
                    }
                }

                Timer::after_millis(16).await;
            }
        },
        async {
            loop {
                if let Some(wifi_controller) = last_wifi_controller_mutex.lock().await.as_mut() {
                    let mut wifi_channel = wifi_channel_mutex.lock().await;

                    let _ = wifi_controller.set_channel(*wifi_channel, SecondaryChannel::None);

                    if *wifi_channel == 14 { *wifi_channel = 1 } else { *wifi_channel += 1 };
                }

                Timer::after_secs(1).await;
            }
        }
    ).await;
}

fn get_date_time(settings: &Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>) -> DateTime<FixedOffset> {
    let timestamp = settings.get_timestamp();
    let timestamp_offset = settings.get_timestamp_offset();
    let timezone_offset = settings.get_timezone_offset();
    let now_ticks: u64 = Instant::now().as_micros();
    let elapsed_micros = now_ticks.saturating_sub(timestamp_offset);

    DateTime::from_timestamp_micros(timestamp + (elapsed_micros as i64))
        .unwrap()
        .with_timezone(&FixedOffset::east_opt(3600 * timezone_offset).unwrap())
}