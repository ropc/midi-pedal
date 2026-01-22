#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::{bind_interrupts, peripherals::USB, usb, gpio};
use embassy_time::{Duration, Timer};
use embassy_usb::class::{cdc_acm, midi};
use embassy_usb::driver::EndpointError;
// use embassy_futures
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
    let mut midiClass = midi::MidiClass::new(&mut usb_builder, 1, 0, 64);
    let mut pin4 = gpio::Input::new(p.PIN_4, gpio::Pull::Up);
    pin4.set_schmitt(true);
    // Debouncer::new(pin4, Duration::from_millis(1));
    // pin4.set_inversion(true);

    let usb = usb_builder.build();
    spawner.spawn(usb_task(usb));
    spawner.spawn(midi_task(midiClass, pin4));

    loop {
        defmt::info!("waiting for serial connection");
        cdcAcmClass.wait_connection().await;
        defmt::info!("got connection");
        _ = echo(&mut cdcAcmClass).await;
    }
}

#[embassy_executor::task]
async fn midi_task(mut class: midi::MidiClass<'static, MyUsbDriver>, mut button0: gpio::Input<'static>) -> ! {
    // let mut button0 = Debouncer::new(button0, Duration::from_millis(1));
    defmt::info!("starting midi task");
    loop {
        defmt::info!("waiting for midi connection");
        class.wait_connection().await;
        defmt::info!("got midi connection!");
        let mut is_momentary = false;
        let mut value = 0xff;  // midi value to send, first press will be 127 (ON)

        loop {
            // wait for transition 
            button0.wait_for_low().await;

            // contruct usb-midi packet
            let header = 0x0b;  // usb-midi header: 0x0_ == cable number, 0x_b == CC (tells receiver how many bytes to expect)
            let status = 0xb0; // midi status: 0xb_ == Control Change msg, 0x_0 == channel 0
            let control_number = 20;
            let packet = [header, status, control_number, value];
            // send packet
            let result = class.write_packet(&packet).await;

            defmt::debug!("sent packet {:?}: {}", packet, result);

            Timer::after_millis(20).await;
            button0.wait_for_high().await;

            // send depress signal
            if (is_momentary) {
                let packet = [0x0b, 0xb0, 20, 0x00];
                class.write_packet(&packet).await;
                defmt::debug!("sent packet {:?}: {}", packet, result);
            } else {
                // not a momentary switch: toggle value
                value = value ^ 0xff;
            }
        }
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
