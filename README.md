# midi-pedal

## overview

firmware for a midi footswitch

this was my introduction to embedded systems and midi. recently taught myself rust and wanted a project that i could use that on. plus i have been using my audio interface/amp modelers on my daw more than my physical amp and wanted a way to be able to control things while i play guitar. this was a good way to accomplish both

## hardware

| front                   | guts                   |
|-------------------------|------------------------| 
| ![front](/assets/0.jpg) | ![guts](/assets/1.jpg) |

this is all running on a raspberry pi pico 2. soldered on some switches, drilled some holes in a blank enclosure, and added a usb panel mount connector. maybe one day i'll add some leds and draw something nice on the outside

## midi controller config

the switches are mapped to midi controllers 20-26, starting from the bottom left to the top right. each of these send out 127 (on) or 0 (off) according to their state, and their behavior is configurable by sending midi cc messages to controllers 30-36, respectively.

supported behaviors/value to send to set for each controller:

| behavior  | midi cc value | explanation                     |
|-----------|---------------|---------------------------------|
| toggle    | 0-42          | simple on/off toggle            |
| momentary | 23-85         | hold to stay on, off otherwise  |
| tap       | 86-127        | sends on only when pressed down |

if for whatever reason persistance of the above config is required, this also does it. in retrospect, this was unnecessary for my use case as my daw sends its default config when i open it up, but if you need it, it's there. at least i learned something about flash memory
