#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::{bind_interrupts, peripherals::USB, usb, gpio};
use embassy_time::{Duration, Timer};
use embassy_usb::class::{cdc_acm, midi::MidiClass};
use embassy_usb::driver::EndpointError;
// use embassy_futures
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};
mod midi;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) -> ! {
    defmt::info!("hello");

    let p = embassy_rp::init(Default::default());

    let usb_driver = usb::Driver::new(p.USB, Irqs);

    let mut config = embassy_usb::Config::new(0xdead, 0xbeef);  // vendor_id, product_id
    config.manufacturer = Some("ropc");
    config.product = Some("midi-pedal");
    config.serial_number = Some("0");
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    // config.device_class =

    static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();

    let mut usb_builder = embassy_usb::Builder::new(
        usb_driver,
        config,
        CONFIG_DESCRIPTOR.init([0; 256]),
        BOS_DESCRIPTOR.init([0; 256]),
        &mut [],
        CONTROL_BUF.init([0; 64])
    );

    static STATE: StaticCell<cdc_acm::State> = StaticCell::new();
    let mut cdcAcmClass = cdc_acm::CdcAcmClass::new(&mut usb_builder, STATE.init(cdc_acm::State::new()), 64);
    let mut midiClass = MidiClass::new(&mut usb_builder, 1, 0, 64);
    let mut pin4 = gpio::Input::new(p.PIN_4, gpio::Pull::Up);
    pin4.set_schmitt(true);

    let usb = usb_builder.build();
    spawner.spawn(usb_task(usb));
    spawner.spawn(midi_task(midiClass, pin4));

    loop {
        defmt::debug!("waiting for serial connection");
        cdcAcmClass.wait_connection().await;
        defmt::info!("got serial connection");
        _ = echo(&mut cdcAcmClass).await;
    }
}

#[embassy_executor::task]
async fn midi_task(mut device: MidiClass<'static, MyUsbDriver>, mut button0: gpio::Input<'static>) -> ! {
    // let mut button0 = Debouncer::new(button0, Duration::from_millis(1));
    defmt::debug!("starting midi task");
    loop {
        defmt::debug!("waiting for midi connection");
        device.wait_connection().await;
        defmt::info!("midi connected");
        midi::run_handler(&mut device, &mut button0).await;
    }
}

type MyUsbDriver = usb::Driver<'static, USB>;
pub type MyUsbDevice = embassy_usb::UsbDevice<'static, MyUsbDriver>;

#[embassy_executor::task]
async fn usb_task(mut usb: MyUsbDevice) -> ! {
    usb.run().await
}

struct Disconnected {}

impl From<EndpointError> for Disconnected {
    fn from(val: EndpointError) -> Self {
        match val {
            EndpointError::BufferOverflow => panic!("Buffer overflow"),
            EndpointError::Disabled => Disconnected {},
        }
    }
}

async fn echo<'d, T: usb::Instance + 'd>(class: &mut cdc_acm::CdcAcmClass<'d, usb::Driver<'d, T>>) -> Result<(), Disconnected> {
    let mut buf = [0; 64];
    loop {
        defmt::info!("waiting to read packet");
        let n = class.read_packet(&mut buf).await?;
        let data = &buf[..n];
        defmt::info!("writing packet");
        class.write_packet(data).await?;
    }
}
