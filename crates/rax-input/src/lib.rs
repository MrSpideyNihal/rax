//! Key mapping: turns keyboard and mouse input into touch events.
//!
//! Games with no keyboard support only accept touches, so a profile places
//! virtual fingers on the screen. This crate is pure logic with no OS or
//! guest dependency.
//!
//! Three rules shape the design:
//!
//! - **Devices are independent.** Keyboard and mouse state are tracked apart,
//!   so one device leaving never disturbs the other.
//! - **No key sticks.** Losing focus or a device releases exactly what is
//!   held, and every press has one matching release.
//! - **No macros.** The mapper has no clock, so it cannot time a sequence,
//!   repeat a press, or fire by itself. One physical action produces one
//!   direct touch action. See `docs/ADR/0007-game-goals-and-input.md`.

#![forbid(unsafe_code)]

mod key;
mod mapper;
mod profile;

pub use key::{Key, MouseButton};
pub use mapper::{DeviceKind, Input, Mapper, Phase, Touch};
pub use profile::{Binding, MAX_SLOTS, Point, Profile, ProfileError};
