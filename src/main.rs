#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::gpio::{Input, Pull};
use embassy_rp::{bind_interrupts, peripherals::USB, usb};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, Sender};
use embassy_time::{Duration, Timer, with_timeout};
use embassy_usb::class::midi::MidiClass;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

static CHANNEL: Channel<CriticalSectionRawMutex, ButtonMessage, 16> = Channel::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) -> ! {
    defmt::info!("hello");

    let p = embassy_rp::init(Default::default());

    // usb setup

    let usb_driver = usb::Driver::new(p.USB, Irqs);

    let config = {
        let mut config = embassy_usb::Config::new(0xdead, 0xbeef); // vendor_id, product_id
        config.manufacturer = Some("ropc");
        config.product = Some("midi-pedal");
        config.serial_number = Some("0");
        config.max_power = 100;
        config.max_packet_size_0 = 64;
        config
    };

    static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();

    let mut usb_builder = embassy_usb::Builder::new(
        usb_driver,
        config,
        CONFIG_DESCRIPTOR.init([0; 256]),
        BOS_DESCRIPTOR.init([0; 256]),
        &mut [],
        CONTROL_BUF.init([0; 64]),
    );

    let mut midi_device = MidiClass::new(&mut usb_builder, 1, 0, 64);

    let usb = usb_builder.build();
    spawner.spawn(usb_task(usb)).unwrap();

    // button setup

    let pin2 = Input::new(p.PIN_2, Pull::Up);
    let pin3 = Input::new(p.PIN_3, Pull::Up);
    let pin4 = Input::new(p.PIN_4, Pull::Up);
    let pin5 = Input::new(p.PIN_5, Pull::Up);
    let pin6 = Input::new(p.PIN_6, Pull::Up);
    let pin7 = Input::new(p.PIN_7, Pull::Up);

    let sender = CHANNEL.sender();
    spawner.spawn(button_task(0, false, pin2, sender)).unwrap();
    spawner.spawn(button_task(1, false, pin3, sender)).unwrap();
    spawner.spawn(button_task(2, false, pin4, sender)).unwrap();
    spawner.spawn(button_task(3, false, pin5, sender)).unwrap();
    spawner.spawn(button_task(4, false, pin6, sender)).unwrap();
    spawner.spawn(button_task(5, false, pin7, sender)).unwrap();

    // midi cc output loop

    defmt::debug!("starting midi controller");

    loop {
        defmt::debug!("waiting for messages");
        let button_message = CHANNEL.receive().await;

        let control_number = button_message.button_id + 20; // use MIDI CC range 20-26
        let value = match button_message.state {
            ButtonState::Pressed => 0xff,
            ButtonState::Released => 0x00,
        };
        defmt::debug!(
            "got message: button_id: {}, state: {}",
            button_message.button_id,
            value
        );

        let packet = midi_packet(control_number, value);
        // if midi device isn't connected, this will hang. instead timeout in 10ms,
        // essentially dropping the packet when disconnected
        match with_timeout(Duration::from_millis(10), midi_device.write_packet(&packet)).await {
            Ok(Ok(_)) => defmt::debug!("sent packet {:?}", packet),
            Ok(Err(err)) => defmt::warn!("error sending packet {:?}: {:?}", packet, err),
            Err(_) => defmt::debug!("hit timeout, dropping packet, {:?}", packet),
        };
        // defmt::debug!("sent packet {:?}: {}", packet, result);
    }
}

type MyUsbDriver = usb::Driver<'static, USB>;
pub type MyUsbDevice = embassy_usb::UsbDevice<'static, MyUsbDriver>;

#[embassy_executor::task]
async fn usb_task(mut usb: MyUsbDevice) -> ! {
    usb.run().await
}

struct ButtonMessage {
    button_id: u8,
    state: ButtonState,
}

#[derive(Debug, Clone, Copy)]
enum ButtonState {
    Pressed,
    Released,
}

impl ButtonState {
    fn toggle(self) -> Self {
        match self {
            ButtonState::Pressed => ButtonState::Released,
            ButtonState::Released => ButtonState::Pressed,
        }
    }
}

#[embassy_executor::task(pool_size = 6)]
async fn button_task(
    id: u8,
    is_momentary: bool,
    mut button: Input<'static>,
    sender: Sender<'static, CriticalSectionRawMutex, ButtonMessage, 16>,
) {
    let mut state: ButtonState = ButtonState::Pressed;

    loop {
        defmt::debug!("button {} waiting for low", id);
        // wait for transition
        button.wait_for_low().await;

        defmt::debug!(
            "will signal from button{}, channel has {} elements",
            id,
            sender.len()
        );

        sender
            .send(ButtonMessage {
                button_id: id,
                state,
            })
            .await;

        Timer::after_millis(20).await;
        button.wait_for_high().await;

        defmt::debug!("button {} got hi", id);

        if is_momentary {
            // send depress signal
            sender
                .send(ButtonMessage {
                    button_id: id,
                    state: ButtonState::Released,
                })
                .await;
        } else {
            // not a momentary switch: toggle value
            state = state.toggle();
        }
    }
}

/// constructs a USB-MIDI CC packet on channel 0
fn midi_packet(control_number: u8, value: u8) -> [u8; 4] {
    [
        0x0b, // usb-midi header: 0x0_ == cable number, 0x_b == CC (tells receiver how many bytes to expect)
        0xb0, // midi status: 0xb_ == Control Change msg, 0x_0 == channel 0
        control_number,
        value,
    ]
}
