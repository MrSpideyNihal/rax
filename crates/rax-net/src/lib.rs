//! Guest networking without drivers or administrator rights.
//!
//! The guest sees an ordinary Ethernet segment with a router at 10.0.2.2.
//! Nothing on the host sees Ethernet: [`nat::Nat`] answers ARP and DHCP
//! itself, and turns the guest's TCP and UDP traffic into normal Windows
//! sockets, so it behaves like any other application's traffic.
//!
//! `nat` and `wire` are pure logic, tested without a network. Only `host`
//! opens sockets.

#![forbid(unsafe_code)]

pub mod host;
pub mod nat;
pub mod wire;
