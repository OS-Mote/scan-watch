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
    delay::Delay,
    dma::{
        DmaRxBuf,
        DmaTxBuf
    },
    dma_buffers,
    gpio::{
        Input,
        InputConfig,
        Level,
        Output,
        OutputConfig
    },
    i2c::master::{
        Config as I2cConfig,
        I2c
    }, 
    peripherals::{
        BT
    },
    rtc_cntl::{
        Rtc,
        sleep::{
            TimerWakeupSource,
        },
    },
    spi::{
        Mode as SpiMode,
        master::{
            Config as SpiConfig,
            Spi
        }
    },
    system::{
        SleepSource,
        wakeup_cause
    },
    time::Rate,
    timer::timg::TimerGroup
};
use esp_storage::FlashStorage;
use esp_nvs::{
    Nvs
};
use esp_radio::{
    ble::controller::BleConnector,
    wifi::{
        SecondaryChannel
    }
};
use embassy_sync::{
    mutex::Mutex,
    blocking_mutex::{
        CriticalSectionMutex,
        raw::CriticalSectionRawMutex
    },
    signal::Signal
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
    prelude::*,
    pixelcolor::Rgb565
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
use drv2605::{
    Drv2605,
    Effect
};
use cst92xx::{
    BlockingCST92xx,
    Point as TouchPoint
};

mod qspi_bus;
mod framebuffer;
mod axp2101;
mod co5300;
mod settings;
mod i2c_proxy_v0_2;

use crate::{
    settings::{
        Settings,
        SCAN_ALERT_DURATION
    },
    axp2101::Axp2101,
    co5300::{
        Co5300Display,
        LCD_WIDTH,
        LCD_HEIGHT
    },
    framebuffer::Framebuffer,
    qspi_bus::QspiBus,
    i2c_proxy_v0_2::I2cProxyV0_2
};

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {}", info);
    loop {}
}

esp_bootloader_esp_idf::esp_app_desc!();

slint::include_modules!();

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

static RTC_STATIC_CELL: StaticCell<CriticalSectionMutex<RefCell<Rtc<'static>>>> = StaticCell::new();
static POWER_STATIC_CELL: StaticCell<CriticalSectionMutex<RefCell<Axp2101<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>>>>> = StaticCell::new();
static FLASH_STORAGE_STATIC_CELL: StaticCell<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>> = StaticCell::new();
static DISPLAY_STATIC_CELL: StaticCell<CriticalSectionMutex<RefCell<Co5300Display<'static>>>> = StaticCell::new();
static TOUCH_STATIC_CELL: StaticCell<CriticalSectionMutex<RefCell<BlockingCST92xx<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>, Delay>>>> = StaticCell::new();
static HAPTIC_STATIC_CELL: StaticCell<CriticalSectionMutex<RefCell<Drv2605<I2cProxyV0_2>>>> = StaticCell::new();
static SETTINGS_STATIC_CELL: StaticCell<CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>> = StaticCell::new();

static BATTERY_STATUS_MUTEX: Mutex<CriticalSectionRawMutex, (u8, bool)> = Mutex::new((0, false));
static REMOTE_ID_SCAN_TASK_STATE_MUTEX: Mutex<CriticalSectionRawMutex, RemoteIdScanTaskState> = Mutex::new(RemoteIdScanTaskState::Stopped);
static REMOTE_ID_ALERT_MUTEX: Mutex<CriticalSectionRawMutex, bool> = Mutex::new(false);
static SMART_GLASSES_SCAN_TASK_STATE_MUTEX: Mutex<CriticalSectionRawMutex, SmartGlassesScanTaskState> = Mutex::new(SmartGlassesScanTaskState::Stopped);
static SMART_GLASSES_ALERT_MUTEX: Mutex<CriticalSectionRawMutex, bool> = Mutex::new(false);
static FLASHLIGHT_ON_MUTEX: Mutex<CriticalSectionRawMutex, bool> = Mutex::new(false);
static DISPLAY_ON_MUTEX: Mutex<CriticalSectionRawMutex, bool> = Mutex::new(true);

static REMOTE_ID_SCAN_TASK_COMMAND_SIGNAL: Signal<CriticalSectionRawMutex, RemoteIdScanTaskCommand> = Signal::new();
static REMOTE_ID_DETECTED_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static SMART_GLASSES_SCAN_TASK_COMMAND_SIGNAL: Signal<CriticalSectionRawMutex, SmartGlassesScanTaskCommand> = Signal::new();
static SMART_GLASSES_DETECTED_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static DISPLAY_TOUCHED_SIGNAL: Signal<CriticalSectionRawMutex, Instant> = Signal::new();
static DISPLAY_TOUCH_EVENT_SIGNAL: Signal<CriticalSectionRawMutex, WindowEvent> = Signal::new();
static DATE_TIME_UPDATED_SIGNAL: Signal<CriticalSectionRawMutex, DateTime<FixedOffset>> = Signal::new();

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

    // If we woke up on a timer, check the battery charge.
    // If the battery charge is less than SLEEP_BATTERY_PERCENT, go back to sleep.
    if let SleepSource::Timer = wakeup_cause() && power.get_battery_percent().unwrap_or(0) <= SLEEP_BATTERY_PERCENTAGE {
        rtc.sleep_deep(&[&TimerWakeupSource::new(Duration::from_secs(SLEEP_SECONDS_FOR_CHARING))]);
    }

    // Initialize flash storage.
    let flash_storage = Nvs::new(0x9000, 0x14000, FlashStorage::new(peripherals.FLASH))
        .expect("Flash storage initilization failed.");

    let flash_storage_static_cell = FLASH_STORAGE_STATIC_CELL.init(CriticalSectionMutex::new(RefCell::new(flash_storage)));

    // Use an embedded-hal 0.2 proxy to support the haptic motor.
    let i2c_v0_2_proxy = I2cProxyV0_2(static_i2c_ref);
    let mut haptic = Drv2605::new(i2c_v0_2_proxy);

    // Initialize the haptic motor.
    let _ = haptic.init_open_loop_erm();

    let haptic_static_cell = HAPTIC_STATIC_CELL.init(CriticalSectionMutex::new(RefCell::new(haptic)));

    let mut settings = Settings::new(flash_storage_static_cell).init();

    // Use the RTC clock to set the local timestamp after boot.
    if settings.get_timestamp_offset() == 0 {
        settings.set_timestamp(rtc.current_time_us() as i64);
        settings.set_timestamp_offset(Instant::now().as_micros());
    }

    let rtc_static_cell = RTC_STATIC_CELL.init(CriticalSectionMutex::new(RefCell::new(rtc)));
    let power_static_cell = POWER_STATIC_CELL.init(CriticalSectionMutex::new(RefCell::new(power)));

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

    // Enable Tearing Effect output on CO5300 (TE pin is GPIO13).
    // Using command 0x35 (TEARON) and param 0x00 (VBlank only).
    display.bus_mut().write_c8d8(0x35, 0x00);

    let te_pin = Input::new(peripherals.GPIO13, InputConfig::default());

    // Initilize the framebuffer.
    let mut framebuffer = Framebuffer::new();

    framebuffer.clear_color(Rgb565::BLACK);
    framebuffer.flush(&mut display);

    // Initialize the touch system.
    let mut touch = BlockingCST92xx::new(RefCellDevice::new(static_i2c_ref), 0x1A, Delay::new());
    let _ = touch.init();

    let touch_static_cell = TOUCH_STATIC_CELL.init(CriticalSectionMutex::new(RefCell::new(touch)));

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

    // Set visual preferences before showing the clock.
    main_window.set_dark_mode(settings.get_screen_dark_mode());
    main_window.set_clock_twelve_hour(settings.get_clock_twelve_hour());

    let settings_static_cell = SETTINGS_STATIC_CELL.init(CriticalSectionMutex::new(RefCell::new(settings)));
    let display_static_cell = DISPLAY_STATIC_CELL.init(CriticalSectionMutex::new(RefCell::new(display)));

    // Set the UTC date while preserving the localized time.
    main_window.on_set_date(|month, day, year| {
        let date_time = critical_section::with(|cs| {
            get_date_time(&settings_static_cell.borrow(cs).borrow())
        });

        let adjusted_datetime = date_time
            .with_month(month as u32)
            .unwrap_or_default()
            .with_day(day as u32)
            .unwrap_or_default()
            .with_year(year)
            .unwrap_or_default()
            .to_utc();

        settings_static_cell.lock(|settings_mutex| {
            let mut settings = settings_mutex.borrow_mut();

            settings.set_timestamp(adjusted_datetime.timestamp_micros());
            settings.set_timestamp_offset(Instant::now().as_micros());
        });

        rtc_static_cell.lock(|rtc_mutex| {
            rtc_mutex.borrow_mut().set_current_time_us(adjusted_datetime.timestamp_micros() as u64);
        });
    });

    // Get localized date.
    main_window.on_get_date(|| {
        let date_time = settings_static_cell.lock(|settings_mutex| {
            get_date_time(&settings_mutex.borrow())
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
            get_date_time(&settings_static_cell.borrow(cs).borrow())
        });

        let adjusted_datetime = date_time
            .with_hour(hour as u32)
            .unwrap_or_default()
            .with_minute(minute as u32)
            .unwrap_or_default()
            .with_second(second as u32)
            .unwrap_or_default()
            .to_utc();

        settings_static_cell.lock(|settings_mutex| {
            let mut settings = settings_mutex.borrow_mut();

            settings.set_timestamp(adjusted_datetime.timestamp_micros());
            settings.set_timestamp_offset(Instant::now().as_micros());
        });

        rtc_static_cell.lock(|rtc_mutex| {
            rtc_mutex.borrow_mut().set_current_time_us(adjusted_datetime.timestamp_micros() as u64);
        });
    });

    // Get localized time.
    main_window.on_get_time(|| {
        let date_time = settings_static_cell.lock(|settings_mutex| {
            get_date_time(&settings_mutex.borrow())
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
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow_mut().set_timezone_offset(offset);
        });
    });

    // Get the timezone offset.
    main_window.on_get_timezone_offset(|| {
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow().get_timezone_offset()
        })
    });

    // Set the clock twelve-hour setting.
    main_window.on_set_clock_twelve_hour(|clock_twelve_hour| {
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow_mut().set_clock_twelve_hour(clock_twelve_hour);
        });
    });

    // Set the screen brightness setting and change the display brightness.
    main_window.on_set_screen_brightness(|brightness| {
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow_mut().set_display_brightness(brightness as u8);
        });

        display_static_cell.lock(|display_mutex| {
            display_mutex.borrow_mut().set_brightness(brightness as u8);
        });
    });

    // Set the screen brightness setting.
    main_window.on_get_screen_brightness(|| {
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow().get_display_brightness() as i32
        })
    });

    // Set the screen timeout setting.
    main_window.on_set_screen_timeout(|timeout| {
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow_mut().set_display_timeout(timeout as u8);
        });
    });

    // Get the screen timeout setting.
    main_window.on_get_screen_timeout(|| {
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow().get_display_timeout() as i32
        })
    });

    // Set the screen dark mode setting.
    main_window.on_set_screen_dark_mode(|dark_mode| {
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow_mut().set_display_dark_mode(dark_mode);
        });
    });

    // Set the smart glasses scan duration.
    main_window.on_set_smart_glasses_scan_duration(|duration| {
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow_mut().set_smart_glasses_scan_duration(duration as u8);
        });
    });

    // Get the smart glasses scan duration.
    main_window.on_get_smart_glasses_scan_duration(|| {
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow().get_smart_glasses_scan_duration() as i32
        })
    });

    // Set the Remote Id scan duration.
    main_window.on_set_remote_id_scan_duration(|duration| {
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow_mut().set_remote_id_scan_duration(duration as u8);
        });
    });

    // Get the Remote Id scan duration.
    main_window.on_get_remote_id_scan_duration(|| {
        settings_static_cell.lock(|settings_mutex| {
            settings_mutex.borrow().get_remote_id_scan_duration() as i32
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
        REMOTE_ID_SCAN_TASK_COMMAND_SIGNAL.signal(command);
    });

    // Issue a smart glasses scan task command.
    main_window.on_set_smart_glasses_scan_task_command(|command| {
        SMART_GLASSES_SCAN_TASK_COMMAND_SIGNAL.signal(command);
    });

    // Set the flashlight status.
    main_window.on_set_flashlight_on(|on| {
        if let Ok(mut flashlight_mutex) = FLASHLIGHT_ON_MUTEX.try_lock() {
            // If the flashlight is on..
            if on {
                display_static_cell.lock(|display| {
                    // Set the display brightness to maximum.
                    display.borrow_mut().set_brightness(255);
                });
            // Else if the flashlight is off..
            } else {
                let brightness = settings_static_cell.lock(|settings_mutex| {
                    settings_mutex.borrow().get_display_brightness()
                });

                // Set the brightness to the user defined value.
                display_static_cell.lock(|display_mutex| {
                    display_mutex.borrow_mut().set_brightness(brightness);
                });
            }

            *flashlight_mutex = on;
        }
    });

    // Spawn tasks.
    spawner.spawn(battery_status_update_task(power_static_cell, rtc_static_cell, settings_static_cell).unwrap());
    spawner.spawn(touch_event_task(touch_static_cell, haptic_static_cell).unwrap());
    spawner.spawn(date_time_update_task(settings_static_cell).unwrap());
    spawner.spawn(display_timeout_countdown_task(display_static_cell, settings_static_cell).unwrap());
    spawner.spawn(remote_id_sniffing_task(settings_static_cell).unwrap());
    spawner.spawn(remote_id_alert_task(haptic_static_cell).unwrap());
    spawner.spawn(smart_glasses_scan_task(settings_static_cell).unwrap());
    spawner.spawn(smart_glasses_alert_task(haptic_static_cell).unwrap());

    main_window.show().unwrap();

    loop {
        slint::platform::update_timers_and_animations();

        // Dispatch any pending touch events.
        if let Some(touch_event) = DISPLAY_TOUCH_EVENT_SIGNAL.try_take() {
            software_window.dispatch_event(touch_event);
        }

        // Set the Remote Id scan task state on the main window.
        if let Ok(remote_id_scan_task_state) = REMOTE_ID_SCAN_TASK_STATE_MUTEX.try_lock() {
            main_window.set_remote_id_scan_task_state(*remote_id_scan_task_state);
        }

        // Set the Remote Id alert on the main window.
        if let Ok(remote_id_alert) = REMOTE_ID_ALERT_MUTEX.try_lock() {
            main_window.set_remote_id_detected(*remote_id_alert);
        }

        // Set the smart glasses scan task state on the main window.
        if let Ok(smart_glasses_scan_task_state) = SMART_GLASSES_SCAN_TASK_STATE_MUTEX.try_lock() {
            main_window.set_smart_glasses_scan_task_state(*smart_glasses_scan_task_state);
        }

        // Set the smart glasses alert on the main window.
        if let Ok(smart_glasses_alert) = SMART_GLASSES_ALERT_MUTEX.try_lock() {
            main_window.set_smart_glasses_detected(*smart_glasses_alert)
        }

        // Set the localized date and time on the main window.
        if let Some(date_time) = DATE_TIME_UPDATED_SIGNAL.try_take() {
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

        // Set battery status on the main window.
        if let Ok(battery_status) = BATTERY_STATUS_MUTEX.try_lock() {
            main_window.invoke_update_battery_status(
                battery_status.0 as i32, // Battery charge percentage
                battery_status.1 // Is charging
            );
        }

        // Draw UI updates and flush the framebuffer.
        if software_window.draw_if_needed(|renderer| {
            renderer.render(framebuffer.as_rgb565_pixels_mut(), LCD_WIDTH as usize);
        }) {
            display_static_cell.lock(|display_mutex| {
                framebuffer.flush_vsync(&mut display_mutex.borrow_mut(), &te_pin);
            });
        }

        Timer::after_millis(16).await;
    }
}

#[task]
async fn touch_event_task(touch_static_cell: &'static CriticalSectionMutex<RefCell<BlockingCST92xx<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>, Delay>>>, haptic_static_cell: &'static CriticalSectionMutex<RefCell<Drv2605<I2cProxyV0_2>>>) {
    let mut last_touch_point: Option<TouchPoint> = None;

    loop {
        if let Ok(touches) = touch_static_cell.lock(|touch_mutex| {
            touch_mutex.borrow_mut().touches()
        }) {
            // We only care about one-finger touches.
            if let Some(Some(touch_point)) = touches.first() {
                DISPLAY_TOUCHED_SIGNAL.signal(Instant::now());

                DISPLAY_TOUCH_EVENT_SIGNAL.signal(
                    // If we have a prior touch point the pointer has moved..
                    if last_touch_point.is_some(){
                        WindowEvent::PointerMoved {
                            position: LogicalPosition::new(touch_point.x as f32, touch_point.y as f32)
                        }
                    // Else this is a new touch.
                    } else {
                        // Use the haptic motor to send a "click" vibration to the user.
                        haptic_static_cell.lock(|haptic_mutex| {
                            let mut haptic = haptic_mutex.borrow_mut();

                            let _ = haptic.set_single_effect(Effect::StrongClick100);
                            let _ = haptic.set_go(true);
                        });

                        WindowEvent::PointerPressed {
                            position: LogicalPosition::new(touch_point.x as f32, touch_point.y as f32),
                            button: PointerEventButton::Left
                        }
                    }  
                );

                last_touch_point = Some(*touch_point);
            // If there are no touches but we have a prior touch point this is the end of the touch.
            } else if let Some(touch_point) = last_touch_point {
                DISPLAY_TOUCH_EVENT_SIGNAL.signal(
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
async fn date_time_update_task(settings_static_cell: &'static CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>) {
    loop {
        let date_time = settings_static_cell.lock(|settings_mutex| {
            get_date_time(&settings_mutex.borrow())
        });

        DATE_TIME_UPDATED_SIGNAL.signal(date_time);

        Timer::after_millis(250).await;
    }
}

#[task]
async fn battery_status_update_task(power_static_cell: &'static CriticalSectionMutex<RefCell<Axp2101<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>>>>, rtc_static_cell: &'static CriticalSectionMutex<RefCell<Rtc<'static>>>, settings_static_cell: &'static CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>) {
    let mut last_battery_status: (u8, bool) = (0, false);

    loop {
        // Get the battery status as charge percentage and charge status.
        let battery_status = power_static_cell.lock(|power_mutex| {
            let mut power = power_mutex.borrow_mut();

            (
                power.get_battery_percent().unwrap_or(0),

                // is_charging() has a bug, use vbus and voltage to determine charging status.
                power.is_vbus_in().unwrap_or(false) && power.get_battery_voltage().unwrap_or(0) < 4150
            )
        });

        // Gracefully shutdown if battery has SLEEP_BATTERY_PERCENT charge or less..
        if battery_status.0 <= SLEEP_BATTERY_PERCENTAGE {
            // Stop and wait for scans to complete with a join between..
            join(
                // Wait for the smart glasses scan to stop..
                async {
                    SMART_GLASSES_SCAN_TASK_COMMAND_SIGNAL.signal(SmartGlassesScanTaskCommand::Stop);

                    loop {
                        if *SMART_GLASSES_SCAN_TASK_STATE_MUTEX.lock().await == SmartGlassesScanTaskState::Stopped {
                            return;
                        }
                    }
                },
                // And wait for the remote id scan to stop.
                async {
                    REMOTE_ID_SCAN_TASK_COMMAND_SIGNAL.signal(RemoteIdScanTaskCommand::Stop);

                    loop {
                        if *REMOTE_ID_SCAN_TASK_STATE_MUTEX.lock().await == RemoteIdScanTaskState::Stopped {
                            return;
                        }
                    }
                }
            ).await;

            let date_time = settings_static_cell.lock(|settings_mutex| {
                get_date_time(&settings_mutex.borrow_mut())
            });

            rtc_static_cell.lock(|rtc_mutex| {
                let mut rtc = rtc_mutex.borrow_mut();

                // Update the RTC with the current time.
                rtc.set_current_time_us(date_time.timestamp_micros() as u64);
                rtc.sleep_deep(&[&TimerWakeupSource::new(Duration::from_secs(10))]);
            });
        }
        // Otherwise signal the UI with the battery level and charge state if they have changed.
        else if battery_status != last_battery_status {
            *BATTERY_STATUS_MUTEX.lock().await = battery_status;

            last_battery_status = battery_status;
        }

        Timer::after_secs(10).await
    }
}

#[task]
async fn display_timeout_countdown_task(display_static_cell: &'static CriticalSectionMutex<RefCell<Co5300Display<'static>>>, settings_static_cell: &'static CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>) {
    let mut last_touch_instant = Instant::now();

    loop {
        if let Ok(mut display_on) = DISPLAY_ON_MUTEX.try_lock() {
            // If the display has been touched..
            if let Some(touch_instant) = DISPLAY_TOUCHED_SIGNAL.try_take() {
                last_touch_instant = touch_instant;

                // And the display is not on..
                if !*display_on {
                    display_static_cell.lock(|display_mutex| {
                        // Turn on the display..
                        display_mutex.borrow_mut().display_on();
                    });

                    // Set the display on flag.
                    *display_on = true
                }
            // Else start the display time-out countdown if the flashlight is not on and the time-out is positive.
            } else if !*FLASHLIGHT_ON_MUTEX.lock().await {
                let display_timeout = settings_static_cell.lock(|settings_mutex| {
                    settings_mutex.borrow().get_display_timeout()
                });

                // If the display has been on longer or equal to the display timeout setting..
                if Instant::now().duration_since(last_touch_instant).as_secs() >= display_timeout as u64 && *display_on {
                    display_static_cell.lock(|display_mutex| {
                        // Turn off the display..
                        display_mutex.borrow_mut().display_off();
                    });

                    // And set the display on flag.
                    *display_on = false;
                }
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
    // When a Bluetooth advertising reports have been detected..
    fn on_adv_reports(&self, mut it: LeAdvReportsIter<'_>) {
        // Iterate through the reports.
        while let Some(Ok(report)) = it.next() {
            // Decode the report data.
            let mut decoder = AdStructure::decode(report.data);

            // Iterate through the decoded data.
            while let Some(Ok(structure)) = decoder.next() {
                // Match the Bluetooth device's company identifier to company identifiers of smart glasses manufacturers.
                if let AdStructure::ManufacturerSpecificData{ company_identifier, payload: _ } = structure &&
                SMART_GLASSES_BLE_COMPANY_IDENTIFIERS.contains(&company_identifier) {
                    // Signal the instant smart glasses have been detected.
                    SMART_GLASSES_DETECTED_SIGNAL.signal(());
                }
            }
        }
    }
}

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 4;

#[task]
async fn smart_glasses_scan_task(settings_static_cell: &'static CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>) {
    loop {
        if SmartGlassesScanTaskCommand::Start == SMART_GLASSES_SCAN_TASK_COMMAND_SIGNAL.wait().await {
            // Steal the Bluetooth peripheral.
            // It will be freed for re-use when it goes out of scope.
            let bluetooth_peripheral = unsafe { BT::steal() };

            // Compose the Bluetooth stack
            let ble_connector = BleConnector::new(bluetooth_peripheral, Default::default()).unwrap();
            let external_controller: ExternalController<_, 1> = ExternalController::new(ble_connector);
            let address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xff]);
            let mut host_resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
            let stack = trouble_host::new(external_controller, &mut host_resources).set_random_address(address);

            // Build the Bluetooth stack.
            let Host {
                central, 
                mut runner,
                ..
            } = stack.build();

            // Set up the Bluetooth scanner with configuration and handler.
            let mut scanner = Scanner::new(central);
            let scan_config = ScanConfig::default();
            let ble_scan_handler = SmartGlassesScanHandler{};

            // Set the smart glasses scan state as running.
            *SMART_GLASSES_SCAN_TASK_STATE_MUTEX.lock().await = SmartGlassesScanTaskState::Running;

            // Select between..
            let _ = select(
                // a join between..
                join(
                    // The Bluetooth runner with handler future..
                    runner.run_with_handler(&ble_scan_handler),
                    // And the scanner future.
                    scanner.scan(&scan_config)
                ),
                // And a select between..
                select(
                    // The scan duration future..
                    async {
                        let scan_duration = settings_static_cell.lock(|settings_mutex| {
                            settings_mutex.borrow().get_smart_glasses_scan_duration()
                        }) as u64;

                        Timer::after_secs(scan_duration).await;
                    },
                    // And the stop command signal future.
                    async {
                        loop {
                            if SmartGlassesScanTaskCommand::Stop == SMART_GLASSES_SCAN_TASK_COMMAND_SIGNAL.wait().await {
                                return;
                            }
                        }
                    }
                )
            )
                .await;

            // Set the smart glasses scan state as stopped.
            *SMART_GLASSES_SCAN_TASK_STATE_MUTEX.lock().await = SmartGlassesScanTaskState::Stopped;
        }
    }
}

#[task]
async fn smart_glasses_alert_task(haptic_static_cell: &'static CriticalSectionMutex<RefCell<Drv2605<I2cProxyV0_2>>>) {
    loop {
        // Wait for a smart glasses detection signal.
        SMART_GLASSES_DETECTED_SIGNAL.wait().await;

        // Set the smart glasses alert as true.
        *SMART_GLASSES_ALERT_MUTEX.lock().await = true;

        // Select between..
        let _ = select(
            // Repeatedly triggering a haptic alert..
            async {
                haptic_static_cell.lock(|haptic_mutex| {
                    let mut haptic = haptic_mutex.borrow_mut();

                    let _ = haptic.set_single_effect(Effect::LongDoubleSharpClickStrongTwo80);
                    let _ = haptic.set_go(true);
                });

                Timer::after_millis(250).await;
            },
            // And a select between..
            select(
                // Waiting for SCAN_ALERT_DURATION since the smart glasses detection
                async {
                    loop {
                        Timer::after_secs(SCAN_ALERT_DURATION).await;
                    }
                },
                async {
                    // Waiting for the smart glasses scan task state to be Stopped.
                    loop {
                        if *SMART_GLASSES_SCAN_TASK_STATE_MUTEX.lock().await == SmartGlassesScanTaskState::Stopped {
                            return;
                        }
                    }
                }
            )
        ).await;

        // Set the smart glasses alert as false.
        *SMART_GLASSES_ALERT_MUTEX.lock().await = false;
    }
}

#[task]
async fn remote_id_sniffing_task(settings_static_cell: &'static CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>) {
    loop {
        if RemoteIdScanTaskCommand::Start == REMOTE_ID_SCAN_TASK_COMMAND_SIGNAL.wait().await {
            // Steal the Wifi peripheral.
            // It will be freed for re-use when it goes out of scope.
            let wifi_peripheral = unsafe { esp_hal::peripherals::WIFI::steal() };

            // Get the Wifi controller and interfaces.
            let (mut wifi_controller, wifi_interfaces) = esp_radio::wifi::new(
                wifi_peripheral, 
                Default::default()
            )
                .unwrap();

            // Get the Wifi sniffer interface.
            let mut wifi_sniffer = wifi_interfaces.sniffer;

            // Set the Wifi packet received callback.
            wifi_sniffer.set_receive_cb(|packet| {
                let _ = match_frames! {
                    packet.data,
                    beacon = BeaconFrame => {
                        // If the frame has vendor-specific elements..
                        for element in beacon.body.elements.get_matching_elements::<VendorSpecificElement>() {
                            // And the payload prefix matches Remote Id..
                            if element.get_payload_if_prefix_matches(&[0xFA, 0x0B, 0xBC]).is_some() {
                                // Signal the instant a Remote Id packet has been detected.
                                REMOTE_ID_DETECTED_SIGNAL.signal(());
                            }
                        }
                    }
                    action = RawActionFrame => {
                        // If the frame has a vendor payload that mathes Remote Id..
                        if action.body.is_vendor_and_matches([0xFA, 0x0B, 0xBC]) {
                            // Signal the instant a Remote Id packet has been detected.
                            REMOTE_ID_DETECTED_SIGNAL.signal(());
                        }
                    }
                };
            });

            // Set promiscuous mode to recieve packets from unconnected access points.
            let _ = wifi_sniffer.set_promiscuous_mode(true);

            // Set the Remote Id scan state as running.
            *REMOTE_ID_SCAN_TASK_STATE_MUTEX.lock().await = RemoteIdScanTaskState::Running;

            // Select between..
            select(
                // Wifi channel-hopping future.
                async {
                    let mut wifi_channel: u8 = 1;

                    loop {
                        // Set the Wifi channel.
                        let _ = wifi_controller.set_channel(wifi_channel, SecondaryChannel::None);

                        // Increment the channel between 1..14.
                        if wifi_channel == 14 { wifi_channel = 1 } else { wifi_channel += 1 };

                        // Hop channels every 1 second.
                        Timer::after_secs(1).await;
                    }
                },
                // And a select between..
                select(
                    // The scan duration future..
                    async {
                        let scan_duration = settings_static_cell.lock(|settings_mutex| {
                            settings_mutex.borrow().get_remote_id_scan_duration()
                        }) as u64;

                        Timer::after_secs(scan_duration).await;
                    },
                    // And the stop command signal future.
                    async {
                        loop {
                            if RemoteIdScanTaskCommand::Stop == REMOTE_ID_SCAN_TASK_COMMAND_SIGNAL.wait().await {
                                return;
                            }
                        }
                    }
                )
            ).await;

            // Set the Remote Id scan state as stopped.
            *REMOTE_ID_SCAN_TASK_STATE_MUTEX.lock().await = RemoteIdScanTaskState::Stopped;
        }
    }
}

#[task]
async fn remote_id_alert_task(haptic_static_cell: &'static CriticalSectionMutex<RefCell<Drv2605<I2cProxyV0_2>>>) {
    loop {
        // Wait for a Remote Id detection signal.
        REMOTE_ID_DETECTED_SIGNAL.wait().await;

        // Set the Remote Id alert as true.
        *REMOTE_ID_ALERT_MUTEX.lock().await = true;

        // Select between..
        let _ = select(
            // Repeatedly triggering a haptic alert..
            async {
                haptic_static_cell.lock(|haptic| {
                    let mut haptic = haptic.borrow_mut();

                    let _ = haptic.set_single_effect(Effect::LongDoubleSharpClickStrongTwo80);
                    let _ = haptic.set_go(true);
                });

                Timer::after_millis(250).await;
            },
            // And a select between..
            select(
                // Waiting for SCAN_ALERT_DURATION since the Remote Id detection
                async {
                    loop {
                        Timer::after_secs(SCAN_ALERT_DURATION).await;
                    }
                },
                async {
                    // Waiting for the Remote Id scan task state to be Stopped.
                    loop {
                        if *REMOTE_ID_SCAN_TASK_STATE_MUTEX.lock().await == RemoteIdScanTaskState::Stopped {
                            return;
                        }
                    }
                }
            )
        ).await;

        // Set the Remote Id alert as false.
        *REMOTE_ID_ALERT_MUTEX.lock().await = false;
    }
}

// Get localized date and time.
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