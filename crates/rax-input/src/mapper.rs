//! Turns physical input into touch events.

use std::collections::HashSet;

use crate::key::{Key, MouseButton};
use crate::profile::{Binding, MAX_SLOTS, Point, Profile};

/// The slot used for the desktop-style pointer.
const POINTER_SLOT: u8 = MAX_SLOTS as u8;

/// A class of input device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// Anything that produces keys.
    Keyboard,
    /// Anything that produces motion and buttons.
    Mouse,
}

/// A physical input event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// A key went down. Auto-repeat is ignored.
    KeyDown(Key),
    /// A key came up.
    KeyUp(Key),
    /// A mouse button went down.
    ButtonDown(MouseButton),
    /// A mouse button came up.
    ButtonUp(MouseButton),
    /// Relative mouse motion, in mouse counts.
    MouseMove {
        /// Horizontal counts.
        dx: f32,
        /// Vertical counts.
        dy: f32,
    },
    /// The absolute cursor position, used in pointer mode.
    CursorAt(Point),
    /// A device was unplugged. Only its own held controls are released.
    DeviceLost(DeviceKind),
    /// The window lost focus. Everything is released.
    FocusLost,
}

/// What a finger did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Finger touched the screen.
    Down,
    /// Finger moved.
    Move,
    /// Finger lifted.
    Up,
}

/// One touch event for the guest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Touch {
    /// Which finger.
    pub slot: u8,
    /// What it did.
    pub phase: Phase,
    /// Where it is.
    pub at: Point,
}

fn clamp(point: Point) -> Point {
    Point {
        x: point.x.clamp(0.0, 1.0),
        y: point.y.clamp(0.0, 1.0),
    }
}

/// Maintains which fingers are down.
pub struct Mapper {
    profile: Profile,
    held_keys: HashSet<Key>,
    held_buttons: HashSet<MouseButton>,
    /// Whether each profile slot's finger is on the screen.
    finger_down: [bool; MAX_SLOTS],
    stick: Option<Point>,
    look_position: Option<Point>,
    pointer_mode: bool,
    pointer_down: bool,
    cursor: Point,
}

impl Mapper {
    /// Creates a mapper with every finger up.
    #[must_use]
    pub fn new(profile: Profile) -> Self {
        Self {
            profile,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            finger_down: [false; MAX_SLOTS],
            stick: None,
            look_position: None,
            pointer_mode: false,
            pointer_down: false,
            cursor: Point { x: 0.5, y: 0.5 },
        }
    }

    /// Whether the mouse is a normal pointer rather than game controls.
    #[must_use]
    pub fn is_pointer_mode(&self) -> bool {
        self.pointer_mode
    }

    /// Handles one physical event, returning the touches it causes.
    pub fn handle(&mut self, input: Input) -> Vec<Touch> {
        let mut touches = Vec::new();
        match input {
            Input::KeyDown(key) => self.key_down(key, &mut touches),
            Input::KeyUp(key) => self.key_up(key, &mut touches),
            Input::ButtonDown(button) => self.button_down(button, &mut touches),
            Input::ButtonUp(button) => self.button_up(button, &mut touches),
            Input::MouseMove { dx, dy } => self.mouse_move(dx, dy, &mut touches),
            Input::CursorAt(point) => self.cursor_at(point, &mut touches),
            Input::DeviceLost(DeviceKind::Keyboard) => self.release_keyboard(&mut touches),
            Input::DeviceLost(DeviceKind::Mouse) => self.release_mouse(&mut touches),
            Input::FocusLost => {
                self.release_keyboard(&mut touches);
                self.release_mouse(&mut touches);
            }
        }
        touches
    }

    fn key_down(&mut self, key: Key, touches: &mut Vec<Touch>) {
        if !self.held_keys.insert(key) {
            return; // Windows auto-repeat.
        }
        if key == self.profile.pointer_toggle {
            self.toggle_pointer(touches);
        } else if !self.pointer_mode {
            self.apply_key(key, touches);
        }
    }

    fn key_up(&mut self, key: Key, touches: &mut Vec<Touch>) {
        if self.held_keys.remove(&key) && !self.pointer_mode {
            self.apply_key(key, touches);
        }
    }

    /// Re-evaluates the bindings a key belongs to against current state.
    fn apply_key(&mut self, key: Key, touches: &mut Vec<Touch>) {
        for slot in 0..self.profile.bindings.len() {
            match self.profile.bindings[slot] {
                Binding::Key { key: bound, at } if bound == key => {
                    let held = self.held_keys.contains(&key);
                    self.set_finger(slot, held, at, touches);
                }
                Binding::Joystick {
                    up,
                    down,
                    left,
                    right,
                    center,
                    radius,
                } if [up, down, left, right].contains(&key) => {
                    self.update_stick(slot, [up, down, left, right], center, radius, touches);
                }
                _ => {}
            }
        }
    }

    fn set_finger(&mut self, slot: usize, down: bool, at: Point, touches: &mut Vec<Touch>) {
        if self.finger_down[slot] == down {
            return;
        }
        self.finger_down[slot] = down;
        touches.push(Touch {
            slot: slot as u8,
            phase: if down { Phase::Down } else { Phase::Up },
            at,
        });
    }

    fn update_stick(
        &mut self,
        slot: usize,
        [up, down, left, right]: [Key; 4],
        center: Point,
        radius: f32,
        touches: &mut Vec<Touch>,
    ) {
        let axis = |positive: Key, negative: Key| {
            f32::from(i8::from(self.held_keys.contains(&positive)))
                - f32::from(i8::from(self.held_keys.contains(&negative)))
        };
        let (x, y) = (axis(right, left), axis(down, up));
        let any_held = [up, down, left, right]
            .iter()
            .any(|key| self.held_keys.contains(key));
        if !any_held {
            let at = self.stick.take().unwrap_or(center);
            self.set_finger(slot, false, at, touches);
            return;
        }
        // Diagonals keep the same speed as straight movement.
        let length = (x * x + y * y).sqrt().max(1.0);
        let target = clamp(Point {
            x: center.x + x / length * radius,
            y: center.y + y / length * radius,
        });
        // Games anchor a stick where the finger lands, so it lands on the centre.
        self.set_finger(slot, true, center, touches);
        if self.stick != Some(target) {
            self.stick = Some(target);
            touches.push(Touch {
                slot: slot as u8,
                phase: Phase::Move,
                at: target,
            });
        }
    }

    fn button_down(&mut self, button: MouseButton, touches: &mut Vec<Touch>) {
        if !self.held_buttons.insert(button) {
            return;
        }
        if self.pointer_mode {
            if button == MouseButton::Left {
                self.pointer_down = true;
                touches.push(Touch {
                    slot: POINTER_SLOT,
                    phase: Phase::Down,
                    at: self.cursor,
                });
            }
        } else {
            self.apply_button(button, touches);
        }
    }

    fn button_up(&mut self, button: MouseButton, touches: &mut Vec<Touch>) {
        if !self.held_buttons.remove(&button) {
            return;
        }
        if self.pointer_mode {
            if button == MouseButton::Left {
                self.lift_pointer(touches);
            }
        } else {
            self.apply_button(button, touches);
        }
    }

    fn apply_button(&mut self, button: MouseButton, touches: &mut Vec<Touch>) {
        for slot in 0..self.profile.bindings.len() {
            if let Binding::Button { button: bound, at } = self.profile.bindings[slot]
                && bound == button
            {
                let held = self.held_buttons.contains(&button);
                self.set_finger(slot, held, at, touches);
            }
        }
    }

    fn mouse_move(&mut self, dx: f32, dy: f32, touches: &mut Vec<Touch>) {
        if self.pointer_mode {
            return;
        }
        for slot in 0..self.profile.bindings.len() {
            if let Binding::Look {
                origin,
                sensitivity,
                range,
            } = self.profile.bindings[slot]
            {
                self.look(slot, dx, dy, (origin, sensitivity, range), touches);
            }
        }
    }

    fn look(
        &mut self,
        slot: usize,
        dx: f32,
        dy: f32,
        (origin, sensitivity, range): (Point, f32, f32),
        touches: &mut Vec<Touch>,
    ) {
        let start = self.look_position.unwrap_or(origin);
        self.set_finger(slot, true, origin, touches);
        let next = clamp(Point {
            x: start.x + dx * sensitivity,
            y: start.y + dy * sensitivity,
        });
        touches.push(Touch {
            slot: slot as u8,
            phase: Phase::Move,
            at: next,
        });
        // A real finger eventually reaches the screen edge, so lift it and
        // start again from the origin. Aiming continues without a jump.
        let far = (next.x - origin.x).abs() >= range || (next.y - origin.y).abs() >= range;
        if far {
            self.look_position = None;
            self.set_finger(slot, false, next, touches);
        } else {
            self.look_position = Some(next);
        }
    }

    fn cursor_at(&mut self, point: Point, touches: &mut Vec<Touch>) {
        self.cursor = clamp(point);
        if self.pointer_down {
            touches.push(Touch {
                slot: POINTER_SLOT,
                phase: Phase::Move,
                at: self.cursor,
            });
        }
    }

    fn lift_pointer(&mut self, touches: &mut Vec<Touch>) {
        if std::mem::take(&mut self.pointer_down) {
            touches.push(Touch {
                slot: POINTER_SLOT,
                phase: Phase::Up,
                at: self.cursor,
            });
        }
    }

    fn toggle_pointer(&mut self, touches: &mut Vec<Touch>) {
        self.lift_game_fingers(touches);
        self.lift_pointer(touches);
        self.pointer_mode = !self.pointer_mode;
    }

    /// Lifts every finger, keeping the record of what is physically held.
    fn lift_game_fingers(&mut self, touches: &mut Vec<Touch>) {
        for slot in 0..self.profile.bindings.len() {
            let at = match self.profile.bindings[slot] {
                Binding::Key { at, .. } | Binding::Button { at, .. } => at,
                Binding::Joystick { center, .. } => self.stick.take().unwrap_or(center),
                Binding::Look { origin, .. } => self.look_position.take().unwrap_or(origin),
            };
            self.set_finger(slot, false, at, touches);
        }
    }

    fn release_keyboard(&mut self, touches: &mut Vec<Touch>) {
        let held: Vec<Key> = self.held_keys.drain().collect();
        if self.pointer_mode {
            return;
        }
        for key in held {
            self.apply_key(key, touches);
        }
    }

    fn release_mouse(&mut self, touches: &mut Vec<Touch>) {
        let held: Vec<MouseButton> = self.held_buttons.drain().collect();
        self.lift_pointer(touches);
        if !self.pointer_mode {
            for button in held {
                self.apply_button(button, touches);
            }
            self.lift_look(touches);
        }
    }

    fn lift_look(&mut self, touches: &mut Vec<Touch>) {
        for slot in 0..self.profile.bindings.len() {
            if let Binding::Look { origin, .. } = self.profile.bindings[slot] {
                let at = self.look_position.take().unwrap_or(origin);
                self.set_finger(slot, false, at, touches);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Key = Key(0x57);
    const A: Key = Key(0x41);
    const S: Key = Key(0x53);
    const D: Key = Key(0x44);
    const R: Key = Key(0x52);
    const ALT: Key = Key(0x12);

    const STICK_SLOT: u8 = 0;
    const RELOAD_SLOT: u8 = 1;
    const FIRE_SLOT: u8 = 2;
    const LOOK_SLOT: u8 = 3;

    fn mapper() -> Mapper {
        let profile = Profile::from_json(
            r#"{"name":"t","pointer_toggle":"Alt","bindings":[
              {"type":"joystick","up":"W","down":"S","left":"A","right":"D",
               "center":{"x":0.2,"y":0.7},"radius":0.1},
              {"type":"key","key":"R","at":{"x":0.8,"y":0.3}},
              {"type":"button","button":"Left","at":{"x":0.9,"y":0.6}},
              {"type":"look","origin":{"x":0.6,"y":0.4},"sensitivity":0.01,"range":0.2}
            ]}"#,
        )
        .unwrap();
        Mapper::new(profile)
    }

    fn phases(touches: &[Touch]) -> Vec<(u8, Phase)> {
        touches.iter().map(|t| (t.slot, t.phase)).collect()
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.0001
    }

    #[test]
    fn a_key_holds_a_finger_until_released() {
        let mut mapper = mapper();
        assert_eq!(
            phases(&mapper.handle(Input::KeyDown(R))),
            [(RELOAD_SLOT, Phase::Down)]
        );
        assert_eq!(
            phases(&mapper.handle(Input::KeyUp(R))),
            [(RELOAD_SLOT, Phase::Up)]
        );
    }

    #[test]
    fn auto_repeat_does_not_press_again() {
        let mut mapper = mapper();
        mapper.handle(Input::KeyDown(R));
        assert!(mapper.handle(Input::KeyDown(R)).is_empty());
    }

    #[test]
    fn a_release_without_a_press_does_nothing() {
        let mut mapper = mapper();
        assert!(mapper.handle(Input::KeyUp(R)).is_empty());
        assert!(mapper.handle(Input::ButtonUp(MouseButton::Left)).is_empty());
    }

    #[test]
    fn unbound_controls_produce_nothing() {
        let mut mapper = mapper();
        assert!(mapper.handle(Input::KeyDown(Key(0x51))).is_empty());
        assert!(
            mapper
                .handle(Input::ButtonDown(MouseButton::Back))
                .is_empty()
        );
    }

    #[test]
    fn the_stick_lands_on_its_centre_then_moves() {
        let mut mapper = mapper();
        let touches = mapper.handle(Input::KeyDown(W));
        assert_eq!(
            phases(&touches),
            [(STICK_SLOT, Phase::Down), (STICK_SLOT, Phase::Move)]
        );
        assert_eq!(touches[0].at, Point { x: 0.2, y: 0.7 });
        assert!(close(touches[1].at.x, 0.2) && close(touches[1].at.y, 0.6));
    }

    #[test]
    fn diagonals_keep_the_same_speed() {
        let mut mapper = mapper();
        mapper.handle(Input::KeyDown(W));
        let touches = mapper.handle(Input::KeyDown(D));
        let at = touches[0].at;
        let (dx, dy) = (at.x - 0.2, at.y - 0.7);
        assert!(close((dx * dx + dy * dy).sqrt(), 0.1));
    }

    #[test]
    fn opposite_keys_cancel_but_the_finger_stays_down() {
        let mut mapper = mapper();
        mapper.handle(Input::KeyDown(A));
        let touches = mapper.handle(Input::KeyDown(D));
        assert_eq!(phases(&touches), [(STICK_SLOT, Phase::Move)]);
        assert!(close(touches[0].at.x, 0.2));
    }

    #[test]
    fn the_stick_lifts_only_when_every_key_is_up() {
        let mut mapper = mapper();
        mapper.handle(Input::KeyDown(W));
        mapper.handle(Input::KeyDown(S));
        assert!(
            !phases(&mapper.handle(Input::KeyUp(W)))
                .iter()
                .any(|&(_, phase)| phase == Phase::Up)
        );
        assert_eq!(
            phases(&mapper.handle(Input::KeyUp(S))),
            [(STICK_SLOT, Phase::Up)]
        );
    }

    #[test]
    fn moving_and_firing_and_aiming_all_work_together() {
        let mut mapper = mapper();
        mapper.handle(Input::KeyDown(W));
        mapper.handle(Input::ButtonDown(MouseButton::Left));
        let look = mapper.handle(Input::MouseMove { dx: 1.0, dy: 0.0 });
        assert!(look.iter().all(|touch| touch.slot == LOOK_SLOT));
        let reload = mapper.handle(Input::KeyDown(R));
        assert_eq!(phases(&reload), [(RELOAD_SLOT, Phase::Down)]);
        assert!(mapper.finger_down[usize::from(STICK_SLOT)]);
        assert!(mapper.finger_down[usize::from(FIRE_SLOT)]);
    }

    #[test]
    fn losing_the_mouse_leaves_the_keyboard_alone() {
        let mut mapper = mapper();
        mapper.handle(Input::KeyDown(W));
        mapper.handle(Input::KeyDown(R));
        mapper.handle(Input::ButtonDown(MouseButton::Left));
        mapper.handle(Input::MouseMove { dx: 1.0, dy: 0.0 });

        let touches = mapper.handle(Input::DeviceLost(DeviceKind::Mouse));

        let lifted: Vec<u8> = touches.iter().map(|t| t.slot).collect();
        assert!(lifted.contains(&FIRE_SLOT) && lifted.contains(&LOOK_SLOT));
        assert!(!lifted.contains(&STICK_SLOT) && !lifted.contains(&RELOAD_SLOT));
        // The keyboard keeps working afterwards, with no reconnect step.
        assert_eq!(
            phases(&mapper.handle(Input::KeyUp(R))),
            [(RELOAD_SLOT, Phase::Up)]
        );
    }

    #[test]
    fn losing_the_keyboard_leaves_the_mouse_alone() {
        let mut mapper = mapper();
        mapper.handle(Input::KeyDown(W));
        mapper.handle(Input::ButtonDown(MouseButton::Left));

        let touches = mapper.handle(Input::DeviceLost(DeviceKind::Keyboard));

        assert_eq!(phases(&touches), [(STICK_SLOT, Phase::Up)]);
        assert_eq!(
            phases(&mapper.handle(Input::ButtonUp(MouseButton::Left))),
            [(FIRE_SLOT, Phase::Up)]
        );
    }

    #[test]
    fn a_reconnected_device_works_immediately() {
        let mut mapper = mapper();
        mapper.handle(Input::KeyDown(R));
        mapper.handle(Input::DeviceLost(DeviceKind::Keyboard));
        assert_eq!(
            phases(&mapper.handle(Input::KeyDown(R))),
            [(RELOAD_SLOT, Phase::Down)]
        );
    }

    #[test]
    fn losing_focus_lifts_every_finger() {
        let mut mapper = mapper();
        mapper.handle(Input::KeyDown(W));
        mapper.handle(Input::KeyDown(R));
        mapper.handle(Input::ButtonDown(MouseButton::Left));
        mapper.handle(Input::MouseMove { dx: 1.0, dy: 0.0 });
        mapper.handle(Input::FocusLost);
        assert_eq!(mapper.finger_down, [false; MAX_SLOTS]);
    }

    #[test]
    fn the_look_finger_lands_on_its_origin_then_drags() {
        let mut mapper = mapper();
        let touches = mapper.handle(Input::MouseMove { dx: 5.0, dy: 0.0 });
        assert_eq!(
            phases(&touches),
            [(LOOK_SLOT, Phase::Down), (LOOK_SLOT, Phase::Move)]
        );
        assert!(close(touches[1].at.x, 0.65));
    }

    #[test]
    fn the_look_finger_recentres_at_the_edge_of_its_range() {
        let mut mapper = mapper();
        let far = mapper.handle(Input::MouseMove { dx: 30.0, dy: 0.0 });
        assert_eq!(far.last().unwrap().phase, Phase::Up);
        let next = mapper.handle(Input::MouseMove { dx: 1.0, dy: 0.0 });
        assert_eq!(next[0].phase, Phase::Down);
        assert_eq!(next[0].at, Point { x: 0.6, y: 0.4 });
    }

    #[test]
    fn touches_never_leave_the_screen() {
        let mut mapper = mapper();
        let touches = mapper.handle(Input::MouseMove {
            dx: 1000.0,
            dy: -1000.0,
        });
        assert!(touches.iter().all(|touch| (0.0..=1.0).contains(&touch.at.x)
            && (0.0..=1.0).contains(&touch.at.y)));
    }

    #[test]
    fn the_toggle_lifts_everything_and_frees_the_mouse() {
        let mut mapper = mapper();
        mapper.handle(Input::KeyDown(W));
        mapper.handle(Input::ButtonDown(MouseButton::Left));
        mapper.handle(Input::ButtonUp(MouseButton::Left));
        let touches = mapper.handle(Input::KeyDown(ALT));
        assert_eq!(phases(&touches), [(STICK_SLOT, Phase::Up)]);
        assert!(mapper.is_pointer_mode());
        assert!(
            mapper
                .handle(Input::MouseMove { dx: 5.0, dy: 5.0 })
                .is_empty()
        );
        assert!(mapper.handle(Input::KeyDown(R)).is_empty());
    }

    #[test]
    fn pointer_mode_clicks_where_the_cursor_is() {
        let mut mapper = mapper();
        mapper.handle(Input::KeyDown(ALT));
        mapper.handle(Input::KeyUp(ALT));
        let target = Point { x: 0.3, y: 0.3 };
        assert!(mapper.handle(Input::CursorAt(target)).is_empty());
        let down = mapper.handle(Input::ButtonDown(MouseButton::Left));
        assert_eq!(down[0].at, target);
        assert_eq!(down[0].slot, POINTER_SLOT);
        let dragged = mapper.handle(Input::CursorAt(Point { x: 0.4, y: 0.3 }));
        assert_eq!(dragged[0].phase, Phase::Move);
        let up = mapper.handle(Input::ButtonUp(MouseButton::Left));
        assert_eq!(up[0].phase, Phase::Up);
    }

    #[test]
    fn toggling_back_restores_game_controls() {
        let mut mapper = mapper();
        for _ in 0..2 {
            mapper.handle(Input::KeyDown(ALT));
            mapper.handle(Input::KeyUp(ALT));
        }
        assert!(!mapper.is_pointer_mode());
        assert_eq!(
            phases(&mapper.handle(Input::KeyDown(R))),
            [(RELOAD_SLOT, Phase::Down)]
        );
    }

    #[test]
    fn every_down_has_exactly_one_up_after_a_messy_session() {
        let mut mapper = mapper();
        let inputs = [
            Input::KeyDown(W),
            Input::KeyDown(D),
            Input::ButtonDown(MouseButton::Left),
            Input::MouseMove { dx: 2.0, dy: 2.0 },
            Input::KeyDown(ALT),
            Input::KeyUp(ALT),
            Input::KeyUp(W),
            Input::KeyDown(ALT),
            Input::KeyUp(ALT),
            Input::KeyDown(R),
            Input::DeviceLost(DeviceKind::Mouse),
            Input::MouseMove { dx: 40.0, dy: 0.0 },
            Input::MouseMove { dx: 1.0, dy: 0.0 },
            Input::FocusLost,
        ];
        let mut down = [false; MAX_SLOTS + 1];
        for input in inputs {
            for touch in mapper.handle(input) {
                let slot = usize::from(touch.slot);
                match touch.phase {
                    Phase::Down => {
                        assert!(!down[slot], "double down on slot {slot}");
                        down[slot] = true;
                    }
                    Phase::Up => {
                        assert!(down[slot], "up without down on slot {slot}");
                        down[slot] = false;
                    }
                    Phase::Move => assert!(down[slot], "move without down on slot {slot}"),
                }
            }
        }
        assert_eq!(down, [false; MAX_SLOTS + 1], "a finger was left stuck");
    }
}
