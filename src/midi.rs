#![no_std]
#![no_main]

use embassy_executor::{SpawnToken};
use embassy_rp::gpio::Input;
use embassy_time::Timer;
use embassy_usb::{class::midi::MidiClass, driver::EndpointError};
use embassy_sync::channel::{Channel, Sender};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use crate::MyUsbDriver;

pub struct Disconnected {}

impl From<EndpointError> for Disconnected {
    fn from(val: EndpointError) -> Self {
        match val {
            EndpointError::BufferOverflow => panic!("Buffer overflow"),
            EndpointError::Disabled => Disconnected {},
        }
    }
}

struct ButtonMessage {
    button_id: u8,
    state: ButtonState,
}

#[derive(Debug, Clone, Copy)]
enum ButtonState {
    Pressed, Released
}

impl ButtonState {
    fn toggle(self) -> Self {
        match self {
            ButtonState::Pressed => ButtonState::Released,
            ButtonState::Released => ButtonState::Pressed,
        }
    }
}

static CHANNEL: Channel<CriticalSectionRawMutex, ButtonMessage, 16> = Channel::new();

pub struct MidiHandler<'a> {
    device: &'a mut MidiClass<'static, MyUsbDriver>,
    button_handlers: [ButtonHandler; 6], // 6 buttons on midi pedal
}

impl<'a> MidiHandler<'a> {
    pub fn new(device: &'a mut MidiClass<'static, MyUsbDriver>, inputs: [&'static mut Input<'static>; 6]) -> Self {
        let [b0, b1, b2, b3, b4, b5] = inputs;
        Self {
            device,
            button_handlers: [
                ButtonHandler { id: 0, is_momentary: false, button: b0, midi_sender: CHANNEL.sender(), },
                ButtonHandler { id: 1, is_momentary: false, button: b1, midi_sender: CHANNEL.sender(), },
                ButtonHandler { id: 2, is_momentary: false, button: b2, midi_sender: CHANNEL.sender(), },
                ButtonHandler { id: 3, is_momentary: false, button: b3, midi_sender: CHANNEL.sender(), },
                ButtonHandler { id: 4, is_momentary: false, button: b4, midi_sender: CHANNEL.sender(), },
                ButtonHandler { id: 5, is_momentary: false, button: b5, midi_sender: CHANNEL.sender(), },
            ],
        }
    }

    pub fn button_tasks(&self) -> [SpawnToken<impl Sized>; 6] {
        self.button_handlers
            .map(|button_handler| button_task(button_handler))
    }

    pub async fn run(&self) -> Result<(), Disconnected> {  
        loop {
            let button_message = CHANNEL.receive().await;
            let control_number = button_message.button_id + 20;  // use MIDI CC range 20-26
            let value = match button_message.state {
                ButtonState::Pressed => 0xff,
                ButtonState::Released => 0x00,
            };

            let packet = midi_packet(control_number, value);
            let result = self.device.write_packet(&packet).await;
            defmt::debug!("sent packet {:?}: {}", packet, result);
        }
    }
}

pub struct ButtonHandler {
    id: u8,
    is_momentary: bool,
    midi_sender: Sender<'static, CriticalSectionRawMutex, ButtonMessage, 16>,
    button: &'static mut Input<'static>,
}

impl ButtonHandler {
    async fn run(&mut self) -> Result<(), Disconnected> {
        let mut state: ButtonState = ButtonState::Pressed;

        loop {
            // wait for transition 
            self.button.wait_for_low().await;

            self.midi_sender.send(ButtonMessage { button_id: self.id, state }).await;

            Timer::after_millis(20).await;
            self.button.wait_for_high().await;

            if self.is_momentary {
                // send depress signal
                self.midi_sender.send(ButtonMessage { button_id: self.id, state: ButtonState::Released }).await;
            } else {
                // not a momentary switch: toggle value
                state = state.toggle();
            }
        }
    }
}

#[embassy_executor::task]
async fn button_task(mut button_handler: ButtonHandler) {
    button_handler.run().await;
}

/// constructs a USB-MIDI CC packet on channel 0
fn midi_packet(control_number: u8, value: u8) -> [u8; 4] {
    [
        0x0b,  // usb-midi header: 0x0_ == cable number, 0x_b == CC (tells receiver how many bytes to expect)
        0xb0,  // midi status: 0xb_ == Control Change msg, 0x_0 == channel 0
        control_number,
        value
    ]
}
