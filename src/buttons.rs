use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rppal::gpio::{Gpio, InputPin, Level};
use tokio::sync::mpsc;
use tracing::{debug, info};

const BUTTON_COUNT: usize = 4;
const GPIO_PINS: [u8; BUTTON_COUNT] = [17, 27, 22, 23];
const POLL_MS: u64 = 10;
const DEBOUNCE_MS: u64 = 30;
const LONG_PRESS_MS: u64 = 650;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonCommand {
    EnterRewindMode,
    EnterFastForwardMode,
    Previous,
    Next,
    Pause,
    Disconnect,
    Play,
    ApprovePairing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ButtonEvent {
    Short(usize),
    Long(usize),
}

pub fn spawn(tx: mpsc::UnboundedSender<ButtonCommand>) -> Result<thread::JoinHandle<()>> {
    let pins = open_pins()?;

    let handle = thread::Builder::new()
        .name("a2a2-gpio-buttons".into())
        .spawn(move || {
            info!(
                pins = ?GPIO_PINS,
                "listening for HS-KEY4B GPIO button events"
            );

            let mut state = ButtonState::new(read_mask(&pins), Instant::now());

            loop {
                let now = Instant::now();
                let mask = read_mask(&pins);
                for event in state.update(mask, now) {
                    if let Some(command) = map_event(event) {
                        debug!(?event, ?command, "GPIO button event");
                        if tx.send(command).is_err() {
                            break;
                        }
                    }
                }

                thread::sleep(Duration::from_millis(POLL_MS));
            }
        })
        .context("spawn GPIO button thread")?;

    Ok(handle)
}

fn open_pins() -> Result<[InputPin; BUTTON_COUNT]> {
    let gpio = Gpio::new().context("open Raspberry Pi GPIO")?;
    Ok([
        open_pin(&gpio, GPIO_PINS[0])?,
        open_pin(&gpio, GPIO_PINS[1])?,
        open_pin(&gpio, GPIO_PINS[2])?,
        open_pin(&gpio, GPIO_PINS[3])?,
    ])
}

fn open_pin(gpio: &Gpio, pin: u8) -> Result<InputPin> {
    let mut pin = gpio
        .get(pin)
        .with_context(|| format!("open GPIO{pin}"))?
        .into_input_pullup();
    pin.set_reset_on_drop(false);
    Ok(pin)
}

fn read_mask(pins: &[InputPin; BUTTON_COUNT]) -> u8 {
    pins.iter().enumerate().fold(0u8, |mut mask, (index, pin)| {
        if pin.read() == Level::Low {
            mask |= 1 << index;
        }
        mask
    })
}

fn map_event(event: ButtonEvent) -> Option<ButtonCommand> {
    match event {
        ButtonEvent::Short(0) => Some(ButtonCommand::EnterRewindMode),
        ButtonEvent::Long(0) => Some(ButtonCommand::Previous),
        ButtonEvent::Short(1) => Some(ButtonCommand::Pause),
        ButtonEvent::Long(1) => Some(ButtonCommand::Disconnect),
        ButtonEvent::Short(2) => Some(ButtonCommand::Play),
        ButtonEvent::Long(2) => Some(ButtonCommand::ApprovePairing),
        ButtonEvent::Short(3) => Some(ButtonCommand::EnterFastForwardMode),
        ButtonEvent::Long(3) => Some(ButtonCommand::Next),
        _ => None,
    }
}

struct ButtonState {
    stable_mask: u8,
    raw_mask: u8,
    debounce_due: [Option<Instant>; BUTTON_COUNT],
    pressed_at: [Option<Instant>; BUTTON_COUNT],
    long_sent_mask: u8,
}

impl ButtonState {
    fn new(raw_mask: u8, now: Instant) -> Self {
        let pressed_at = core::array::from_fn(|index| {
            if contains(raw_mask, index) {
                Some(now)
            } else {
                None
            }
        });

        Self {
            stable_mask: raw_mask,
            raw_mask,
            debounce_due: [None; BUTTON_COUNT],
            pressed_at,
            long_sent_mask: 0,
        }
    }

    fn update(&mut self, raw_mask: u8, now: Instant) -> Vec<ButtonEvent> {
        let mut events = Vec::new();

        for index in 0..BUTTON_COUNT {
            if contains(raw_mask, index) != contains(self.raw_mask, index) {
                self.debounce_due[index] = Some(now + Duration::from_millis(DEBOUNCE_MS));
            }
        }
        self.raw_mask = raw_mask;

        for index in 0..BUTTON_COUNT {
            let Some(due) = self.debounce_due[index] else {
                continue;
            };
            if now < due {
                continue;
            }
            self.debounce_due[index] = None;

            let pressed = contains(raw_mask, index);
            let was_pressed = contains(self.stable_mask, index);
            if pressed == was_pressed {
                continue;
            }

            if pressed {
                self.stable_mask = insert(self.stable_mask, index);
                self.pressed_at[index] = Some(now);
                self.long_sent_mask = remove(self.long_sent_mask, index);
            } else {
                self.stable_mask = remove(self.stable_mask, index);
                self.pressed_at[index] = None;
                if !contains(self.long_sent_mask, index) {
                    events.push(ButtonEvent::Short(index));
                }
                self.long_sent_mask = remove(self.long_sent_mask, index);
            }
        }

        for index in 0..BUTTON_COUNT {
            let Some(pressed_at) = self.pressed_at[index] else {
                continue;
            };
            if contains(self.long_sent_mask, index) {
                continue;
            }
            if now.duration_since(pressed_at) >= Duration::from_millis(LONG_PRESS_MS) {
                self.long_sent_mask = insert(self.long_sent_mask, index);
                events.push(ButtonEvent::Long(index));
            }
        }

        events
    }
}

fn contains(mask: u8, index: usize) -> bool {
    mask & (1 << index) != 0
}

fn insert(mask: u8, index: usize) -> u8 {
    mask | (1 << index)
}

fn remove(mask: u8, index: usize) -> u8 {
    mask & !(1 << index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(index: usize) -> u8 {
        1 << index
    }

    #[test]
    fn short_press_emits_on_release_after_debounce() {
        let start = Instant::now();
        let mut state = ButtonState::new(0, start);

        assert!(state.update(mask(0), start).is_empty());
        assert!(state
            .update(mask(0), start + Duration::from_millis(DEBOUNCE_MS + 1))
            .is_empty());
        assert!(state
            .update(0, start + Duration::from_millis(DEBOUNCE_MS + 20))
            .is_empty());

        let events = state.update(0, start + Duration::from_millis(DEBOUNCE_MS * 2 + 21));
        assert_eq!(events, vec![ButtonEvent::Short(0)]);
    }

    #[test]
    fn long_press_emits_once_and_suppresses_short() {
        let start = Instant::now();
        let mut state = ButtonState::new(0, start);

        state.update(mask(3), start);
        state.update(mask(3), start + Duration::from_millis(DEBOUNCE_MS + 1));

        let events = state.update(
            mask(3),
            start + Duration::from_millis(DEBOUNCE_MS + LONG_PRESS_MS + 1),
        );
        assert_eq!(events, vec![ButtonEvent::Long(3)]);

        assert!(state
            .update(
                mask(3),
                start + Duration::from_millis(DEBOUNCE_MS + LONG_PRESS_MS + 50)
            )
            .is_empty());
        state.update(
            0,
            start + Duration::from_millis(DEBOUNCE_MS + LONG_PRESS_MS + 60),
        );
        assert!(state
            .update(
                0,
                start + Duration::from_millis(DEBOUNCE_MS + LONG_PRESS_MS + 100)
            )
            .is_empty());
    }

    #[test]
    fn hs_key4b_mapping_matches_requested_layout() {
        assert_eq!(
            map_event(ButtonEvent::Short(0)),
            Some(ButtonCommand::EnterRewindMode)
        );
        assert_eq!(
            map_event(ButtonEvent::Long(0)),
            Some(ButtonCommand::Previous)
        );
        assert_eq!(map_event(ButtonEvent::Short(1)), Some(ButtonCommand::Pause));
        assert_eq!(
            map_event(ButtonEvent::Long(1)),
            Some(ButtonCommand::Disconnect)
        );
        assert_eq!(map_event(ButtonEvent::Short(2)), Some(ButtonCommand::Play));
        assert_eq!(
            map_event(ButtonEvent::Long(2)),
            Some(ButtonCommand::ApprovePairing)
        );
        assert_eq!(
            map_event(ButtonEvent::Short(3)),
            Some(ButtonCommand::EnterFastForwardMode)
        );
        assert_eq!(map_event(ButtonEvent::Long(3)), Some(ButtonCommand::Next));
    }
}
