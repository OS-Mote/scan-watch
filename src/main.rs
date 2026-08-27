#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

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
        Level,
        Output,
        OutputConfig,
        Input,
        InputConfig
    },
    peripherals::BT,
    spi::{
        Mode as SpiMode,
        master::{
            Config as SpiConfig,
            Spi
        }
    },
    i2c::master::{
        Config as I2cConfig,
        I2c
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::sniffer::Sniffer;
use esp_storage::FlashStorage;
use esp_nvs::{
    Nvs,
    Key
};
use esp_radio::{
    ble::controller::BleConnector,
    wifi::{
        WifiController
    }
};
use embassy_sync::{
    blocking_mutex::CriticalSectionMutex,
    mutex::Mutex,
    blocking_mutex::raw::CriticalSectionRawMutex,
    signal::Signal,
    channel::Channel
};
use embassy_executor::{
    Spawner,
    task
};
use embassy_time::{
    Timer,
    Instant
};
use ieee80211::{
    match_frames,
    mgmt_frame::{
        BeaconFrame,
        RawActionFrame
    },
    elements::VendorSpecificElement
};
use core::cell::RefCell;
use embedded_hal_bus::i2c::RefCellDevice;
use embedded_graphics::{
    pixelcolor::Rgb565, prelude::*
};
use slint::{
    VecModel,
    ModelRc,
    platform::{
        software_renderer::{
            MinimalSoftwareWindow,
            RepaintBufferType
        },
        PointerEventButton,
        WindowEvent
    }
};
use cst92xx::{BlockingCST92xx, Point as TouchPoint};

mod qspi_bus;
mod framebuffer;
mod axp2101;
mod co5300;
mod defaults;

use crate::{
    TouchUpdate::{Moved, Pressed, Released}, axp2101::Axp2101, co5300::*, defaults::*, framebuffer::Framebuffer, qspi_bus::QspiBus
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

static POWER_CELL: StaticCell<CriticalSectionMutex<RefCell<Axp2101<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>>>>> = StaticCell::new();
static STORAGE_CELL: StaticCell<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>> = StaticCell::new();
static DISPLAY_CELL: StaticCell<CriticalSectionMutex<RefCell<Co5300Display<'static>>>> = StaticCell::new();
static TOUCH_CELL: StaticCell<CriticalSectionMutex<RefCell<BlockingCST92xx<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>, Delay>>>> = StaticCell::new();
static WIFI_CONTROLLER_MUTEX: Mutex<CriticalSectionRawMutex, Option<WifiController<'static>>> = Mutex::new(None);
static WIFI_SNIFFER_MUTEX: Mutex<CriticalSectionRawMutex, Option<Sniffer<'static>>> = Mutex::new(None);
static DISPLAY_ON_CELL: StaticCell<CriticalSectionMutex<RefCell<bool>>> = StaticCell::new();

static REMOTE_ID_SCAN_TASK_COMMAND: Signal<CriticalSectionRawMutex, RemoteIdScanTaskCommand> = Signal::new();
static REMOTE_ID_SCAN_TASK_STATE: Signal<CriticalSectionRawMutex, RemoteIdScanTaskState> = Signal::new();
static REMOTE_ID_DETECTED: Signal<CriticalSectionRawMutex, Instant> = Signal::new();
static DISPLAY_TOUCHED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static DISPLAY_TOUCH_UPDATED: Signal<CriticalSectionRawMutex, TouchUpdate> = Signal::new();
static BATTERY_STATUS_UPDATED: Signal<CriticalSectionRawMutex, (u8, bool)> = Signal::new();
static DATE_TIME_UPDATED: Signal<CriticalSectionRawMutex, DateTime<FixedOffset>> = Signal::new();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default();
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 128 * 1024);
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt = esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);

    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    let i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
        .expect("i2c failed")
        .with_sda(peripherals.GPIO3)
        .with_scl(peripherals.GPIO2);

    let i2c_ref = RefCell::new(i2c);
    let i2c_ref_boxed = Box::new(i2c_ref);
    let static_i2c_ref: &'static mut RefCell<I2c<'_, esp_hal::Blocking>> = Box::leak(i2c_ref_boxed);
    let i2c_ref_cell_device = RefCellDevice::new(static_i2c_ref);

    let mut power = Axp2101::new(i2c_ref_cell_device);
    let _ = power.init();
    let _ = power.trim_adc_channels();

    let mut storage = Nvs::new(0x9000, 0x14000, FlashStorage::new(peripherals.FLASH)).unwrap();
    
    let spi_config = SpiConfig::default()
        .with_frequency(Rate::from_mhz(80))
        .with_mode(SpiMode::_0);

    let (rx_buf, rx_desc, tx_buf, tx_desc) = dma_buffers!(8000);
    let dma_rx = DmaRxBuf::new(rx_desc, rx_buf).unwrap();
    let dma_tx = DmaTxBuf::new(tx_desc, tx_buf).unwrap();
    let spi = Spi::new(peripherals.SPI2, spi_config)
        .expect("SPI failed")
        .with_sck(peripherals.GPIO40)
        .with_sio0(peripherals.GPIO38)
        .with_sio1(peripherals.GPIO39)
        .with_sio2(peripherals.GPIO42)
        .with_sio3(peripherals.GPIO45)
        .with_dma(peripherals.DMA_CH0)
        .with_buffers(dma_rx, dma_tx);

    let cs = Output::new(peripherals.GPIO41, Level::High, OutputConfig::default());
    let reset = Output::new(peripherals.GPIO37, Level::High, OutputConfig::default());
    let mut display = Co5300Display::new(QspiBus::new(spi, cs), reset);

    display.init();
    display.set_brightness(get_display_brightness(&mut storage));

    // Enable Tearing Effect output on CO5300 (TE pin = GPIO13)
    // Command 0x35 = TEARON, param 0x00 = VBlank only
    display.bus_mut().write_c8d8(0x35, 0x00);

    let te_pin = Input::new(peripherals.GPIO13, InputConfig::default());
    let mut framebuffer = Framebuffer::new();

    framebuffer.clear_color(Rgb565::BLACK);
    framebuffer.flush(&mut display);
    
    let mut touch = BlockingCST92xx::new(RefCellDevice::new(static_i2c_ref), 0x1A, Delay::new());
    let _ = touch.init();

    let software_window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);

    software_window.set_size(slint::PhysicalSize::new(LCD_WIDTH as u32, LCD_HEIGHT as u32));

    let platform = EmbassySlintPlatform::new(software_window.clone());

    slint::platform::set_platform(alloc::boxed::Box::new(platform))
        .expect("Slint platform initilization failed");

    let main_window = MainWindow::new()
        .expect("Could not create window");

    let power_cell = POWER_CELL.init(CriticalSectionMutex::new(RefCell::new(power)));
    let display_on_cell = DISPLAY_ON_CELL.init(CriticalSectionMutex::new(RefCell::new(true)));
    let storage_cell = STORAGE_CELL.init(CriticalSectionMutex::new(RefCell::new(storage)));
    let touch_cell = TOUCH_CELL.init(CriticalSectionMutex::new(RefCell::new(touch)));
    let display_cell = DISPLAY_CELL.init(CriticalSectionMutex::new(RefCell::new(display)));

    main_window.on_set_date(|month, day, year| {
        let date_time = critical_section::with(|cs| {
            get_date_time(&mut storage_cell.borrow(cs).borrow_mut())
        });

        let adjusted_datetime = date_time
            .with_month(month as u32)
            .unwrap()
            .with_day(day as u32)
            .unwrap()
            .with_year(year)
            .unwrap()
            .to_utc();

        critical_section::with(|cs| {
            set_time(adjusted_datetime.timestamp_micros(), &mut storage_cell.borrow(cs).borrow_mut());
        });
    });

    main_window.on_get_date(|| {
        let date_time = critical_section::with(|cs| {
            get_date_time(&mut storage_cell.borrow(cs).borrow_mut())
        });

        let model: Rc<VecModel<i32>> = Rc::new(VecModel::from(vec![
            date_time.day() as i32,
            date_time.month() as i32,
            date_time.year(),
        ]));

        ModelRc::from(model.clone())
    });

    main_window.on_set_time(|hour, minute, second| {
        let date_time = critical_section::with(|cs| {
            get_date_time(&mut storage_cell.borrow(cs).borrow_mut())
        });

        let adjusted_datetime = date_time
            .with_hour(hour as u32)
            .unwrap()
            .with_minute(minute as u32)
            .unwrap()
            .with_second(second as u32)
            .unwrap()
            .to_utc();

        critical_section::with(|cs| {
            let mut storage = storage_cell.borrow(cs).borrow_mut();

            set_time(adjusted_datetime.timestamp_micros(), &mut storage);
            set_timestamp_set(Instant::now().as_micros(), &mut storage);
        });
    });

    main_window.on_get_time(|| {
        let date_time = critical_section::with(|cs| {
            get_date_time(&mut storage_cell.borrow(cs).borrow_mut())
        });

        let model: Rc<VecModel<i32>> = Rc::new(VecModel::from(vec![
            date_time.hour() as i32,
            date_time.minute() as i32,
            date_time.second() as i32,
        ]));

        ModelRc::from(model.clone())
    });

    main_window.on_set_timezone_offset(|offset| {
        critical_section::with(|cs| {
            set_timezone_offset(offset, &mut storage_cell.borrow(cs).borrow_mut());
        });
    });

    main_window.on_get_timezone_offset(|| {
        critical_section::with(|cs| {
            get_timezone_offset(&mut storage_cell.borrow(cs).borrow_mut())
        })
    });

    main_window.on_set_screen_brightness(|brightness| {
        critical_section::with(|cs| {
            set_display_brightness(brightness as u8, &mut display_cell.borrow(cs).borrow_mut(), &mut storage_cell.borrow(cs).borrow_mut());
        });
    });

    main_window.on_get_screen_brightness(|| {
        critical_section::with(|cs| {
            get_display_brightness(&mut storage_cell.borrow(cs).borrow_mut()) as i32
        })
    });

    main_window.on_set_screen_timeout(|timeout| {
        critical_section::with(|cs| {
            set_display_timeout(timeout as u8, &mut storage_cell.borrow(cs).borrow_mut());
        });
    });

    main_window.on_get_screen_timeout(|| {
        critical_section::with(|cs| {
            get_display_timeout(&mut storage_cell.borrow(cs).borrow_mut()) as i32
        })
    });

    main_window.on_get_days_in_year_month(|year, month| {
        NaiveDate::from_ymd_opt(
            year,
            month as u32,
            1,
        )
            .unwrap()
            .num_days_in_month() as i32
    });

    main_window.on_set_remote_id_scan_task_command(move |command| {
        REMOTE_ID_SCAN_TASK_COMMAND.signal(command);
    });

    spawner.spawn(battery_status_task(power_cell).unwrap());
    spawner.spawn(touch_update_task(touch_cell).unwrap());
    spawner.spawn(date_time_update_task(storage_cell).unwrap());
    spawner.spawn(display_timeout_countdown_task(display_cell, display_on_cell, storage_cell).unwrap());
    spawner.spawn(wifi_sniffing_task().unwrap());

    let mut last_remote_id_detection: Option<Instant> = None;

    main_window.show().unwrap();

    loop {
        Timer::after_millis(8).await;

        if let Some(touch_update) = DISPLAY_TOUCH_UPDATED.try_take() {
            match touch_update {
                Pressed(point) => {
                    software_window.dispatch_event(
                        WindowEvent::PointerPressed {
                            position: slint::LogicalPosition::new(point.x as f32, point.y as f32),
                            button: PointerEventButton::Left
                        }
                    );
                }
                Moved(point) => {
                    software_window.dispatch_event(
                        WindowEvent::PointerMoved {
                            position: slint::LogicalPosition::new(point.x as f32, point.y as f32)
                        }
                    );
                }
                Released(point) => {
                    software_window.dispatch_event(
                        WindowEvent::PointerReleased {
                            position: slint::LogicalPosition::new(point.x as f32, point.y as f32), 
                            button: PointerEventButton::Left 
                        }
                    );
                }
            }
        }

        slint::platform::update_timers_and_animations();

        // Display is off, skip the rest of the loop.

        if critical_section::with(|cs| { !*display_on_cell.borrow(cs).borrow() }) { continue; }
            
        // Display is on, continue drawing the display..

        // Fixme

        if let Some(remote_id_scan_task_state) = REMOTE_ID_SCAN_TASK_STATE.try_take() {
            main_window.set_remote_id_scan_task_state(remote_id_scan_task_state);
        }

        if let Some(remote_id_detected) = REMOTE_ID_DETECTED.try_take() {
            last_remote_id_detection = Some(remote_id_detected);

            main_window.set_remote_id_detected(true);
        } else {
            if let Some(detection) = last_remote_id_detection && detection.elapsed().as_secs() > 30 {
                last_remote_id_detection = None;

                main_window.set_remote_id_detected(false);
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
                battery_status.0 as i32,
                battery_status.1
            );
        }

        if software_window.draw_if_needed(|renderer| {
            renderer.render(framebuffer.as_rgb565_pixels_mut(), LCD_WIDTH as usize);
        }) {
            critical_section::with(|cs| {
                let mut display = display_cell.borrow(cs).borrow_mut();
        
                framebuffer.flush_vsync(&mut display, &te_pin);
            });
        }
    }
}

fn set_display_brightness(brightness: u8, display: &mut Co5300Display<'static>, storage: &mut Nvs<FlashStorage<'static>>) {
    display.set_brightness(brightness);

    let _ = storage.set(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(DISPLAY_BRIGHTNESS_DEFAULT_KEY), brightness);
}

fn get_display_brightness(storage: &mut Nvs<FlashStorage<'static>>) -> u8 {
    storage.get(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(DISPLAY_BRIGHTNESS_DEFAULT_KEY))
        .unwrap_or(DISPLAY_BRIGHTNESS_DEFAULT)
}

fn set_display_timeout(timeout: u8, storage: &mut Nvs<FlashStorage<'static>>) {
    let _ = storage.set(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(DISPLAY_TIMEOUT_DEFAULT_KEY), timeout);
}

fn get_display_timeout(storage: &mut Nvs<FlashStorage<'static>>) -> u8 {
    storage.get(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(DISPLAY_TIMEOUT_DEFAULT_KEY))
        .unwrap_or(DISPLAY_TIMEOUT_DEFAULT)
}

fn set_timezone_offset(offset: i32, storage: &mut Nvs<FlashStorage<'static>>) {
    let _ = storage.set(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(TIMEZONE_OFFSET_DEFAULT_KEY), offset);
}

fn get_timezone_offset(storage: &mut Nvs<FlashStorage<'static>>) -> i32 {
    storage.get(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(TIMEZONE_OFFSET_DEFAULT_KEY))
        .unwrap_or(TIMEZONE_OFFSET_DEFAULT)
}

fn set_time(timestamp: i64, storage: &mut Nvs<FlashStorage<'static>>) {
    let _ = storage.set(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(TIMESTAMP_DEFAULT_KEY), timestamp);
}

fn get_timestamp(storage: &mut Nvs<FlashStorage<'static>>) -> i64 {
    storage.get(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(TIMESTAMP_DEFAULT_KEY))
        .unwrap_or(TIMESTAMP_DEFAULT)
}

fn set_timestamp_set(timestamp_set: u64, storage: &mut Nvs<FlashStorage<'static>>) {
    let _ = storage.set(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(TIMESTAMP_SET_DEFAULT_KEY), timestamp_set);
}

fn get_timestamp_set(storage: &mut Nvs<FlashStorage<'static>>) -> u64 {
    storage.get(&Key::from_str(DEFAULTS_NAMESPACE_KEY), &Key::from_str(TIMESTAMP_SET_DEFAULT_KEY))
        .unwrap_or(TIMESTAMP_SET_DEFAULT)
}

fn get_date_time(storage: &mut Nvs<FlashStorage<'static>>) -> DateTime<FixedOffset> {
    let timestamp = get_timestamp(storage);
    let timestamp_set = get_timestamp_set(storage);
    let timezone_offset = get_timezone_offset(storage);
    let now_ticks = Instant::now().as_micros();
    let elapsed_micros = now_ticks.saturating_sub(timestamp_set);

    DateTime::from_timestamp_micros(timestamp + (elapsed_micros as i64))
        .unwrap()
        .with_timezone(&FixedOffset::east_opt(3600 * timezone_offset).unwrap())
}

enum TouchUpdate {
    Pressed(TouchPoint),
    Moved(TouchPoint),
    Released(TouchPoint)
}

#[task]
async fn touch_update_task(touch_cell: &'static CriticalSectionMutex<RefCell<BlockingCST92xx<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>, Delay>>>) {
    let mut last_touch_point: Option<TouchPoint> = None;

    loop {
        let touches = critical_section::with(|cs| {
            let mut touch = touch_cell.borrow(cs).borrow_mut();

            touch.touches()
        });

        if let Ok(touches) = touches {
            if let Some(Some(touch_point)) = touches.first() { // We only care about one-finger touches
                DISPLAY_TOUCHED.signal(());

                if last_touch_point.is_some() {
                    DISPLAY_TOUCH_UPDATED.signal(Moved(*touch_point));
                } else {
                    DISPLAY_TOUCH_UPDATED.signal(Pressed(*touch_point));
                }

                last_touch_point = Some(*touch_point);

            } else if let Some(touch_point) = last_touch_point {
                DISPLAY_TOUCH_UPDATED.signal(Released(touch_point));

                last_touch_point = None;
            }
        }

        Timer::after_millis(16).await;
    }
}

#[task]
async fn date_time_update_task(storage_cell: &'static CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>) {
    let mut last_date_time = critical_section::with(|cs| {
        get_date_time(&mut storage_cell.borrow(cs).borrow_mut())
    });    

    loop {
        let date_time = critical_section::with(|cs| {
            get_date_time(&mut storage_cell.borrow(cs).borrow_mut())
        });

        if date_time != last_date_time {
            DATE_TIME_UPDATED.signal(date_time);

            last_date_time = date_time;
        }

        Timer::after_millis(500).await;
    }
}

#[task]
async fn battery_status_task(power_cell: &'static CriticalSectionMutex<RefCell<Axp2101<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>>>>) {
    let mut last_battery_status: (u8, bool) = (0, false);

    loop {
        let battery_status = critical_section::with(|cs| {
            let mut power = power_cell.borrow(cs).borrow_mut();

            (
                power.get_battery_percent().unwrap_or(0),
                power.is_vbus_in().unwrap_or(false) && power.get_battery_voltage().unwrap_or(0) < 4150
            )
        });

        if battery_status != last_battery_status {
            BATTERY_STATUS_UPDATED.signal(battery_status);

            last_battery_status = battery_status;
        }

        Timer::after_secs(10).await
    }
}

#[task]
async fn display_timeout_countdown_task(display_cell: &'static CriticalSectionMutex<RefCell<Co5300Display<'static>>>, display_on_cell: &'static CriticalSectionMutex<RefCell<bool>>, storage_cell: &'static CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>) {
    loop {
        let display_timeout = critical_section::with(|cs| {            
            display_cell.borrow(cs).borrow_mut().display_on();
            display_on_cell.borrow(cs).replace(true);

            get_display_timeout(&mut storage_cell.borrow(cs).borrow_mut()) as u64
        });

        Timer::after_secs(display_timeout).await;
        
        if !DISPLAY_TOUCHED.signaled() {
            critical_section::with(|cs| {
                display_cell.borrow(cs).borrow_mut().display_off();
                display_on_cell.borrow(cs).replace(false);
            });
        }

        DISPLAY_TOUCHED.wait().await;
    }
}

// const CONNECTIONS_MAX: usize = 1;
// const L2CAP_CHANNELS_MAX: usize = 4;

// struct BleScanHandler {}

// impl EventHandler for BleScanHandler {
//     fn on_adv_reports(&self, mut it: LeAdvReportsIter<'_>) {
//         println!("Got BT packet");
//         while let Some(Ok(report)) = it.next() {
//             let mut decoder = AdStructure::decode(report.data);

//             while let Some(Ok(structure)) = decoder.next() {
//                 if let AdStructure::ManufacturerSpecificData{ company_identifier, payload } = structure {
//                     println!("BT device with manufacturer ID: 0x{:04X}", company_identifier);
//                 }
//             }
//         }
//     }
// }

// #[allow(
//     clippy::large_stack_frames,
//     reason = "BLE stack is very large."
// )]
// #[task]
// async fn ble_scan_task(bt: BT<'static>) {
//     let connector = BleConnector::new(bt, Default::default()).unwrap();
//     let controller: ExternalController<_, 1> = ExternalController::new(connector);
//     let address = trouble_host::Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xff]);
//     let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
//     let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
//     let Host {
//         central,
//         mut runner,
//         ..
//     } = stack.build();
//     let printer = BleScanHandler {};
//     let mut scanner = Scanner::new(central);

//     let _ = join(runner.run_with_handler(&printer), async {
//         scanner.scan(&ScanConfig::default()).await.unwrap();
//     })
//     .await;
// }



// fn is_remote_id_packet<'a>(packet: &PromiscuousPkt<'a>) -> bool {
//     let _ = match_frames! {
//         packet.data,
//         beacon = BeaconFrame => {
//             for element in beacon.body.elements.get_matching_elements::<VendorSpecificElement>() {
//                 if element.get_payload_if_prefix_matches(&[0xFA, 0x0B, 0xBC]).is_some() {
//                     return true;
//                     //REMOTE_ID_DETECTED.signal(Instant::now());
//                 }
//             }
//         }
//         action = RawActionFrame => {
//             if action.body.is_vendor_and_matches([0xFA, 0x0B, 0xBC]) {
//                 return true;
//                 // REMOTE_ID_DETECTED.signal(Instant::now());
//             }
//         }
//     };

//     return false
// }

static REMOTE_ID_PACKET_CHANNEL: Channel<CriticalSectionRawMutex, &[u8], 8> = Channel::new();

#[task]
async fn wifi_sniffing_task() {
    loop {
        match REMOTE_ID_SCAN_TASK_COMMAND.wait().await {
            RemoteIdScanTaskCommand::Start => {
                if WIFI_CONTROLLER_MUTEX.lock().await.is_some() || WIFI_SNIFFER_MUTEX.lock().await.is_some() {
                    continue;
                }

                let wifi_peripheral = unsafe { esp_hal::peripherals::WIFI::steal() };

                let (wifi_controller, wifi_interfaces) = esp_radio::wifi::new(
                    wifi_peripheral, 
                    Default::default()
                ).unwrap();

                let mut wifi_controller_guard = WIFI_CONTROLLER_MUTEX.lock().await;
                
                *wifi_controller_guard = Some(wifi_controller);

                let mut wifi_sniffer_guard = WIFI_SNIFFER_MUTEX.lock().await;

                *wifi_sniffer_guard = Some(wifi_interfaces.sniffer);

                if let Some(wifi_sniffer) = wifi_sniffer_guard.as_mut() {
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

                    REMOTE_ID_SCAN_TASK_STATE.signal(RemoteIdScanTaskState::Running);
                }
            }
            RemoteIdScanTaskCommand::Stop => {
                let mut wifi_sniffer_guard = WIFI_SNIFFER_MUTEX.lock().await;

                *wifi_sniffer_guard = None;

                drop(wifi_sniffer_guard);

                let mut wifi_controller_guard = WIFI_CONTROLLER_MUTEX.lock().await;

                *wifi_controller_guard = None;

                drop(wifi_controller_guard);

                REMOTE_ID_SCAN_TASK_STATE.signal(RemoteIdScanTaskState::Stopped);
            }
        }
    }
}
