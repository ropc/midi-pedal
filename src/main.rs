#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::{bind_interrupts, peripherals::USB, usb};
use embassy_usb::class::cdc_acm;
use embassy_usb::driver::EndpointError;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) -> ! {
    defmt::info!("hello");

    let p = embassy_rp::init(Default::default());

    let usb_driver = usb::Driver::new(p.USB, Irqs);

    let mut config = embassy_usb::Config::new(0xdead, 0xbeef);  // vendor_id, product_id
    config.manufacturer = Some("noone");
    config.product = Some("midi-pedal");
    config.serial_number = Some("01234");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

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
    let mut class = cdc_acm::CdcAcmClass::new(&mut usb_builder, STATE.init(cdc_acm::State::new()), 64);

    let usb = usb_builder.build();
    spawner.spawn(usb_task(usb));

    loop {
        defmt::info!("waiting for connection {}", class.line_coding());
        class.wait_connection().await;
        defmt::info!("got connection");
        _ = echo(&mut class).await;
    }
}

type MyUsbDriver = usb::Driver<'static, USB>;
type MyUsbDevice = embassy_usb::UsbDevice<'static, MyUsbDriver>;

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
