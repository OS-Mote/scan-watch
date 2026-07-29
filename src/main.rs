#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use bt_hci::controller::ExternalController;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_radio::ble::controller::BleConnector;
use trouble_host::prelude::*;
use esp_hal::peripherals::WIFI;
use esp_hal::peripherals::BT;
use esp_hal::gpio::{Level, Output, OutputConfig, AnyPin};
use esp_hal::delay::Delay;
use embedded_hal::delay::DelayNs;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode as SpiMode;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use ieee80211::{match_frames, mgmt_frame::BeaconFrame};
use embassy_executor::task;
use embassy_futures::join::join;
use embedded_hal_bus::spi::ExclusiveDevice;
use mipidsi::interface::SpiInterface;
use alloc::rc::Rc;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::software_renderer::{LineBufferProvider, Rgb565Pixel};

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

pub struct ChipOneCO5300;

impl mipidsi::models::Model for ChipOneCO5300 {
    // Satisfy native mipidsi 0.10.0 requirements directly with standard Rgb565
    type ColorFormat = embedded_graphics::pixelcolor::Rgb565;
    const FRAMEBUFFER_SIZE: (u16, u16) = (410, 502);

    fn init<DELAY, DI>(
        &mut self,
        _di: &mut DI,
        _delay: &mut DELAY,
        _options: &mipidsi::options::ModelOptions,
    ) -> Result<mipidsi::dcs::SetAddressMode, mipidsi::models::ModelInitError<DI::Error>>
    where
        DELAY: embedded_hal::delay::DelayNs,
        DI: mipidsi::interface::Interface,
    {
        let madctl = mipidsi::dcs::SetAddressMode::default()
            .with_color_order(mipidsi::options::ColorOrder::Rgb);
        Ok(madctl)
    }
}

struct DisplayWrapper<'a, DI, M, RST>
where
    DI: mipidsi::interface::Interface<Word = u8>,
    M: mipidsi::models::Model<ColorFormat = embedded_graphics::pixelcolor::Rgb565>,
    RST: embedded_hal::digital::OutputPin,
{
    display: &'a mut mipidsi::Display<DI, M, RST>,
    line_buffer: &'a mut [Rgb565Pixel; 410],
}

impl<'a, DI, M, RST> LineBufferProvider for DisplayWrapper<'a, DI, M, RST>
where
    DI: mipidsi::interface::Interface<Word = u8>,
    M: mipidsi::models::Model<ColorFormat = embedded_graphics::pixelcolor::Rgb565>,
    RST: embedded_hal::digital::OutputPin,
{
    type TargetPixel = Rgb565Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        render_fn(&mut self.line_buffer[range.clone()]);

        let sx = range.start as u16;
        let sy = line as u16;
        let ex = (range.end - 1) as u16;
        let ey = line as u16;

        self.display
            .set_pixels(
                sx,
                sy,
                ex,
                ey,
                self.line_buffer[range].iter().map(|p| {
                    embedded_graphics::pixelcolor::raw::RawU16::new(p.0).into()
                }),
            )
            .unwrap();
    }
}

pub struct EmbassySlintPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl EmbassySlintPlatform {
    pub fn new(window: Rc<MinimalSoftwareWindow>) -> Self {
        Self { window }
    }
}

impl slint::platform::Platform for EmbassySlintPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn slint::platform::WindowAdapter + 'static>, slint::PlatformError> {
        Ok(self.window.clone() as Rc<dyn slint::platform::WindowAdapter>)
    }

    fn duration_since_start(&self) -> core::time::Duration {
        core::time::Duration::from_millis(embassy_time::Instant::now().as_millis())
    }
}

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 1;

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    esp_alloc::heap_allocator!(size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt = esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    spawner.spawn(wifi_sniffing_task(peripherals.WIFI).unwrap());
    spawner.spawn(ble_scan_task(peripherals.BT).unwrap());
    spawner.spawn(slint_ui_task(
        peripherals.SPI2,
        peripherals.GPIO4.into(),
        peripherals.GPIO6.into(),
        peripherals.GPIO7.into(),
        peripherals.GPIO2.into(),
        peripherals.GPIO1.into(),
    ).unwrap());

    loop {
        Timer::after(Duration::from_secs(1)).await;
    }
}

struct BleScanHandler {}

impl EventHandler for BleScanHandler {
    fn on_adv_reports(&self, mut it: LeAdvReportsIter<'_>) {
        while let Some(Ok(report)) = it.next() {
            let mut decoder = AdStructure::decode(report.data);

            while let Some(Ok(structure)) = decoder.next() {
                if let AdStructure::ManufacturerSpecificData{ company_identifier, payload } = structure {
                    println!("BT device with manufacturer ID: 0x{:04X}", company_identifier);
                }
            }
        }
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "BLE stack is very large."
)]
#[task]
async fn ble_scan_task(bt: BT<'static>) {
    let connector = BleConnector::new(bt, Default::default()).unwrap();
    let controller: ExternalController<_, 1> = ExternalController::new(connector);
    let address: Address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xff]);
    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
    let Host {
        central,
        mut runner,
        ..
    } = stack.build();

    let printer = BleScanHandler {};
    let mut scanner = Scanner::new(central);

    let _ = join(runner.run_with_handler(&printer), async {
        let config = ScanConfig::default();
        let mut _session = scanner.scan(&config).await.unwrap();
        // Scan forever
        loop {
            Timer::after(Duration::from_secs(1)).await;
        }
    })
    .await;
}

#[task]
async fn wifi_sniffing_task(wifi: WIFI<'static>) {
    let (_controller, interfaces) = esp_radio::wifi::new(wifi, Default::default()).unwrap();

    let mut sniffer = interfaces.sniffer;
    sniffer.set_promiscuous_mode(true).unwrap();

    sniffer.set_receive_cb(|packet| {
        let _ = match_frames! {
            packet.data,
            beacon = BeaconFrame => {
                let Some(ssid) = beacon.ssid() else {
                    return;
                };

                println!("Wifi AP with SSID: {ssid}");

                // if critical_section::with(|cs| {
                //     KNOWN_SSIDS.borrow_ref_mut(cs).insert(ssid.to_string())
                // }) {
                //     println!("Found new AP with SSID: {ssid}");
                // }
            }
        };
    });

    loop {
        Timer::after(Duration::from_secs(1)).await;
    }
}

slint::slint! {
    export component MainWindow inherits Window {
        width: 410px;
        height: 502px;
        background: #000000;
        Text {
            text: "3-Arg Interface Fixed";
            color: #ffaa00;
            font-size: 24px;
            horizontal-alignment: center;
            vertical-alignment: center;
        }
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "Slint stack is very large."
)]
#[task]
async fn slint_ui_task(
    spi_periph: esp_hal::peripherals::SPI2<'static>,
    sclk: AnyPin<'static>,
    mosi: AnyPin<'static>,
    cs: AnyPin<'static>,
    dc: AnyPin<'static>,
    rst: AnyPin<'static>,
) {
    let spi_bus = Spi::new(
        spi_periph,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(40))
            .with_mode(SpiMode::_0),
    )
        .unwrap()
        .with_sck(sclk)
        .with_mosi(mosi);

    let cs_output = Output::new(cs, Level::High, OutputConfig::default());
    let spi_device = ExclusiveDevice::new_no_delay(spi_bus, cs_output).unwrap();
    let mut spi_scratch_buffer = [0_u8; 512];
    let dc_output = Output::new(dc, Level::Low, OutputConfig::default());
    let di = SpiInterface::new(spi_device, dc_output, &mut spi_scratch_buffer);
    let mut rst_output = Output::new(rst, Level::Low, OutputConfig::default());
    let mut delay_provider = Delay::new();

    rst_output.set_low();
    delay_provider.delay_ms(20u32);
    rst_output.set_high();
    delay_provider.delay_ms(120u32);

    let mut display = mipidsi::Builder::new(ChipOneCO5300, di)
        .display_size(410, 502)
        .init(&mut delay_provider)
        .unwrap();

    let software_window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    let platform = EmbassySlintPlatform::new(software_window.clone());

    slint::platform::set_platform(alloc::boxed::Box::new(platform)).unwrap();

    let main_window = MainWindow::new().unwrap();

    main_window.show().unwrap();

    let mut line_buffer = [Rgb565Pixel(0); 410];

    loop {
        slint::platform::update_timers_and_animations();

        software_window.draw_if_needed(|renderer| {
            renderer.render_by_line(DisplayWrapper {
                display: &mut display,
                line_buffer: &mut line_buffer,
            });
        });

        Timer::after_millis(16).await; 
    }
}