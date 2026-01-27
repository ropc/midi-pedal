#![no_std]
#![no_main]

use embassy_rp::gpio::Input;
use embassy_time::Timer;
use embassy_usb::{class::midi::MidiClass, driver::EndpointError};

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

pub async fn run_handler(device: &mut MidiClass<'static, MyUsbDriver>, button0: &mut Input<'static>) -> Result<(), Disconnected>{
    let mut is_momentary = false;
    let mut value = 0xff;  // midi value to send, first press will be 127 (ON)

    loop {
        // wait for transition 
        button0.wait_for_low().await;

        let packet = midi_packet(20, value);
        let result = device.write_packet(&packet).await;
        defmt::debug!("sent packet {:?}: {}", packet, result);

        Timer::after_millis(20).await;
        button0.wait_for_high().await;

        // send depress signal
        if (is_momentary) {
            let packet = midi_packet(20, value);
            device.write_packet(&packet).await;
            defmt::debug!("sent packet {:?}: {}", packet, result);
        } else {
            // not a momentary switch: toggle value
            value = value ^ 0xff;
        }
    }
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
