#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::gpio::{Input, Pull};
use embassy_rp::{bind_interrupts, peripherals::USB, usb};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, Sender};
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer, with_timeout};
use embassy_usb::class::midi::MidiClass;
use embassy_futures::select::{select, Either};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

static CHANNEL: Channel<CriticalSectionRawMutex, ButtonMessage, 16> = Channel::new();
static SIGNAL_BUTTON_0: Signal<CriticalSectionRawMutex, ButtonConfig> = Signal::new();
static SIGNAL_BUTTON_1: Signal<CriticalSectionRawMutex, ButtonConfig> = Signal::new();
static SIGNAL_BUTTON_2: Signal<CriticalSectionRawMutex, ButtonConfig> = Signal::new();
static SIGNAL_BUTTON_3: Signal<CriticalSectionRawMutex, ButtonConfig> = Signal::new();
static SIGNAL_BUTTON_4: Signal<CriticalSectionRawMutex, ButtonConfig> = Signal::new();
static SIGNAL_BUTTON_5: Signal<CriticalSectionRawMutex, ButtonConfig> = Signal::new();

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

    let mut midi_device = MidiClass::new(&mut usb_builder, 1, 1, 64);

    let usb = usb_builder.build();
    spawner.spawn(usb_task(usb)).unwrap();

    // button setup

    let button0_pin = Input::new(p.PIN_1, Pull::Up);
    let button1_pin = Input::new(p.PIN_22, Pull::Up);
    let button2_pin = Input::new(p.PIN_18, Pull::Up);
    let button3_pin = Input::new(p.PIN_5, Pull::Up);
    let button4_pin = Input::new(p.PIN_9, Pull::Up);
    let button5_pin = Input::new(p.PIN_14, Pull::Up);

    let sender = CHANNEL.sender();
    spawner.spawn(button_task(0, &SIGNAL_BUTTON_0, button0_pin, sender)).unwrap();
    spawner.spawn(button_task(1, &SIGNAL_BUTTON_1, button1_pin, sender)).unwrap();
    spawner.spawn(button_task(2, &SIGNAL_BUTTON_2, button2_pin, sender)).unwrap();
    spawner.spawn(button_task(3, &SIGNAL_BUTTON_3, button3_pin, sender)).unwrap();
    spawner.spawn(button_task(4, &SIGNAL_BUTTON_4, button4_pin, sender)).unwrap();
    spawner.spawn(button_task(5, &SIGNAL_BUTTON_5, button5_pin, sender)).unwrap();

    // midi cc output loop

    defmt::debug!("starting midi controller");

    loop {
        defmt::debug!("waiting for messages");
        let mut buf = [0; 64];
        match select(CHANNEL.receive(), midi_device.read_packet(&mut buf)).await {
            Either::First(button_message) => {
                let control_number = button_message.button_id + 20; // use MIDI CC range 20-26
                let value = match button_message.state {
                    ButtonState::On => 127,
                    ButtonState::Off => 0,
                };
                defmt::debug!(
                    "got message: button_id: {}, state: {}",
                    button_message.button_id,
                    value
                );

                let packet = midi_packet(control_number, value);
                // if midi device isn't connected, write_packet() will hang. instead timeout in 10ms,
                // essentially dropping the packet when disconnected
                match with_timeout(Duration::from_millis(10), midi_device.write_packet(&packet)).await {
                    Ok(Ok(_)) => defmt::debug!("sent packet {:?}", packet),
                    Ok(Err(err)) => defmt::warn!("error sending packet {:?}: {:?}", packet, err),
                    Err(_) => defmt::debug!("hit timeout, dropping packet, {:?}", packet),
                };
            },
            Either::Second(Ok(midi_message_size)) => handle_midi_message(&buf, midi_message_size),
            Either::Second(Err(err)) => defmt::warn!("midi error: {}", err),
        };
    }
}

fn handle_midi_message(message: &[u8], size: usize) {
    if size != 4 {
        return;  // wrong size for CC message
    }
    if message[..2] != [0x0b, 0xb0] {
        return;  // wrong headers/channel for CC message
    }

    // received CC message, only controllers 20-26 are valid
    let controller = message[2];
    let Some(signal) = (match controller - 20 {
        0 => Some(&SIGNAL_BUTTON_0),
        1 => Some(&SIGNAL_BUTTON_1),
        2 => Some(&SIGNAL_BUTTON_2),
        3 => Some(&SIGNAL_BUTTON_3),
        4 => Some(&SIGNAL_BUTTON_4),
        5 => Some(&SIGNAL_BUTTON_5),
        _ => None,
    }) else {
        return;  // received message for unsupported controller
    };

    // set button behavior according to value
    let value = message[3];
    let behavior = ButtonBehavior::from(value);
    signal.signal(ButtonConfig { behavior: behavior });
}

type MyUsbDriver = usb::Driver<'static, USB>;
type MyUsbDevice = embassy_usb::UsbDevice<'static, MyUsbDriver>;

#[embassy_executor::task]
async fn usb_task(mut usb: MyUsbDevice) -> ! {
    usb.run().await
}

struct ButtonMessage {
    button_id: u8,
    state: ButtonState,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ButtonBehavior {
    #[default]
    Toggle,
    Momentary,
    Tap,
}

impl From<u8> for ButtonBehavior {
    fn from(value: u8) -> Self {
        match value {
            0x00 => ButtonBehavior::Toggle,
            0x01 => ButtonBehavior::Momentary,
            0x02 => ButtonBehavior::Tap,
            _ => ButtonBehavior::Toggle, // default to toggle
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ButtonState {
    On,
    Off,
}

impl ButtonState {
    fn toggle(self) -> Self {
        match self {
            ButtonState::On => ButtonState::Off,
            ButtonState::Off => ButtonState::On,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ButtonConfig {
    behavior: ButtonBehavior,
}

#[embassy_executor::task(pool_size = 6)]
async fn button_task(
    id: u8,
    config_source: &'static Signal<CriticalSectionRawMutex, ButtonConfig>,
    mut button: Input<'static>,
    sender: Sender<'static, CriticalSectionRawMutex, ButtonMessage, 16>,
) {
    let mut behavior = config_source.try_take()
        .map(|conf| conf.behavior)
        .unwrap_or_default();
    let mut prev_state: ButtonState = ButtonState::Off;

    defmt::debug!("starting button{} loop", id);
    loop {

        prev_state = match select(button.wait_for_low(), config_source.wait()).await {
            Either::First(_) => {
                defmt::debug!(
                    "will signal from button{}, channel has {} elements",
                    id,
                    sender.len()
                );

                let state = match behavior {
                    ButtonBehavior::Toggle => prev_state.toggle(),
                    ButtonBehavior::Momentary => ButtonState::On,
                    ButtonBehavior::Tap => ButtonState::On
                };

                sender
                    .send(ButtonMessage {
                        button_id: id,
                        state,
                    })
                    .await;

                Timer::after_millis(20).await;
                button.wait_for_high().await;

                defmt::debug!("button {} got hi", id);

                if behavior == ButtonBehavior::Momentary {
                    // send depress signal
                    sender
                        .send(ButtonMessage {
                            button_id: id,
                            state: ButtonState::Off,
                        })
                        .await;
                }

                // return ending state
                match behavior {
                    ButtonBehavior::Toggle => state,
                    ButtonBehavior::Momentary => ButtonState::Off,
                    ButtonBehavior::Tap => ButtonState::Off,
                }
            },
            Either::Second(config) => {
                // defmt::debug!("received button{} config.behavior: {}", id, config.behavior);
                behavior = config.behavior;
                ButtonState::Off // reset state
            },
        }
    }
}

/// constructs a USB-MIDI CC packet on channel 0
/// 
/// `control_number` and `value` are 7-bit numbers, first bit will be set to zero
fn midi_packet(control_number: u8, value: u8) -> [u8; 4] {
    [
        0x0b, // usb-midi header: 0x0_ == cable number, 0x_b == CC (tells receiver how many bytes to expect)
        0xb0, // midi status: 0xb_ == Control Change msg, 0x_0 == channel 0
        0b0111_1111 & control_number, // first bit in MIDI data byte should be 0
        0b0111_1111 & value, // same as above
    ]
}
