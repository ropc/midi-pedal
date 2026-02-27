#![no_std]
#![no_main]

use core::ops::Range;

use defmt::Format;
use embassy_executor::Spawner;
use embassy_rp::flash::{Async, Flash};
use embassy_rp::gpio::{Input, Pull};
use embassy_rp::{bind_interrupts, peripherals::USB, usb};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, self};
use embassy_sync::pubsub::{PubSubBehavior, PubSubChannel, Publisher, Subscriber};
use embassy_time::{with_timeout, Duration, Timer};
use embassy_usb::class::midi::MidiClass;
use embassy_futures::select::{select, Either};
use sequential_storage::cache::KeyPointerCache;
use sequential_storage::map::{MapConfig, MapStorage, PostcardValue};
use static_cell::StaticCell;
use serde::{Serialize, Deserialize};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

const FLASH_SIZE: usize = 4 * 1024 * 1024;
// the driver uses offsets, hence not starting at 0x103F8000 like in memory.x
const FLASH_STORAGE_RANGE: Range<u32> = 0x003F_8000..0x0040_0000;
// technically, this would be aligned/rounded up to 256-byte writes, but this is handled internally by the driver
const FLASH_BUFFER_SIZE: usize = size_of::<ButtonConfig>() + size_of::<u8>();

static BUTTON_ACTION_CHANNEL: Channel<CriticalSectionRawMutex, ButtonMessage<ButtonState>, 16> = Channel::new();
static MIDI_INPUT_CHANNEL: PubSubChannel<CriticalSectionRawMutex, ButtonMessage<ButtonConfig>, 16, 7, 1> = PubSubChannel::new();

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

    // flash storage setup

    let mut flash = embassy_rp::flash::Flash::<_, Async, FLASH_SIZE>::new(p.FLASH, p.DMA_CH0);
    let mut map_storage = MapStorage::new(
        flash,
        const { MapConfig::new(FLASH_STORAGE_RANGE) },
        KeyPointerCache::<32, u8, 6>::new()
    );

    // button setup

    let button0_pin = Input::new(p.PIN_1, Pull::Up);
    let button1_pin = Input::new(p.PIN_22, Pull::Up);
    let button2_pin = Input::new(p.PIN_18, Pull::Up);
    let button3_pin = Input::new(p.PIN_5, Pull::Up);
    let button4_pin = Input::new(p.PIN_9, Pull::Up);
    let button5_pin = Input::new(p.PIN_14, Pull::Up);

    let button_press_sender = BUTTON_ACTION_CHANNEL.sender();
    spawner.spawn(button_task(0, MIDI_INPUT_CHANNEL.subscriber().unwrap(), button0_pin, button_press_sender)).unwrap();
    spawner.spawn(button_task(1, MIDI_INPUT_CHANNEL.subscriber().unwrap(), button1_pin, button_press_sender)).unwrap();
    spawner.spawn(button_task(2, MIDI_INPUT_CHANNEL.subscriber().unwrap(), button2_pin, button_press_sender)).unwrap();
    spawner.spawn(button_task(3, MIDI_INPUT_CHANNEL.subscriber().unwrap(), button3_pin, button_press_sender)).unwrap();
    spawner.spawn(button_task(4, MIDI_INPUT_CHANNEL.subscriber().unwrap(), button4_pin, button_press_sender)).unwrap();
    spawner.spawn(button_task(5, MIDI_INPUT_CHANNEL.subscriber().unwrap(), button5_pin, button_press_sender)).unwrap();

    // initialize button state

    let mut buf = [0; FLASH_BUFFER_SIZE];
    for index in 0..6 {
        if let Ok(Some(config)) = map_storage.fetch_item::<ButtonConfig>(&mut buf, &(index as u8)).await {
            defmt::info!("loaded button{} initial value: {}", index, config);
            MIDI_INPUT_CHANNEL.publish_immediate(ButtonMessage {
                button_id: index,
                payload: config,
            });
        }
    }

    spawner.spawn(save_config_task(map_storage, MIDI_INPUT_CHANNEL.subscriber().unwrap())).unwrap();

    // midi cc output loop

    defmt::debug!("starting midi controller");
    let mut buf = [0; 64];
    let mut state_buf: [Option<ButtonConfig>; 6] = [None; 6];

    loop {
        defmt::debug!("waiting for messages");
        match select(BUTTON_ACTION_CHANNEL.receive(), midi_device.read_packet(&mut buf)).await {
            Either::First(button_message) => handle_button_message(button_message, &mut midi_device).await,
            Either::Second(Ok(midi_message_size)) => handle_midi_message(&buf[..midi_message_size], MIDI_INPUT_CHANNEL.publisher().unwrap(), &mut state_buf),
            Either::Second(Err(err)) => defmt::warn!("midi error: {}", err),
        };
    }
}

async fn handle_button_message(
    button_message: ButtonMessage<ButtonState>,
    midi_device: &mut MidiClass<'static, MyUsbDriver>
) {
    let control_number = button_message.button_id + 20; // use MIDI CC range 20-26
    let value = match button_message.payload {
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
}

fn handle_midi_message(
    message: &[u8],
    publisher: Publisher<'static, CriticalSectionRawMutex, ButtonMessage<ButtonConfig>, 16, 7, 1>,
    state_buf: &mut [Option<ButtonConfig>; 6],
) {
    if message.len() != 4 {
        return;  // wrong size for CC message
    }
    if message[..2] != [0x0b, 0xb0] {
        return;  // wrong headers/channel for CC message
    }

    // received CC message, only controllers 30-36 are valid
    let controller = message[2];
    if !(30..=36).contains(&controller) {
        return;  // invalid controller
    }

    let index = usize::from(controller - 30);

    // set button behavior according to value
    let value = message[3];
    let behavior = ButtonBehavior::from(value);

    defmt::debug!("received midi message for controller {} (button {}): {}", controller, index, behavior);

    // filter out unchanged behavior
    if let Some(old_config) = state_buf[index] && old_config.behavior == behavior {
        return; // no change, don't need to update button
    }

    let config = ButtonConfig { behavior: behavior };

    // publish_immediate will drop old messages
    publisher.publish_immediate(ButtonMessage {
        button_id: index as u8,
        payload: config,
    });

    state_buf[index] = Some(config.clone());
}

type MyUsbDriver = usb::Driver<'static, USB>;
type MyUsbDevice = embassy_usb::UsbDevice<'static, MyUsbDriver>;

#[embassy_executor::task]
async fn usb_task(mut usb: MyUsbDevice) -> ! {
    usb.run().await
}

#[derive(Clone, Copy, Format)]
struct ButtonMessage<T> {
    button_id: u8,
    payload: T,
}

#[derive(Debug, Format, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ButtonBehavior {
    #[default]
    Toggle,
    Momentary,
    Tap,
}

impl From<u8> for ButtonBehavior {
    fn from(value: u8) -> Self {
        match value {
            0..43 => ButtonBehavior::Toggle,
            43..86 => ButtonBehavior::Momentary,
            86..128 => ButtonBehavior::Tap,
            _ => ButtonBehavior::Toggle, // default to toggle
        }
    }
}

impl PostcardValue<'_> for ButtonBehavior {}

#[derive(Debug, Clone, Copy, Format)]
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Format)]
struct ButtonConfig {
    behavior: ButtonBehavior,
}

impl PostcardValue<'_> for ButtonConfig {}

#[embassy_executor::task(pool_size = 6)]
async fn button_task(
    id: u8,
    mut config_source: Subscriber<'static, CriticalSectionRawMutex, ButtonMessage<ButtonConfig>, 16, 7, 1>,
    mut button: Input<'static>,
    sender: channel::Sender<'static, CriticalSectionRawMutex, ButtonMessage<ButtonState>, 16>,
) {
    let mut behavior = ButtonBehavior::default();
    let mut prev_state: ButtonState = ButtonState::Off;

    loop {
        defmt::debug!("button{} listening for messages. current state: {}", id, prev_state);

        prev_state = match select(button.wait_for_low(), config_source.next_message_pure()).await {
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
                        payload: state,
                    })
                    .await;

                Timer::after_millis(20).await;  // ignore jitter while pressing down
                button.wait_for_high().await;

                defmt::debug!("button {} got hi", id);

                if behavior == ButtonBehavior::Momentary {
                    // send depress signal
                    sender
                        .send(ButtonMessage {
                            button_id: id,
                            payload: ButtonState::Off,
                        })
                        .await;
                }

                Timer::after_millis(20).await;  // ignore jitter while releasing

                // return ending state
                match behavior {
                    ButtonBehavior::Toggle => state,
                    ButtonBehavior::Momentary => ButtonState::Off,
                    ButtonBehavior::Tap => ButtonState::Off,
                }
            },
            Either::Second(message) => {
                if message.button_id != id {
                    continue;
                }
                defmt::debug!("received button{} config.behavior: {}", id, message.payload.behavior);
                behavior = message.payload.behavior;
                ButtonState::Off // reset state
            },
        }
    }
}

#[embassy_executor::task]
async fn save_config_task(
    mut map_storage: MapStorage<u8, Flash<'static, embassy_rp::peripherals::FLASH, Async, FLASH_SIZE>, KeyPointerCache<32, u8, 6>>,
    mut subscriber: Subscriber<'static, CriticalSectionRawMutex, ButtonMessage<ButtonConfig>, 16, 7, 1>,
) {
    let mut buf = [0; FLASH_BUFFER_SIZE];

    loop {
        let mut values_buf: [Option<ButtonMessage<ButtonConfig>>; 6] = [None; 6];
        debounced_next_set(&mut subscriber, &mut values_buf).await;

        for message in values_buf.iter().flatten() {
            defmt::info!("storing button{} config: {}", message.button_id, message.payload);
            let result = map_storage.store_item(&mut buf, &message.button_id, &message.payload).await;
            defmt::debug!("store result: {}", result);
            
            if let Err(err) = result {
                defmt::error!("failed to store value. {:?}", err);
            }
        }
    }
}

async fn debounced_next_set(
    subscriber: &mut Subscriber<'static, CriticalSectionRawMutex, ButtonMessage<ButtonConfig>, 16, 7, 1>,
    values_buf: &mut [Option<ButtonMessage<ButtonConfig>>; 6],
) {
    let message = subscriber.next_message_pure().await;
    values_buf[usize::from(message.button_id)] = Some(message);

    loop {
        match select(subscriber.next_message_pure(), Timer::after_secs(2)).await {
            Either::First(message) => {
                values_buf[usize::from(message.button_id)] = Some(message);
            },
            Either::Second(_) => break,  // timer elapsed, return unchanged values
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
