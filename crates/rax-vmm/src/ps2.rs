//! An 8042 controller with a PS/2 keyboard and mouse.
//!
//! Scancodes pass straight through: Windows reports set 1, which is what the
//! 8042 delivers after translation, so no key table is needed. A guest sees
//! both devices at all times, so neither appears or vanishes when the host's
//! hardware changes.

use std::collections::VecDeque;

const STATUS_OUTPUT_FULL: u8 = 0x01;
const STATUS_SYSTEM: u8 = 0x04;
const STATUS_LAST_WAS_COMMAND: u8 = 0x08;
/// Set means the keylock is off.
const STATUS_UNLOCKED: u8 = 0x10;
const STATUS_FROM_MOUSE: u8 = 0x20;

const CONFIG_KEYBOARD_IRQ: u8 = 0x01;
const CONFIG_MOUSE_IRQ: u8 = 0x02;
const CONFIG_SYSTEM: u8 = 0x04;
const CONFIG_KEYBOARD_DISABLED: u8 = 0x10;
const CONFIG_MOUSE_DISABLED: u8 = 0x20;
const CONFIG_TRANSLATE: u8 = 0x40;

const ACK: u8 = 0xfa;
const SELF_TEST_OK: u8 = 0xaa;
const CONTROLLER_OK: u8 = 0x55;
const QUEUE_LIMIT: usize = 4096;

/// A physical event for the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestInput {
    /// A key changed. `scancode` is set 1, without the 0x80 release bit.
    Key {
        /// Set 1 scancode.
        scancode: u8,
        /// Whether the key has an 0xE0 prefix.
        extended: bool,
        /// True for press, false for release.
        down: bool,
    },
    /// The mouse moved. Positive `dy` is down the screen.
    MouseMove {
        /// Horizontal counts.
        dx: i32,
        /// Vertical counts.
        dy: i32,
    },
    /// A mouse button changed. Bit 0 is left, 1 right, 2 middle.
    MouseButtons(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    Config,
    MouseByte,
    MouseOutputLoop,
    KeyboardOutputLoop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceArgument {
    None,
    Expected,
}

/// Which interrupt a queued byte needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ps2Irq {
    /// Keyboard, IRQ 1.
    Keyboard,
    /// Mouse, IRQ 12.
    Mouse,
}

/// The controller and both devices.
#[derive(Debug)]
pub struct Ps2 {
    queue: VecDeque<(u8, bool)>,
    config: u8,
    pending: Pending,
    keyboard_argument: DeviceArgument,
    mouse_argument: DeviceArgument,
    mouse_enabled: bool,
    buttons: u8,
    last_was_command: bool,
    irq: Option<Ps2Irq>,
}

impl Default for Ps2 {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            config: CONFIG_KEYBOARD_IRQ | CONFIG_SYSTEM | CONFIG_TRANSLATE,
            pending: Pending::None,
            keyboard_argument: DeviceArgument::None,
            mouse_argument: DeviceArgument::None,
            mouse_enabled: false,
            buttons: 0,
            last_was_command: false,
            irq: None,
        }
    }
}

impl Ps2 {
    fn push(&mut self, byte: u8, from_mouse: bool) {
        if self.queue.len() < QUEUE_LIMIT {
            self.queue.push_back((byte, from_mouse));
        }
        self.update_irq();
    }

    fn update_irq(&mut self) {
        self.irq = match self.queue.front() {
            Some((_, true)) if self.config & CONFIG_MOUSE_IRQ != 0 => Some(Ps2Irq::Mouse),
            Some((_, false)) if self.config & CONFIG_KEYBOARD_IRQ != 0 => Some(Ps2Irq::Keyboard),
            _ => None,
        };
    }

    /// Returns a waiting interrupt request, once.
    pub fn take_irq(&mut self) -> Option<Ps2Irq> {
        self.irq.take()
    }

    /// Handles a read of port 0x60.
    pub fn read_data(&mut self) -> u8 {
        let byte = self.queue.pop_front().map_or(0, |(byte, _)| byte);
        self.update_irq();
        byte
    }

    /// Handles a read of port 0x64.
    #[must_use]
    pub fn read_status(&self) -> u8 {
        let mut status = STATUS_SYSTEM | STATUS_UNLOCKED;
        if let Some((_, from_mouse)) = self.queue.front() {
            status |= STATUS_OUTPUT_FULL;
            if *from_mouse {
                status |= STATUS_FROM_MOUSE;
            }
        }
        if self.last_was_command {
            status |= STATUS_LAST_WAS_COMMAND;
        }
        status
    }

    /// Handles a write to port 0x64.
    pub fn write_command(&mut self, command: u8) {
        self.last_was_command = true;
        match command {
            0x20 => self.push(self.config, false),
            0x60 => self.pending = Pending::Config,
            0xa7 => self.config |= CONFIG_MOUSE_DISABLED,
            0xa8 => self.config &= !CONFIG_MOUSE_DISABLED,
            0xad => self.config |= CONFIG_KEYBOARD_DISABLED,
            0xae => self.config &= !CONFIG_KEYBOARD_DISABLED,
            0xa9 | 0xab => self.push(0, false),
            0xaa => self.push(CONTROLLER_OK, false),
            0xd2 => self.pending = Pending::KeyboardOutputLoop,
            0xd3 => self.pending = Pending::MouseOutputLoop,
            0xd4 => self.pending = Pending::MouseByte,
            _ => {}
        }
    }

    /// Handles a write to port 0x60.
    pub fn write_data(&mut self, value: u8) {
        self.last_was_command = false;
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::Config => {
                self.config = value;
                self.update_irq();
            }
            Pending::MouseByte => self.mouse_command(value),
            Pending::MouseOutputLoop => self.push(value, true),
            Pending::KeyboardOutputLoop => self.push(value, false),
            Pending::None => self.keyboard_command(value),
        }
    }

    fn keyboard_command(&mut self, value: u8) {
        if self.keyboard_argument == DeviceArgument::Expected {
            self.keyboard_argument = DeviceArgument::None;
            self.push(ACK, false);
            return;
        }
        self.push(ACK, false);
        match value {
            0xff => self.push(SELF_TEST_OK, false),
            // Keyboard ID, as seen through translation.
            0xf2 => {
                self.push(0xab, false);
                self.push(0x41, false);
            }
            0xed | 0xf0 | 0xf3 => self.keyboard_argument = DeviceArgument::Expected,
            _ => {}
        }
    }

    fn mouse_command(&mut self, value: u8) {
        if self.mouse_argument == DeviceArgument::Expected {
            self.mouse_argument = DeviceArgument::None;
            self.push(ACK, true);
            return;
        }
        self.push(ACK, true);
        match value {
            0xff => {
                self.mouse_enabled = false;
                self.push(SELF_TEST_OK, true);
                self.push(0, true);
            }
            0xf2 => self.push(0, true),
            0xf3 | 0xe8 => self.mouse_argument = DeviceArgument::Expected,
            0xf4 => self.mouse_enabled = true,
            0xf5 | 0xf6 => self.mouse_enabled = false,
            0xe9 => {
                for byte in [0, 2, 100] {
                    self.push(byte, true);
                }
            }
            _ => {}
        }
    }

    /// Queues a physical event for the guest.
    pub fn input(&mut self, event: GuestInput) {
        match event {
            GuestInput::Key {
                scancode,
                extended,
                down,
            } => {
                if extended {
                    self.push(0xe0, false);
                }
                self.push(if down { scancode } else { scancode | 0x80 }, false);
            }
            GuestInput::MouseButtons(buttons) => {
                self.buttons = buttons & 0x07;
                self.mouse_packet(0, 0);
            }
            GuestInput::MouseMove { mut dx, mut dy } => {
                // A packet carries nine signed bits, so large moves split.
                while dx != 0 || dy != 0 {
                    let step_x = dx.clamp(-255, 255);
                    let step_y = dy.clamp(-255, 255);
                    self.mouse_packet(step_x, step_y);
                    dx -= step_x;
                    dy -= step_y;
                }
            }
        }
    }

    fn mouse_packet(&mut self, dx: i32, dy: i32) {
        if !self.mouse_enabled {
            return;
        }
        // PS/2 counts up as positive.
        let up = -dy;
        let mut flags = 0x08 | self.buttons;
        if dx < 0 {
            flags |= 0x10;
        }
        if up < 0 {
            flags |= 0x20;
        }
        self.push(flags, true);
        self.push(dx as u8, true);
        self.push(up as u8, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(ps2: &mut Ps2) -> Vec<u8> {
        let mut bytes = Vec::new();
        while ps2.read_status() & STATUS_OUTPUT_FULL != 0 {
            bytes.push(ps2.read_data());
        }
        bytes
    }

    fn mouse_ready() -> Ps2 {
        let mut ps2 = Ps2::default();
        ps2.write_command(0x60);
        ps2.write_data(CONFIG_KEYBOARD_IRQ | CONFIG_MOUSE_IRQ);
        ps2.write_command(0xd4);
        ps2.write_data(0xf4);
        read_all(&mut ps2);
        ps2
    }

    #[test]
    fn controller_self_tests_pass() {
        let mut ps2 = Ps2::default();
        ps2.write_command(0xaa);
        ps2.write_command(0xa9);
        ps2.write_command(0xab);
        assert_eq!(read_all(&mut ps2), [CONTROLLER_OK, 0, 0]);
    }

    #[test]
    fn config_byte_round_trips() {
        let mut ps2 = Ps2::default();
        ps2.write_command(0x60);
        ps2.write_data(0x47);
        ps2.write_command(0x20);
        assert_eq!(read_all(&mut ps2), [0x47]);
    }

    #[test]
    fn port_disable_commands_show_in_the_config_byte() {
        let mut ps2 = Ps2::default();
        ps2.write_command(0xa7);
        ps2.write_command(0x20);
        assert_ne!(read_all(&mut ps2)[0] & CONFIG_MOUSE_DISABLED, 0);
        ps2.write_command(0xa8);
        ps2.write_command(0x20);
        assert_eq!(read_all(&mut ps2)[0] & CONFIG_MOUSE_DISABLED, 0);
    }

    #[test]
    fn the_keylock_reads_as_off() {
        assert_ne!(Ps2::default().read_status() & STATUS_UNLOCKED, 0);
    }

    #[test]
    fn keyboard_reset_and_id_match_a_standard_keyboard() {
        let mut ps2 = Ps2::default();
        ps2.write_data(0xff);
        assert_eq!(read_all(&mut ps2), [ACK, SELF_TEST_OK]);
        ps2.write_data(0xf2);
        assert_eq!(read_all(&mut ps2), [ACK, 0xab, 0x41]);
    }

    #[test]
    fn keyboard_commands_with_arguments_ack_twice() {
        let mut ps2 = Ps2::default();
        ps2.write_data(0xed);
        ps2.write_data(0x02);
        assert_eq!(read_all(&mut ps2), [ACK, ACK]);
    }

    #[test]
    fn mouse_reset_and_id_match_a_standard_mouse() {
        let mut ps2 = Ps2::default();
        ps2.write_command(0xd4);
        ps2.write_data(0xff);
        assert_eq!(read_all(&mut ps2), [ACK, SELF_TEST_OK, 0]);
        ps2.write_command(0xd4);
        ps2.write_data(0xf2);
        assert_eq!(read_all(&mut ps2), [ACK, 0]);
    }

    #[test]
    fn the_status_register_marks_mouse_bytes() {
        let mut ps2 = Ps2::default();
        ps2.write_command(0xd4);
        ps2.write_data(0xf2);
        assert_ne!(ps2.read_status() & STATUS_FROM_MOUSE, 0);
    }

    #[test]
    fn loopback_commands_return_the_byte_on_the_right_channel() {
        let mut ps2 = Ps2::default();
        ps2.write_command(0xd3);
        ps2.write_data(0x5a);
        assert_ne!(ps2.read_status() & STATUS_FROM_MOUSE, 0);
        assert_eq!(read_all(&mut ps2), [0x5a]);
    }

    #[test]
    fn keys_produce_set_one_make_and_break() {
        let mut ps2 = Ps2::default();
        for down in [true, false] {
            ps2.input(GuestInput::Key {
                scancode: 0x1e,
                extended: false,
                down,
            });
        }
        assert_eq!(read_all(&mut ps2), [0x1e, 0x9e]);
    }

    #[test]
    fn extended_keys_get_a_prefix() {
        let mut ps2 = Ps2::default();
        ps2.input(GuestInput::Key {
            scancode: 0x48,
            extended: true,
            down: true,
        });
        assert_eq!(read_all(&mut ps2), [0xe0, 0x48]);
    }

    #[test]
    fn keyboard_input_raises_the_keyboard_interrupt() {
        let mut ps2 = Ps2::default();
        ps2.input(GuestInput::Key {
            scancode: 0x1e,
            extended: false,
            down: true,
        });
        assert_eq!(ps2.take_irq(), Some(Ps2Irq::Keyboard));
        assert_eq!(ps2.take_irq(), None);
    }

    #[test]
    fn a_disabled_mouse_stays_silent() {
        let mut ps2 = Ps2::default();
        ps2.input(GuestInput::MouseMove { dx: 5, dy: 5 });
        assert!(read_all(&mut ps2).is_empty());
    }

    #[test]
    fn mouse_motion_encodes_sign_bits_and_flips_y() {
        let mut ps2 = mouse_ready();
        ps2.input(GuestInput::MouseMove { dx: -3, dy: 4 });
        assert_eq!(read_all(&mut ps2), [0x08 | 0x10 | 0x20, 0xfd, 0xfc]);
    }

    #[test]
    fn large_moves_split_into_packets() {
        let mut ps2 = mouse_ready();
        ps2.input(GuestInput::MouseMove { dx: 300, dy: 0 });
        let bytes = read_all(&mut ps2);
        assert_eq!(bytes.len(), 6);
        assert_eq!(u32::from(bytes[1]) + u32::from(bytes[4]), 300);
    }

    #[test]
    fn buttons_stay_in_later_packets() {
        let mut ps2 = mouse_ready();
        ps2.input(GuestInput::MouseButtons(1));
        ps2.input(GuestInput::MouseMove { dx: 1, dy: 0 });
        let bytes = read_all(&mut ps2);
        assert_eq!(bytes[0], 0x09);
        assert_eq!(bytes[3], 0x09);
    }

    #[test]
    fn mouse_data_raises_the_mouse_interrupt() {
        let mut ps2 = mouse_ready();
        ps2.input(GuestInput::MouseButtons(1));
        assert_eq!(ps2.take_irq(), Some(Ps2Irq::Mouse));
    }

    #[test]
    fn keyboard_and_mouse_work_independently() {
        let mut ps2 = mouse_ready();
        ps2.input(GuestInput::MouseMove { dx: 1, dy: 0 });
        ps2.input(GuestInput::Key {
            scancode: 0x1e,
            extended: false,
            down: true,
        });
        assert_eq!(read_all(&mut ps2), [0x08, 1, 0, 0x1e]);
    }
}
