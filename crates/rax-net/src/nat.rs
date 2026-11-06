//! The router the guest talks to.
//!
//! This is a TCP proxy rather than a packet forwarder: the guest's connection
//! ends here, and a separate host socket carries the data. The link to the
//! guest cannot lose frames, because they wait in a queue until the guest has
//! buffers, so no retransmission is needed in that direction.

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};

use crate::wire::{
    self, ETHER_ARP, ETHER_IPV4, MSS, Mac, PROTO_ICMP, PROTO_TCP, PROTO_UDP, Route, TCP_ACK,
    TCP_FIN, TCP_PSH, TCP_RST, TCP_SYN, TcpOut,
};

/// The guest's Ethernet address.
pub const GUEST_MAC: Mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
/// The router's Ethernet address.
pub const ROUTER_MAC: Mac = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];
/// The address given to the guest.
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
/// The router's address.
pub const ROUTER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// The DNS server's address, answered here.
pub const DNS_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);

const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
const DNS_PORT: u16 = 53;
const DHCP_MAGIC: [u8; 4] = [99, 130, 83, 99];
const DHCP_DISCOVER: u8 = 1;
const DHCP_OFFER: u8 = 2;
const DHCP_REQUEST: u8 = 3;
const DHCP_ACK: u8 = 5;
const DHCP_OPTIONS_AT: usize = 240;
const LEASE_SECONDS: u32 = 86_400;

const OUR_WINDOW: u16 = 65_535;
const INITIAL_SEQ: u32 = 0x1000_0000;

/// One guest TCP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TcpKey {
    /// The guest's port.
    pub guest_port: u16,
    /// The remote end.
    pub remote: SocketAddrV4,
}

/// A DNS question waiting for the host to answer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsQuery {
    /// The guest's port.
    pub guest_port: u16,
    /// The raw question, kept to build the answer.
    pub packet: Vec<u8>,
    /// The name asked about.
    pub name: String,
    /// Whether it asked for an IPv4 address.
    pub wants_ipv4: bool,
}

/// What the router needs from the host.
pub trait Backend {
    /// Opens a connection. The result arrives at [`Nat::tcp_connected`].
    fn tcp_connect(&mut self, key: TcpKey);
    /// Sends data on an open connection.
    fn tcp_send(&mut self, key: TcpKey, data: Vec<u8>);
    /// Says the guest has finished sending.
    fn tcp_finish(&mut self, key: TcpKey);
    /// Abandons a connection.
    fn tcp_abort(&mut self, key: TcpKey);
    /// Tells the host how many bytes the guest has taken.
    fn tcp_consumed(&mut self, key: TcpKey, bytes: usize);
    /// Sends a datagram.
    fn udp_send(&mut self, guest_port: u16, remote: SocketAddrV4, data: Vec<u8>);
    /// Looks up a name. The result arrives at [`Nat::dns_answer`].
    fn dns_lookup(&mut self, query: DnsQuery);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TcpState {
    Connecting,
    SynSent,
    Established,
}

#[derive(Debug)]
struct Connection {
    state: TcpState,
    /// Next byte expected from the guest.
    receive_next: u32,
    /// Next byte to send to the guest.
    send_next: u32,
    /// Oldest byte the guest has not acknowledged.
    send_acked: u32,
    /// The guest's window.
    window: u16,
    /// Bytes from the host waiting for window space.
    pending: VecDeque<u8>,
    /// The host has finished sending.
    host_finished: bool,
    /// Our FIN has been sent.
    fin_sent: bool,
    /// The guest has finished sending.
    guest_finished: bool,
}

/// The router.
pub struct Nat<B> {
    backend: B,
    output: VecDeque<Vec<u8>>,
    connections: HashMap<TcpKey, Connection>,
    next_seq: u32,
}

fn is_local(address: Ipv4Addr) -> bool {
    address == ROUTER_IP || address == DNS_IP
}

fn route_from(source: Ipv4Addr) -> Route {
    Route {
        dest_mac: GUEST_MAC,
        source_mac: ROUTER_MAC,
        source,
        dest: GUEST_IP,
    }
}

/// Reads a DNS name from a question. Compression never appears in questions
/// from a stub resolver, so a pointer is treated as malformed.
fn dns_question(packet: &[u8]) -> Option<(String, u16)> {
    let mut at = 12;
    let mut labels = Vec::new();
    loop {
        let length = usize::from(*packet.get(at)?);
        at += 1;
        if length == 0 {
            break;
        }
        if length > 63 {
            return None;
        }
        labels.push(std::str::from_utf8(packet.get(at..at + length)?).ok()?);
        at += length;
    }
    let kind = u16::from_be_bytes([*packet.get(at)?, *packet.get(at + 1)?]);
    Some((labels.join("."), kind))
}

/// Builds the answer to a DNS question.
fn dns_reply(question: &[u8], addresses: &[Ipv4Addr]) -> Option<Vec<u8>> {
    let (_, _) = dns_question(question)?;
    let mut end = 12;
    while *question.get(end)? != 0 {
        end += 1 + usize::from(question[end]);
    }
    end += 5;
    let mut reply = question.get(..end)?.to_vec();
    reply[2] = 0x81;
    reply[3] = 0x80;
    reply[6..8].copy_from_slice(&(addresses.len() as u16).to_be_bytes());
    reply[8..12].fill(0);
    for address in addresses {
        reply.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        reply.extend_from_slice(&address.octets());
    }
    Some(reply)
}

/// Finds the message type in DHCP options.
fn dhcp_message_type(options: &[u8]) -> Option<u8> {
    let mut at = 0;
    loop {
        match *options.get(at)? {
            0 => at += 1,
            255 => return None,
            53 => return options.get(at + 2).copied(),
            _ => at += 2 + usize::from(*options.get(at + 1)?),
        }
    }
}

fn dhcp_reply(request: &[u8], kind: u8) -> Option<Vec<u8>> {
    let mut reply = request.get(..DHCP_OPTIONS_AT)?.to_vec();
    reply[0] = 2;
    reply[12..16].fill(0);
    reply[16..20].copy_from_slice(&GUEST_IP.octets());
    reply[20..24].copy_from_slice(&ROUTER_IP.octets());
    reply[24..28].fill(0);
    reply[236..240].copy_from_slice(&DHCP_MAGIC);
    reply.extend_from_slice(&[53, 1, kind]);
    reply.extend_from_slice(&[54, 4]);
    reply.extend_from_slice(&ROUTER_IP.octets());
    reply.extend_from_slice(&[51, 4]);
    reply.extend_from_slice(&LEASE_SECONDS.to_be_bytes());
    reply.extend_from_slice(&[1, 4]);
    reply.extend_from_slice(&NETMASK.octets());
    reply.extend_from_slice(&[3, 4]);
    reply.extend_from_slice(&ROUTER_IP.octets());
    reply.extend_from_slice(&[6, 4]);
    reply.extend_from_slice(&DNS_IP.octets());
    reply.push(255);
    Some(reply)
}

impl<B: Backend> Nat<B> {
    /// Creates a router.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            output: VecDeque::new(),
            connections: HashMap::new(),
            next_seq: INITIAL_SEQ,
        }
    }

    /// The backend, for delivering events.
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Takes the next frame for the guest.
    pub fn next_output(&mut self) -> Option<Vec<u8>> {
        self.output.pop_front()
    }

    /// Open TCP connections.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Handles a frame from the guest. Anything malformed is dropped.
    pub fn handle_frame(&mut self, data: &[u8]) {
        let Some(frame) = wire::parse_frame(data) else {
            return;
        };
        match frame.ether_type {
            ETHER_ARP => self.handle_arp(frame.payload),
            ETHER_IPV4 => self.handle_ipv4(frame.payload),
            _ => {}
        }
    }

    fn handle_arp(&mut self, data: &[u8]) {
        if let Some(request) = wire::parse_arp(data)
            && request.target_ip != GUEST_IP
            && request.sender_mac == GUEST_MAC
        {
            self.output
                .push_back(wire::build_arp_reply(&request, ROUTER_MAC));
        }
    }

    fn handle_ipv4(&mut self, data: &[u8]) {
        let Some(packet) = wire::parse_ipv4(data) else {
            return;
        };
        match packet.protocol {
            PROTO_UDP => self.handle_udp(&packet),
            PROTO_TCP => self.handle_tcp(&packet),
            PROTO_ICMP if is_local(packet.dest) => {
                if let Some(reply) =
                    wire::build_echo_reply(&route_from(packet.dest), packet.payload)
                {
                    self.output.push_back(reply);
                }
            }
            _ => {}
        }
    }

    fn handle_udp(&mut self, packet: &wire::Ipv4<'_>) {
        let Some(udp) = wire::parse_udp(packet.payload) else {
            return;
        };
        if udp.dest_port == DHCP_SERVER_PORT {
            self.handle_dhcp(udp.payload);
        } else if packet.dest == DNS_IP && udp.dest_port == DNS_PORT {
            self.handle_dns(udp.source_port, udp.payload);
        } else if !is_local(packet.dest) && !packet.dest.is_broadcast() {
            let remote = SocketAddrV4::new(packet.dest, udp.dest_port);
            self.backend
                .udp_send(udp.source_port, remote, udp.payload.to_vec());
        }
    }

    fn handle_dhcp(&mut self, data: &[u8]) {
        let kind = data
            .get(DHCP_OPTIONS_AT..)
            .filter(|_| data.get(236..240) == Some(DHCP_MAGIC.as_slice()))
            .and_then(dhcp_message_type);
        let response = match kind {
            Some(DHCP_DISCOVER) => DHCP_OFFER,
            Some(DHCP_REQUEST) => DHCP_ACK,
            _ => return,
        };
        if let Some(reply) = dhcp_reply(data, response) {
            self.output.push_back(wire::build_udp(
                &route_from(ROUTER_IP),
                DHCP_SERVER_PORT,
                DHCP_CLIENT_PORT,
                &reply,
            ));
        }
    }

    fn handle_dns(&mut self, guest_port: u16, data: &[u8]) {
        let Some((name, kind)) = dns_question(data) else {
            return;
        };
        let query = DnsQuery {
            guest_port,
            packet: data.to_vec(),
            name,
            wants_ipv4: kind == 1,
        };
        if query.wants_ipv4 {
            self.backend.dns_lookup(query);
        } else {
            // The guest network is IPv4 only, so other record types are empty.
            self.dns_answer(&query, &[]);
        }
    }

    /// Delivers the host's answer to a DNS question.
    pub fn dns_answer(&mut self, query: &DnsQuery, addresses: &[Ipv4Addr]) {
        if let Some(reply) = dns_reply(&query.packet, addresses) {
            self.output.push_back(wire::build_udp(
                &route_from(DNS_IP),
                DNS_PORT,
                query.guest_port,
                &reply,
            ));
        }
    }

    /// Delivers a datagram from the host.
    pub fn udp_received(&mut self, guest_port: u16, remote: SocketAddrV4, data: &[u8]) {
        self.output.push_back(wire::build_udp(
            &route_from(*remote.ip()),
            remote.port(),
            guest_port,
            data,
        ));
    }

    fn send_segment(&mut self, key: TcpKey, seq: u32, ack: u32, flags: u8, payload: &[u8]) {
        self.output.push_back(wire::build_tcp(
            &route_from(*key.remote.ip()),
            &TcpOut {
                source_port: key.remote.port(),
                dest_port: key.guest_port,
                seq,
                ack,
                flags,
                window: OUR_WINDOW,
                payload,
            },
        ));
    }

    fn handle_tcp(&mut self, packet: &wire::Ipv4<'_>) {
        let Some(segment) = wire::parse_tcp(packet.payload) else {
            return;
        };
        let key = TcpKey {
            guest_port: segment.source_port,
            remote: SocketAddrV4::new(packet.dest, segment.dest_port),
        };
        if !self.connections.contains_key(&key) {
            if segment.flags & TCP_SYN != 0 && segment.flags & TCP_ACK == 0 {
                self.open_connection(key, &segment);
            } else if segment.flags & TCP_RST == 0 {
                self.send_segment(
                    key,
                    segment.ack,
                    segment.seq.wrapping_add(1),
                    TCP_RST | TCP_ACK,
                    &[],
                );
            }
            return;
        }
        if segment.flags & TCP_RST != 0 {
            self.connections.remove(&key);
            self.backend.tcp_abort(key);
            return;
        }
        self.update_connection(key, &segment);
    }

    fn open_connection(&mut self, key: TcpKey, segment: &wire::Tcp<'_>) {
        if is_local(*key.remote.ip()) {
            self.send_segment(key, 0, segment.seq.wrapping_add(1), TCP_RST | TCP_ACK, &[]);
            return;
        }
        let start = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(0x10_0000);
        self.connections.insert(
            key,
            Connection {
                state: TcpState::Connecting,
                receive_next: segment.seq.wrapping_add(1),
                send_next: start,
                send_acked: start,
                window: segment.window,
                pending: VecDeque::new(),
                host_finished: false,
                fin_sent: false,
                guest_finished: false,
            },
        );
        self.backend.tcp_connect(key);
    }

    /// Delivers the result of [`Backend::tcp_connect`].
    pub fn tcp_connected(&mut self, key: TcpKey, success: bool) {
        let Some(connection) = self.connections.get_mut(&key) else {
            if success {
                self.backend.tcp_abort(key);
            }
            return;
        };
        let (seq, ack) = (connection.send_next, connection.receive_next);
        if success {
            connection.state = TcpState::SynSent;
            connection.send_next = seq.wrapping_add(1);
            self.send_segment(key, seq, ack, TCP_SYN | TCP_ACK, &[]);
        } else {
            self.connections.remove(&key);
            self.send_segment(key, 0, ack, TCP_RST | TCP_ACK, &[]);
        }
    }

    fn update_connection(&mut self, key: TcpKey, segment: &wire::Tcp<'_>) {
        let Some(connection) = self.connections.get_mut(&key) else {
            return;
        };
        if connection.state == TcpState::Connecting {
            return;
        }
        let mut consumed = 0;
        if segment.flags & TCP_ACK != 0 {
            if connection.state == TcpState::SynSent {
                connection.state = TcpState::Established;
                connection.send_acked = connection.send_acked.wrapping_add(1);
            }
            let advance = segment.ack.wrapping_sub(connection.send_acked);
            let in_flight = connection.send_next.wrapping_sub(connection.send_acked);
            if advance <= in_flight {
                connection.send_acked = segment.ack;
                consumed = advance as usize;
            }
            connection.window = segment.window;
        }

        let mut reply = false;
        if !segment.payload.is_empty() || segment.flags & TCP_FIN != 0 {
            reply = true;
        }
        let in_order = segment.seq == connection.receive_next;
        let fresh_data = in_order && !segment.payload.is_empty();
        if fresh_data {
            connection.receive_next = connection
                .receive_next
                .wrapping_add(segment.payload.len() as u32);
        }
        let finished = in_order && segment.flags & TCP_FIN != 0;
        if finished {
            connection.receive_next = connection.receive_next.wrapping_add(1);
            connection.guest_finished = true;
        }
        let (seq, ack) = (connection.send_next, connection.receive_next);

        if consumed > 0 {
            self.backend.tcp_consumed(key, consumed);
        }
        if fresh_data {
            self.backend.tcp_send(key, segment.payload.to_vec());
        }
        if finished {
            self.backend.tcp_finish(key);
        }
        if reply {
            self.send_segment(key, seq, ack, TCP_ACK, &[]);
        }
        self.pump(key);
    }

    /// Delivers data from the host.
    pub fn tcp_received(&mut self, key: TcpKey, data: &[u8]) {
        if let Some(connection) = self.connections.get_mut(&key) {
            connection.pending.extend(data);
            self.pump(key);
        }
    }

    /// Says the host has finished sending.
    pub fn tcp_host_finished(&mut self, key: TcpKey) {
        if let Some(connection) = self.connections.get_mut(&key) {
            connection.host_finished = true;
            self.pump(key);
        }
    }

    /// Sends what the guest's window allows, then any FIN.
    fn pump(&mut self, key: TcpKey) {
        let mut chunk = [0_u8; MSS];
        loop {
            let Some(connection) = self.connections.get_mut(&key) else {
                return;
            };
            if connection.state != TcpState::Established {
                return;
            }
            let in_flight = connection.send_next.wrapping_sub(connection.send_acked) as usize;
            let room = usize::from(connection.window).saturating_sub(in_flight);
            let length = connection.pending.len().min(MSS).min(room);
            if length == 0 {
                break;
            }
            for slot in chunk.iter_mut().take(length) {
                *slot = connection.pending.pop_front().unwrap_or(0);
            }
            let (seq, ack) = (connection.send_next, connection.receive_next);
            connection.send_next = seq.wrapping_add(length as u32);
            self.send_segment(key, seq, ack, TCP_ACK | TCP_PSH, &chunk[..length]);
        }
        let Some(connection) = self.connections.get_mut(&key) else {
            return;
        };
        if connection.host_finished && connection.pending.is_empty() && !connection.fin_sent {
            connection.fin_sent = true;
            let (seq, ack) = (connection.send_next, connection.receive_next);
            connection.send_next = seq.wrapping_add(1);
            self.send_segment(key, seq, ack, TCP_FIN | TCP_ACK, &[]);
        }
        let Some(connection) = self.connections.get(&key) else {
            return;
        };
        let everything_acked = connection.send_acked == connection.send_next;
        if connection.fin_sent && connection.guest_finished && everything_acked {
            self.connections.remove(&key);
            self.backend.tcp_abort(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Tcp, Udp};

    #[derive(Debug, Default)]
    struct Fake {
        connects: Vec<TcpKey>,
        sent: Vec<u8>,
        finished: u32,
        aborted: u32,
        consumed: usize,
        udp: Vec<(u16, SocketAddrV4, Vec<u8>)>,
        lookups: Vec<DnsQuery>,
    }

    impl Backend for Fake {
        fn tcp_connect(&mut self, key: TcpKey) {
            self.connects.push(key);
        }
        fn tcp_send(&mut self, _key: TcpKey, data: Vec<u8>) {
            self.sent.extend(data);
        }
        fn tcp_finish(&mut self, _key: TcpKey) {
            self.finished += 1;
        }
        fn tcp_abort(&mut self, _key: TcpKey) {
            self.aborted += 1;
        }
        fn tcp_consumed(&mut self, _key: TcpKey, bytes: usize) {
            self.consumed += bytes;
        }
        fn udp_send(&mut self, port: u16, remote: SocketAddrV4, data: Vec<u8>) {
            self.udp.push((port, remote, data));
        }
        fn dns_lookup(&mut self, query: DnsQuery) {
            self.lookups.push(query);
        }
    }

    const REMOTE: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);
    const KEY: TcpKey = TcpKey {
        guest_port: 4000,
        remote: SocketAddrV4::new(REMOTE, 80),
    };

    fn guest_route(dest: Ipv4Addr) -> Route {
        Route {
            dest_mac: ROUTER_MAC,
            source_mac: GUEST_MAC,
            source: GUEST_IP,
            dest,
        }
    }

    fn guest_tcp(seq: u32, ack: u32, flags: u8, window: u16, payload: &[u8]) -> Vec<u8> {
        wire::build_tcp(
            &guest_route(REMOTE),
            &TcpOut {
                source_port: 4000,
                dest_port: 80,
                seq,
                ack,
                flags,
                window,
                payload,
            },
        )
    }

    fn output(nat: &mut Nat<Fake>) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| nat.next_output()).collect()
    }

    fn tcp_of(frame: &[u8]) -> Tcp<'_> {
        let ip = wire::parse_ipv4(wire::parse_frame(frame).unwrap().payload).unwrap();
        wire::parse_tcp(ip.payload).unwrap()
    }

    fn udp_of(frame: &[u8]) -> Udp<'_> {
        let ip = wire::parse_ipv4(wire::parse_frame(frame).unwrap().payload).unwrap();
        wire::parse_udp(ip.payload).unwrap()
    }

    /// Runs a handshake, returning the next sequence number from each side.
    fn established(nat: &mut Nat<Fake>, window: u16) -> (u32, u32) {
        nat.handle_frame(&guest_tcp(100, 0, TCP_SYN, window, &[]));
        nat.tcp_connected(KEY, true);
        let frames = output(nat);
        let syn_ack = tcp_of(&frames[0]);
        let ours = syn_ack.seq.wrapping_add(1);
        nat.handle_frame(&guest_tcp(101, ours, TCP_ACK, window, &[]));
        (101, ours)
    }

    fn dns_query(name: &str, kind: u16) -> Vec<u8> {
        let mut packet = vec![0xab, 0xcd, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            packet.push(label.len() as u8);
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0);
        packet.extend_from_slice(&kind.to_be_bytes());
        packet.extend_from_slice(&[0, 1]);
        packet
    }

    fn dhcp_packet(kind: u8) -> Vec<u8> {
        let mut packet = vec![0_u8; DHCP_OPTIONS_AT];
        packet[0] = 1;
        packet[4..8].copy_from_slice(&[1, 2, 3, 4]);
        packet[236..240].copy_from_slice(&DHCP_MAGIC);
        packet.extend_from_slice(&[53, 1, kind, 255]);
        packet
    }

    fn guest_udp(dest: Ipv4Addr, port: u16, payload: &[u8]) -> Vec<u8> {
        wire::build_udp(&guest_route(dest), 5000, port, payload)
    }

    #[test]
    fn arp_for_the_router_is_answered() {
        let mut nat = Nat::new(Fake::default());
        let mut request = vec![0_u8; 28];
        request[..8].copy_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
        request[8..14].copy_from_slice(&GUEST_MAC);
        request[14..18].copy_from_slice(&GUEST_IP.octets());
        request[24..28].copy_from_slice(&ROUTER_IP.octets());
        let mut frame = vec![0xff; 6];
        frame.extend_from_slice(&GUEST_MAC);
        frame.extend_from_slice(&ETHER_ARP.to_be_bytes());
        frame.extend_from_slice(&request);
        nat.handle_frame(&frame);
        let frames = output(&mut nat);
        assert_eq!(frames.len(), 1);
        assert_eq!(&frames[0][22..28], &ROUTER_MAC);
    }

    #[test]
    fn dhcp_offers_then_acknowledges() {
        for (request, expected) in [(DHCP_DISCOVER, DHCP_OFFER), (DHCP_REQUEST, DHCP_ACK)] {
            let mut nat = Nat::new(Fake::default());
            nat.handle_frame(&guest_udp(
                Ipv4Addr::BROADCAST,
                DHCP_SERVER_PORT,
                &dhcp_packet(request),
            ));
            let frames = output(&mut nat);
            let udp = udp_of(&frames[0]);
            assert_eq!(udp.dest_port, DHCP_CLIENT_PORT);
            assert_eq!(&udp.payload[16..20], &GUEST_IP.octets());
            assert_eq!(&udp.payload[4..8], &[1, 2, 3, 4], "transaction id");
            assert_eq!(
                dhcp_message_type(&udp.payload[DHCP_OPTIONS_AT..]),
                Some(expected)
            );
        }
    }

    #[test]
    fn dhcp_without_the_magic_cookie_is_ignored() {
        let mut nat = Nat::new(Fake::default());
        let mut packet = dhcp_packet(DHCP_DISCOVER);
        packet[236] = 0;
        nat.handle_frame(&guest_udp(Ipv4Addr::BROADCAST, DHCP_SERVER_PORT, &packet));
        assert!(output(&mut nat).is_empty());
    }

    #[test]
    fn ipv4_names_go_to_the_host_resolver() {
        let mut nat = Nat::new(Fake::default());
        nat.handle_frame(&guest_udp(DNS_IP, 53, &dns_query("example.com", 1)));
        assert_eq!(nat.backend.lookups[0].name, "example.com");
    }

    #[test]
    fn dns_answers_carry_the_addresses_and_id() {
        let mut nat = Nat::new(Fake::default());
        nat.handle_frame(&guest_udp(DNS_IP, 53, &dns_query("a.b", 1)));
        let query = nat.backend.lookups.remove(0);
        nat.dns_answer(&query, &[REMOTE]);
        let frames = output(&mut nat);
        let udp = udp_of(&frames[0]);
        assert_eq!(udp.source_port, 53);
        assert_eq!(&udp.payload[..2], &[0xab, 0xcd]);
        assert_eq!(&udp.payload[6..8], &[0, 1], "one answer");
        assert_eq!(&udp.payload[udp.payload.len() - 4..], &REMOTE.octets());
    }

    #[test]
    fn ipv6_questions_get_an_empty_answer_at_once() {
        let mut nat = Nat::new(Fake::default());
        nat.handle_frame(&guest_udp(DNS_IP, 53, &dns_query("a.b", 28)));
        assert!(nat.backend.lookups.is_empty());
        let frames = output(&mut nat);
        assert_eq!(&udp_of(&frames[0]).payload[6..8], &[0, 0]);
    }

    #[test]
    fn malformed_dns_is_dropped() {
        let mut nat = Nat::new(Fake::default());
        nat.handle_frame(&guest_udp(DNS_IP, 53, &[0; 5]));
        let mut pointer = dns_query("a.b", 1);
        pointer[12] = 0xc0;
        nat.handle_frame(&guest_udp(DNS_IP, 53, &pointer));
        assert!(nat.backend.lookups.is_empty());
        assert!(output(&mut nat).is_empty());
    }

    #[test]
    fn udp_passes_both_ways() {
        let mut nat = Nat::new(Fake::default());
        nat.handle_frame(&guest_udp(REMOTE, 9000, b"ping"));
        let (port, remote, data) = nat.backend.udp.remove(0);
        assert_eq!(
            (port, remote.port(), data.as_slice()),
            (5000, 9000, b"ping".as_slice())
        );
        nat.udp_received(port, remote, b"pong");
        let frames = output(&mut nat);
        assert_eq!(udp_of(&frames[0]).payload, b"pong");
    }

    #[test]
    fn a_connection_opens_only_after_the_host_connects() {
        let mut nat = Nat::new(Fake::default());
        nat.handle_frame(&guest_tcp(100, 0, TCP_SYN, 1000, &[]));
        assert_eq!(nat.backend.connects, [KEY]);
        assert!(output(&mut nat).is_empty(), "no SYN-ACK until connected");
        nat.tcp_connected(KEY, true);
        let frames = output(&mut nat);
        let syn_ack = tcp_of(&frames[0]);
        assert_eq!(syn_ack.flags, TCP_SYN | TCP_ACK);
        assert_eq!(syn_ack.ack, 101);
    }

    #[test]
    fn a_failed_connection_resets_the_guest() {
        let mut nat = Nat::new(Fake::default());
        nat.handle_frame(&guest_tcp(100, 0, TCP_SYN, 1000, &[]));
        nat.tcp_connected(KEY, false);
        let frames = output(&mut nat);
        assert_ne!(tcp_of(&frames[0]).flags & TCP_RST, 0);
        assert_eq!(nat.connection_count(), 0);
    }

    #[test]
    fn a_host_connection_for_a_dead_session_is_abandoned() {
        let mut nat = Nat::new(Fake::default());
        nat.tcp_connected(KEY, true);
        assert_eq!(nat.backend.aborted, 1);
    }

    #[test]
    fn guest_data_reaches_the_host_and_is_acknowledged() {
        let mut nat = Nat::new(Fake::default());
        let (seq, ack) = established(&mut nat, 60000);
        nat.handle_frame(&guest_tcp(seq, ack, TCP_ACK | TCP_PSH, 60000, b"GET /"));
        assert_eq!(nat.backend.sent, b"GET /");
        let frames = output(&mut nat);
        assert_eq!(tcp_of(&frames[0]).ack, seq + 5);
    }

    #[test]
    fn a_retransmitted_segment_is_not_sent_twice() {
        let mut nat = Nat::new(Fake::default());
        let (seq, ack) = established(&mut nat, 60000);
        for _ in 0..2 {
            nat.handle_frame(&guest_tcp(seq, ack, TCP_ACK, 60000, b"once"));
        }
        assert_eq!(nat.backend.sent, b"once");
        let frames = output(&mut nat);
        assert_eq!(tcp_of(&frames[1]).ack, seq + 4, "it is acknowledged again");
    }

    #[test]
    fn host_data_splits_into_segments() {
        let mut nat = Nat::new(Fake::default());
        established(&mut nat, 60000);
        nat.tcp_received(KEY, &vec![7; MSS + 10]);
        let frames = output(&mut nat);
        assert_eq!(frames.len(), 2);
        assert_eq!(tcp_of(&frames[0]).payload.len(), MSS);
        assert_eq!(tcp_of(&frames[1]).payload.len(), 10);
        assert_eq!(tcp_of(&frames[1]).seq, tcp_of(&frames[0]).seq + MSS as u32);
    }

    #[test]
    fn sending_stops_at_the_guest_window_and_resumes_on_ack() {
        let mut nat = Nat::new(Fake::default());
        let (seq, ours) = established(&mut nat, 2000);
        nat.tcp_received(KEY, &vec![1; 5000]);
        let frames = output(&mut nat);
        let sent: usize = frames.iter().map(|f| tcp_of(f).payload.len()).sum();
        assert_eq!(sent, 2000);
        nat.handle_frame(&guest_tcp(seq, ours + 2000, TCP_ACK, 2000, &[]));
        let frames = output(&mut nat);
        let sent: usize = frames.iter().map(|f| tcp_of(f).payload.len()).sum();
        assert_eq!(sent, 2000);
        assert_eq!(nat.backend.consumed, 2000);
    }

    #[test]
    fn an_impossible_ack_is_ignored() {
        let mut nat = Nat::new(Fake::default());
        let (seq, ours) = established(&mut nat, 2000);
        nat.handle_frame(&guest_tcp(seq, ours + 999_999, TCP_ACK, 2000, &[]));
        assert_eq!(nat.backend.consumed, 0);
    }

    #[test]
    fn closing_from_the_host_finishes_cleanly() {
        let mut nat = Nat::new(Fake::default());
        let (seq, ours) = established(&mut nat, 60000);
        nat.tcp_received(KEY, b"bye");
        nat.tcp_host_finished(KEY);
        let frames = output(&mut nat);
        assert_ne!(tcp_of(frames.last().unwrap()).flags & TCP_FIN, 0);
        nat.handle_frame(&guest_tcp(seq, ours + 4, TCP_ACK | TCP_FIN, 60000, &[]));
        assert_eq!(nat.backend.finished, 1);
        assert_eq!(nat.connection_count(), 0);
    }

    #[test]
    fn fin_waits_for_queued_data() {
        let mut nat = Nat::new(Fake::default());
        established(&mut nat, 100);
        nat.tcp_received(KEY, &[1; 300]);
        nat.tcp_host_finished(KEY);
        let frames = output(&mut nat);
        assert!(frames.iter().all(|f| tcp_of(f).flags & TCP_FIN == 0));
    }

    #[test]
    fn a_guest_reset_abandons_the_connection() {
        let mut nat = Nat::new(Fake::default());
        let (seq, ours) = established(&mut nat, 1000);
        nat.handle_frame(&guest_tcp(seq, ours, TCP_RST, 0, &[]));
        assert_eq!(nat.connection_count(), 0);
        assert_eq!(nat.backend.aborted, 1);
    }

    #[test]
    fn segments_for_unknown_connections_are_reset() {
        let mut nat = Nat::new(Fake::default());
        nat.handle_frame(&guest_tcp(5, 9, TCP_ACK, 1000, b"x"));
        let frames = output(&mut nat);
        assert_ne!(tcp_of(&frames[0]).flags & TCP_RST, 0);
    }

    #[test]
    fn the_router_itself_refuses_connections() {
        let mut nat = Nat::new(Fake::default());
        let frame = wire::build_tcp(
            &guest_route(ROUTER_IP),
            &TcpOut {
                source_port: 4000,
                dest_port: 80,
                seq: 1,
                ack: 0,
                flags: TCP_SYN,
                window: 1000,
                payload: &[],
            },
        );
        nat.handle_frame(&frame);
        assert!(nat.backend.connects.is_empty());
        assert_ne!(tcp_of(&output(&mut nat)[0]).flags & TCP_RST, 0);
    }

    #[test]
    fn garbage_frames_do_nothing() {
        let mut nat = Nat::new(Fake::default());
        nat.handle_frame(&[]);
        nat.handle_frame(&[0xff; 64]);
        assert!(output(&mut nat).is_empty());
    }
}
