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

use bt_hci::param::DisconnectReason::PairingWithUnitKeyNotSupported;
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
use esp_storage::FlashStorage;
use esp_nvs::{
    Nvs,
    Key
};
use esp_radio::{
    ble::controller::BleConnector,
    wifi::{
        WifiController,
        sniffer::Sniffer
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
    task,
    Spawner
};
use embassy_time::{ Timer, Instant };
use embassy_futures::join::{join, join3};
use embassy_futures::select::{select, select3, Either};
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
use cst92xx::{BlockingCST92xx, Point as TouchPoint};

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

static POWER_CELL: StaticCell<CriticalSectionMutex<RefCell<Axp2101<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>>>>> = StaticCell::new();
static FLASH_STORAGE_CELL: StaticCell<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>> = StaticCell::new();
static DISPLAY_CELL: StaticCell<CriticalSectionMutex<RefCell<Co5300Display<'static>>>> = StaticCell::new();
static TOUCH_CELL: StaticCell<CriticalSectionMutex<RefCell<BlockingCST92xx<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>, Delay>>>> = StaticCell::new();
static SETTINGS_CELL: StaticCell<CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>> = StaticCell::new();

static REMOTE_ID_SCAN_TASK_COMMAND: Signal<CriticalSectionRawMutex, RemoteIdScanTaskCommand> = Signal::new();
static REMOTE_ID_SCAN_TASK_STATE: Signal<CriticalSectionRawMutex, RemoteIdScanTaskState> = Signal::new();
static REMOTE_ID_DETECTED: Signal<CriticalSectionRawMutex, Instant> = Signal::new();

static SMART_GLASSES_SCAN_TASK_COMMAND: Signal<CriticalSectionRawMutex, SmartGlassesScanTaskCommand> = Signal::new();
static SMART_GLASSES_SCAN_TASK_STATE: Signal<CriticalSectionRawMutex, SmartGlassesScanTaskState> = Signal::new();
static SMART_GLASSES_DETECTED: Signal<CriticalSectionRawMutex, Instant> = Signal::new();

static DISPLAY_TOUCHED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static DISPLAY_TOUCH_EVENT_UPDATED: Signal<CriticalSectionRawMutex, WindowEvent> = Signal::new();
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
        .expect("i2c initilization failed.")
        .with_sda(peripherals.GPIO3)
        .with_scl(peripherals.GPIO2);

    let i2c_ref = RefCell::new(i2c);
    let i2c_ref_boxed = Box::new(i2c_ref);
    let static_i2c_ref: &'static mut RefCell<I2c<'_, esp_hal::Blocking>> = Box::leak(i2c_ref_boxed);
    let i2c_ref_cell_device = RefCellDevice::new(static_i2c_ref);

    let mut power = Axp2101::new(i2c_ref_cell_device);
    let _ = power.init();
    let _ = power.trim_adc_channels();

    let flash_storage = Nvs::new(0x9000, 0x14000, FlashStorage::new(peripherals.FLASH))
        .expect("Flash storage initilization failed.");
    
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

    let cs = Output::new(peripherals.GPIO41, Level::High, OutputConfig::default());
    let reset = Output::new(peripherals.GPIO37, Level::High, OutputConfig::default());
    let mut display = Co5300Display::new(QspiBus::new(spi, cs), reset);

    display.init();

    // Enable Tearing Effect output on CO5300 (TE pin is GPIO13)
    // Using command 0x35 (TEARON) and param 0x00 (VBlank only)
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
        .expect("Slint platform initilization failed.");

    let main_window = MainWindow::new()
        .expect("Could not create window.");

    let power_cell = POWER_CELL.init(CriticalSectionMutex::new(RefCell::new(power)));
    let flash_storage_cell = FLASH_STORAGE_CELL.init(CriticalSectionMutex::new(RefCell::new(flash_storage)));

    let settings = Settings::new(flash_storage_cell).init();

    display.set_brightness(settings.get_display_brightness());

    let settings_cell = SETTINGS_CELL.init(CriticalSectionMutex::new(RefCell::new(settings)));
    let touch_cell = TOUCH_CELL.init(CriticalSectionMutex::new(RefCell::new(touch)));
    let display_cell = DISPLAY_CELL.init(CriticalSectionMutex::new(RefCell::new(display)));

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

        critical_section::with(|cs| {
            settings_cell.borrow(cs).borrow_mut().set_timestamp(adjusted_datetime.timestamp_micros());
            settings_cell.borrow(cs).borrow_mut().set_timestamp_offset(Instant::now().as_micros());
        });
    });

    main_window.on_get_date(|| {
        let date_time = critical_section::with(|cs| {
            get_date_time(&settings_cell.borrow(cs).borrow())
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
            get_date_time(&settings_cell.borrow(cs).borrow())
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
            settings_cell.borrow(cs).borrow_mut().set_timestamp(adjusted_datetime.timestamp_micros());
            settings_cell.borrow(cs).borrow_mut().set_timestamp_offset(Instant::now().as_micros());
        });
    });

    main_window.on_get_time(|| {
        let date_time = critical_section::with(|cs| {
            get_date_time(&settings_cell.borrow(cs).borrow())
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
            settings_cell.borrow(cs).borrow_mut().set_timezone_offset(offset);
        });
    });

    main_window.on_get_timezone_offset(|| {
        critical_section::with(|cs| {
            settings_cell.borrow(cs).borrow().get_timezone_offset()
        })
    });

    main_window.on_set_screen_brightness(|brightness| {
        critical_section::with(|cs| {
            settings_cell.borrow(cs).borrow_mut().set_display_brightness(brightness as u8);
            display_cell.borrow(cs).borrow_mut().set_brightness(brightness as u8);
        });
    });

    main_window.on_get_screen_brightness(|| {
        critical_section::with(|cs| {
            settings_cell.borrow(cs).borrow().get_display_brightness() as i32
        })
    });

    main_window.on_set_screen_timeout(|timeout| {
        critical_section::with(|cs| {
            settings_cell.borrow(cs).borrow_mut().set_display_timeout(timeout as u8);
        });
    });

    main_window.on_get_screen_timeout(|| {
        critical_section::with(|cs| {
            settings_cell.borrow(cs).borrow().get_display_timeout() as i32
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

    main_window.on_set_remote_id_scan_task_command(|command| {
        REMOTE_ID_SCAN_TASK_COMMAND.signal(command);
    });

    main_window.on_set_smart_glasses_scan_task_command(|command| {
        SMART_GLASSES_SCAN_TASK_COMMAND.signal(command);
    });

    spawner.spawn(battery_status_task(power_cell).unwrap());
    spawner.spawn(touch_event_update_task(touch_cell).unwrap());
    spawner.spawn(date_time_update_task(settings_cell).unwrap());
    spawner.spawn(display_timeout_countdown_task(display_cell, settings_cell).unwrap());
    spawner.spawn(remote_id_sniffing_task().unwrap());
    spawner.spawn(smart_glasses_scan_task().unwrap());

    let mut last_remote_id_detection: Option<Instant> = None;

    main_window.show().unwrap();

    loop {
        slint::platform::update_timers_and_animations();

        if let Some(touch_event) = DISPLAY_TOUCH_EVENT_UPDATED.try_take() {
            software_window.dispatch_event(touch_event);
        }

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

        if let Some(smart_glasses_scan_task_state) = SMART_GLASSES_SCAN_TASK_STATE.try_take() {
            main_window.set_smart_glasses_scan_task_state(smart_glasses_scan_task_state);
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
                battery_status.0 as i32, // Percentage
                battery_status.1 // Is charging
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

        Timer::after_millis(8).await;
    }
}

#[task]
async fn touch_event_update_task(touch_cell: &'static CriticalSectionMutex<RefCell<BlockingCST92xx<RefCellDevice<'static, I2c<'static, esp_hal::Blocking>>, Delay>>>) {
    let mut last_touch_point: Option<TouchPoint> = None;

    loop {
        if let Ok(touches) = critical_section::with(|cs| {
            touch_cell.borrow(cs).borrow_mut().touches()
        }) {
            if let Some(Some(touch_point)) = touches.first() { // We only care about one-finger touches
                DISPLAY_TOUCHED.signal(());

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
        let date_time = critical_section::with(|cs| {
            get_date_time(&settings_cell.borrow(cs).borrow())
        });

        if date_time != last_date_time {
            DATE_TIME_UPDATED.signal(date_time);

            last_date_time = date_time;
        }

        Timer::after_millis(10).await;
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
async fn display_timeout_countdown_task(display_cell: &'static CriticalSectionMutex<RefCell<Co5300Display<'static>>>, settings_cell: &'static CriticalSectionMutex<RefCell<Settings<CriticalSectionMutex<RefCell<Nvs<FlashStorage<'static>>>>>>>) {
    let mut last_touch = Instant::now();
    let mut display_on: bool = true;

    loop {
        if DISPLAY_TOUCHED.try_take().is_some() {
            last_touch = Instant::now();

            if !display_on {
                critical_section::with(|cs| {
                    display_cell.borrow(cs).borrow_mut().display_on();
                });

                display_on = true;
            }
        } else {
            let display_timeout = critical_section::with(|cs| {            
                settings_cell.borrow(cs).borrow().get_display_timeout()
            });

            if Instant::now().duration_since(last_touch).as_secs() > display_timeout as u64 && display_on {
                critical_section::with(|cs| {
                    display_cell.borrow(cs).borrow_mut().display_off();
                });

                display_on = false;
            }
        }

        Timer::after_millis(16).await;
    }
}

struct BleScanHandler {}

impl EventHandler for BleScanHandler {
    fn on_adv_reports(&self, mut it: LeAdvReportsIter<'_>) {
        while let Some(Ok(report)) = it.next() {
            let mut decoder = AdStructure::decode(report.data);

            while let Some(Ok(structure)) = decoder.next() {
                if let AdStructure::ManufacturerSpecificData{ company_identifier, payload: _ } = structure {
                    SMART_GLASSES_DETECTED.signal(Instant::now());

                    // println!("BT device with manufacturer ID: 0x{:04X}", company_identifier);
                }
            }
        }
    }
}

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 4;

#[task]
async fn smart_glasses_scan_task() {
    loop {
        if SmartGlassesScanTaskCommand::Start == SMART_GLASSES_SCAN_TASK_COMMAND.wait().await {
            SMART_GLASSES_SCAN_TASK_STATE.signal(SmartGlassesScanTaskState::Running);

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

                    let ble_scan_handler = BleScanHandler{};

                    let _ = join(
                        runner.run_with_handler(&ble_scan_handler),
                        scanner.scan(&scan_config)
                    )
                        .await;
                },
                async {
                    loop {
                        if Some(SmartGlassesScanTaskCommand::Stop) == SMART_GLASSES_SCAN_TASK_COMMAND.try_take() {
                            return;
                        }

                        Timer::after_millis(16).await;
                    }
                }
            )
                .await;
            
            SMART_GLASSES_SCAN_TASK_STATE.signal(SmartGlassesScanTaskState::Stopped);
        }
    }
}

static REMOTE_ID_PACKET_CHANNEL: Channel<CriticalSectionRawMutex, &[u8], 8> = Channel::new();

#[task]
async fn remote_id_sniffing_task() {
    let mut last_wifi_controller: Option<WifiController<'static>> = None;

    loop {
        match REMOTE_ID_SCAN_TASK_COMMAND.wait().await {
            RemoteIdScanTaskCommand::Start => {
                if last_wifi_controller.is_some() { continue; }

                let wifi_peripheral = unsafe { esp_hal::peripherals::WIFI::steal() };

                let (wifi_controller, wifi_interfaces) = esp_radio::wifi::new(
                    wifi_peripheral, 
                    Default::default()
                )
                    .unwrap();

                last_wifi_controller = Some(wifi_controller);

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

                REMOTE_ID_SCAN_TASK_STATE.signal(RemoteIdScanTaskState::Running);
            }
            RemoteIdScanTaskCommand::Stop => {
                last_wifi_controller = None;

                REMOTE_ID_SCAN_TASK_STATE.signal(RemoteIdScanTaskState::Stopped);
            }
        }
    }
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