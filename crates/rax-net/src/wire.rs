//! Packet formats. Parsers return `None` for anything malformed, because
//! every byte here comes from the untrusted guest.

use std::net::Ipv4Addr;

/// An Ethernet address.
pub type Mac = [u8; 6];

/// EtherType for IPv4.
pub const ETHER_IPV4: u16 = 0x0800;
/// EtherType for ARP.
pub const ETHER_ARP: u16 = 0x0806;
/// IP protocol number for ICMP.
pub const PROTO_ICMP: u8 = 1;
/// IP protocol number for TCP.
pub const PROTO_TCP: u8 = 6;
/// IP protocol number for UDP.
pub const PROTO_UDP: u8 = 17;

/// TCP FIN flag.
pub const TCP_FIN: u8 = 0x01;
/// TCP SYN flag.
pub const TCP_SYN: u8 = 0x02;
/// TCP RST flag.
pub const TCP_RST: u8 = 0x04;
/// TCP PSH flag.
pub const TCP_PSH: u8 = 0x08;
/// TCP ACK flag.
pub const TCP_ACK: u8 = 0x10;

const ETHERNET_LEN: usize = 14;
const IPV4_LEN: usize = 20;
const TCP_LEN: usize = 20;
const UDP_LEN: usize = 8;
/// Maximum TCP payload per segment.
pub const MSS: usize = 1460;

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*data.get(at)?, *data.get(at + 1)?]))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *data.get(at)?,
        *data.get(at + 1)?,
        *data.get(at + 2)?,
        *data.get(at + 3)?,
    ]))
}

fn ip_at(data: &[u8], at: usize) -> Option<Ipv4Addr> {
    u32_at(data, at).map(Ipv4Addr::from)
}

/// The Internet checksum over several byte runs.
#[must_use]
pub fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0_u32;
    let mut carry: Option<u8> = None;
    for part in parts {
        for &byte in *part {
            match carry.take() {
                Some(high) => sum += u32::from(u16::from_be_bytes([high, byte])),
                None => carry = Some(byte),
            }
        }
    }
    if let Some(high) = carry {
        sum += u32::from(u16::from_be_bytes([high, 0]));
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// A parsed Ethernet frame.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// EtherType.
    pub ether_type: u16,
    /// Sender address.
    pub source: Mac,
    /// Everything after the Ethernet header.
    pub payload: &'a [u8],
}

/// Parses an Ethernet frame.
#[must_use]
pub fn parse_frame(data: &[u8]) -> Option<Frame<'_>> {
    Some(Frame {
        ether_type: u16_at(data, 12)?,
        source: data.get(6..12)?.try_into().ok()?,
        payload: data.get(ETHERNET_LEN..)?,
    })
}

/// A parsed ARP request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArpRequest {
    /// Asker's address.
    pub sender_mac: Mac,
    /// Asker's IP.
    pub sender_ip: Ipv4Addr,
    /// The IP being looked up.
    pub target_ip: Ipv4Addr,
}

/// Parses an ARP request for IPv4 over Ethernet.
#[must_use]
pub fn parse_arp(data: &[u8]) -> Option<ArpRequest> {
    let valid = u16_at(data, 0)? == 1
        && u16_at(data, 2)? == ETHER_IPV4
        && *data.get(4)? == 6
        && *data.get(5)? == 4
        && u16_at(data, 6)? == 1;
    valid.then_some(())?;
    Some(ArpRequest {
        sender_mac: data.get(8..14)?.try_into().ok()?,
        sender_ip: ip_at(data, 14)?,
        target_ip: ip_at(data, 24)?,
    })
}

fn ethernet(dest: Mac, source: Mac, ether_type: u16, capacity: usize) -> Vec<u8> {
    let mut frame = Vec::with_capacity(ETHERNET_LEN + capacity);
    frame.extend_from_slice(&dest);
    frame.extend_from_slice(&source);
    frame.extend_from_slice(&ether_type.to_be_bytes());
    frame
}

/// Builds an ARP reply.
#[must_use]
pub fn build_arp_reply(request: &ArpRequest, our_mac: Mac) -> Vec<u8> {
    let mut frame = ethernet(request.sender_mac, our_mac, ETHER_ARP, 28);
    frame.extend_from_slice(&1_u16.to_be_bytes());
    frame.extend_from_slice(&ETHER_IPV4.to_be_bytes());
    frame.extend_from_slice(&[6, 4]);
    frame.extend_from_slice(&2_u16.to_be_bytes());
    frame.extend_from_slice(&our_mac);
    frame.extend_from_slice(&request.target_ip.octets());
    frame.extend_from_slice(&request.sender_mac);
    frame.extend_from_slice(&request.sender_ip.octets());
    frame
}

/// A parsed IPv4 packet.
#[derive(Debug, Clone, Copy)]
pub struct Ipv4<'a> {
    /// Protocol number.
    pub protocol: u8,
    /// Source address.
    pub source: Ipv4Addr,
    /// Destination address.
    pub dest: Ipv4Addr,
    /// Everything after the IP header.
    pub payload: &'a [u8],
}

/// Parses an IPv4 packet. Fragments are refused, because nothing the guest
/// sends through this path needs them.
#[must_use]
pub fn parse_ipv4(data: &[u8]) -> Option<Ipv4<'_>> {
    let first = *data.first()?;
    if first >> 4 != 4 {
        return None;
    }
    let header = usize::from(first & 0x0f) * 4;
    let total = usize::from(u16_at(data, 2)?);
    let fragment = u16_at(data, 6)?;
    if header < IPV4_LEN || total < header || fragment & 0x3fff != 0 {
        return None;
    }
    Some(Ipv4 {
        protocol: *data.get(9)?,
        source: ip_at(data, 12)?,
        dest: ip_at(data, 16)?,
        payload: data.get(header..total)?,
    })
}

/// Addressing for a frame sent to the guest.
#[derive(Debug, Clone, Copy)]
pub struct Route {
    /// The guest's Ethernet address.
    pub dest_mac: Mac,
    /// Our Ethernet address.
    pub source_mac: Mac,
    /// Source IP, usually the remote host's.
    pub source: Ipv4Addr,
    /// Destination IP.
    pub dest: Ipv4Addr,
}

fn ipv4_frame(route: &Route, protocol: u8, payload: &[u8]) -> Vec<u8> {
    let total = (IPV4_LEN + payload.len()) as u16;
    let mut header = [0_u8; IPV4_LEN];
    header[0] = 0x45;
    header[2..4].copy_from_slice(&total.to_be_bytes());
    header[6] = 0x40; // Don't fragment.
    header[8] = 64;
    header[9] = protocol;
    header[12..16].copy_from_slice(&route.source.octets());
    header[16..20].copy_from_slice(&route.dest.octets());
    let sum = checksum(&[&header]);
    header[10..12].copy_from_slice(&sum.to_be_bytes());

    let mut frame = ethernet(
        route.dest_mac,
        route.source_mac,
        ETHER_IPV4,
        IPV4_LEN + payload.len(),
    );
    frame.extend_from_slice(&header);
    frame.extend_from_slice(payload);
    frame
}

fn pseudo_header(route: &Route, protocol: u8, length: usize) -> [u8; 12] {
    let mut pseudo = [0_u8; 12];
    pseudo[0..4].copy_from_slice(&route.source.octets());
    pseudo[4..8].copy_from_slice(&route.dest.octets());
    pseudo[9] = protocol;
    pseudo[10..12].copy_from_slice(&(length as u16).to_be_bytes());
    pseudo
}

/// A parsed UDP datagram.
#[derive(Debug, Clone, Copy)]
pub struct Udp<'a> {
    /// Source port.
    pub source_port: u16,
    /// Destination port.
    pub dest_port: u16,
    /// Payload.
    pub payload: &'a [u8],
}

/// Parses a UDP datagram.
#[must_use]
pub fn parse_udp(data: &[u8]) -> Option<Udp<'_>> {
    let length = usize::from(u16_at(data, 4)?);
    Some(Udp {
        source_port: u16_at(data, 0)?,
        dest_port: u16_at(data, 2)?,
        payload: data.get(UDP_LEN..length)?,
    })
}

/// Builds a UDP frame for the guest.
#[must_use]
pub fn build_udp(route: &Route, source_port: u16, dest_port: u16, payload: &[u8]) -> Vec<u8> {
    let length = UDP_LEN + payload.len();
    let mut packet = Vec::with_capacity(length);
    packet.extend_from_slice(&source_port.to_be_bytes());
    packet.extend_from_slice(&dest_port.to_be_bytes());
    packet.extend_from_slice(&(length as u16).to_be_bytes());
    packet.extend_from_slice(&[0, 0]);
    packet.extend_from_slice(payload);
    let pseudo = pseudo_header(route, PROTO_UDP, length);
    let sum = match checksum(&[&pseudo, &packet]) {
        0 => 0xffff,
        sum => sum,
    };
    packet[6..8].copy_from_slice(&sum.to_be_bytes());
    ipv4_frame(route, PROTO_UDP, &packet)
}

/// A parsed TCP segment.
#[derive(Debug, Clone, Copy)]
pub struct Tcp<'a> {
    /// Source port.
    pub source_port: u16,
    /// Destination port.
    pub dest_port: u16,
    /// Sequence number.
    pub seq: u32,
    /// Acknowledgement number.
    pub ack: u32,
    /// Flag bits.
    pub flags: u8,
    /// Advertised window.
    pub window: u16,
    /// Payload.
    pub payload: &'a [u8],
}

/// Parses a TCP segment.
#[must_use]
pub fn parse_tcp(data: &[u8]) -> Option<Tcp<'_>> {
    let offset = usize::from(*data.get(12)? >> 4) * 4;
    if offset < TCP_LEN {
        return None;
    }
    Some(Tcp {
        source_port: u16_at(data, 0)?,
        dest_port: u16_at(data, 2)?,
        seq: u32_at(data, 4)?,
        ack: u32_at(data, 8)?,
        flags: *data.get(13)?,
        window: u16_at(data, 14)?,
        payload: data.get(offset..)?,
    })
}

/// A TCP segment to build.
#[derive(Debug, Clone, Copy)]
pub struct TcpOut<'a> {
    /// Source port.
    pub source_port: u16,
    /// Destination port.
    pub dest_port: u16,
    /// Sequence number.
    pub seq: u32,
    /// Acknowledgement number.
    pub ack: u32,
    /// Flag bits.
    pub flags: u8,
    /// Advertised window.
    pub window: u16,
    /// Payload.
    pub payload: &'a [u8],
}

/// Builds a TCP frame for the guest. A SYN carries the MSS option.
#[must_use]
pub fn build_tcp(route: &Route, segment: &TcpOut<'_>) -> Vec<u8> {
    let options: &[u8] = if segment.flags & TCP_SYN != 0 {
        &[2, 4, (MSS >> 8) as u8, MSS as u8]
    } else {
        &[]
    };
    let header_len = TCP_LEN + options.len();
    let length = header_len + segment.payload.len();
    let mut packet = Vec::with_capacity(length);
    packet.extend_from_slice(&segment.source_port.to_be_bytes());
    packet.extend_from_slice(&segment.dest_port.to_be_bytes());
    packet.extend_from_slice(&segment.seq.to_be_bytes());
    packet.extend_from_slice(&segment.ack.to_be_bytes());
    packet.push(((header_len / 4) as u8) << 4);
    packet.push(segment.flags);
    packet.extend_from_slice(&segment.window.to_be_bytes());
    packet.extend_from_slice(&[0, 0, 0, 0]);
    packet.extend_from_slice(options);
    packet.extend_from_slice(segment.payload);
    let pseudo = pseudo_header(route, PROTO_TCP, length);
    let sum = checksum(&[&pseudo, &packet]);
    packet[16..18].copy_from_slice(&sum.to_be_bytes());
    ipv4_frame(route, PROTO_TCP, &packet)
}

/// Builds an ICMP echo reply from an echo request's bytes.
#[must_use]
pub fn build_echo_reply(route: &Route, request: &[u8]) -> Option<Vec<u8>> {
    if request.len() < 8 || request[0] != 8 {
        return None;
    }
    let mut reply = request.to_vec();
    reply[0] = 0;
    reply[2..4].fill(0);
    let sum = checksum(&[&reply]);
    reply[2..4].copy_from_slice(&sum.to_be_bytes());
    Some(ipv4_frame(route, PROTO_ICMP, &reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTE: Route = Route {
        dest_mac: [1; 6],
        source_mac: [2; 6],
        source: Ipv4Addr::new(93, 184, 216, 34),
        dest: Ipv4Addr::new(10, 0, 2, 15),
    };

    #[test]
    fn checksum_matches_the_rfc_1071_example() {
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&[&data]), !0xddf2);
    }

    #[test]
    fn checksum_handles_odd_splits_across_parts() {
        assert_eq!(
            checksum(&[&[0x12], &[0x34, 0x56]]),
            checksum(&[&[0x12, 0x34, 0x56]])
        );
    }

    #[test]
    fn a_correct_packet_checksums_to_zero() {
        let frame = build_udp(&ROUTE, 53, 1000, b"hello");
        let header = &frame[14..34];
        assert_eq!(checksum(&[header]), 0);
    }

    #[test]
    fn udp_round_trips() {
        let frame = build_udp(&ROUTE, 53, 1000, b"hello");
        let parsed = parse_frame(&frame).unwrap();
        assert_eq!(parsed.ether_type, ETHER_IPV4);
        let ip = parse_ipv4(parsed.payload).unwrap();
        assert_eq!(ip.source, ROUTE.source);
        let udp = parse_udp(ip.payload).unwrap();
        assert_eq!((udp.source_port, udp.dest_port), (53, 1000));
        assert_eq!(udp.payload, b"hello");
        let pseudo = pseudo_header(&ROUTE, PROTO_UDP, ip.payload.len());
        assert_eq!(checksum(&[&pseudo, ip.payload]), 0);
    }

    #[test]
    fn tcp_round_trips_with_a_valid_checksum() {
        let frame = build_tcp(
            &ROUTE,
            &TcpOut {
                source_port: 80,
                dest_port: 4000,
                seq: 7,
                ack: 9,
                flags: TCP_ACK | TCP_PSH,
                window: 1000,
                payload: b"data",
            },
        );
        let ip = parse_ipv4(parse_frame(&frame).unwrap().payload).unwrap();
        let tcp = parse_tcp(ip.payload).unwrap();
        assert_eq!((tcp.seq, tcp.ack, tcp.window), (7, 9, 1000));
        assert_eq!(tcp.payload, b"data");
        let pseudo = pseudo_header(&ROUTE, PROTO_TCP, ip.payload.len());
        assert_eq!(checksum(&[&pseudo, ip.payload]), 0);
    }

    #[test]
    fn syn_segments_carry_the_mss() {
        let frame = build_tcp(
            &ROUTE,
            &TcpOut {
                source_port: 80,
                dest_port: 4000,
                seq: 0,
                ack: 1,
                flags: TCP_SYN | TCP_ACK,
                window: 1000,
                payload: &[],
            },
        );
        let ip = parse_ipv4(parse_frame(&frame).unwrap().payload).unwrap();
        assert_eq!(ip.payload[12] >> 4, 6, "header grew by one word");
        assert_eq!(&ip.payload[20..24], &[2, 4, 0x05, 0xb4]);
    }

    #[test]
    fn arp_reply_swaps_the_addresses() {
        let request = ArpRequest {
            sender_mac: [1; 6],
            sender_ip: Ipv4Addr::new(10, 0, 2, 15),
            target_ip: Ipv4Addr::new(10, 0, 2, 2),
        };
        let frame = build_arp_reply(&request, [2; 6]);
        assert_eq!(&frame[..6], &[1; 6]);
        assert_eq!(u16_at(&frame, 12), Some(ETHER_ARP));
        assert_eq!(u16_at(&frame, 20), Some(2), "reply opcode");
        assert_eq!(&frame[28..32], &[10, 0, 2, 2]);
    }

    #[test]
    fn echo_replies_keep_the_identifier_and_data() {
        let mut request = vec![8, 0, 0, 0, 0xab, 0xcd, 0, 1, 1, 2, 3];
        let sum = checksum(&[&request]);
        request[2..4].copy_from_slice(&sum.to_be_bytes());
        let frame = build_echo_reply(&ROUTE, &request).unwrap();
        let ip = parse_ipv4(parse_frame(&frame).unwrap().payload).unwrap();
        assert_eq!(ip.payload[0], 0);
        assert_eq!(&ip.payload[4..], &request[4..]);
        assert_eq!(checksum(&[ip.payload]), 0);
    }

    #[test]
    fn truncated_input_is_refused_everywhere() {
        assert!(parse_frame(&[0; 3]).is_none());
        assert!(parse_arp(&[0; 3]).is_none());
        assert!(parse_ipv4(&[0x45]).is_none());
        assert!(parse_udp(&[0; 3]).is_none());
        assert!(parse_tcp(&[0; 3]).is_none());
    }

    #[test]
    fn lengths_beyond_the_data_are_refused() {
        let mut packet = [0_u8; 28];
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&500_u16.to_be_bytes());
        assert!(parse_ipv4(&packet).is_none());
        let mut udp = [0_u8; 8];
        udp[4..6].copy_from_slice(&500_u16.to_be_bytes());
        assert!(parse_udp(&udp).is_none());
    }

    #[test]
    fn bad_header_lengths_are_refused() {
        let mut packet = [0_u8; 40];
        packet[0] = 0x41;
        assert!(parse_ipv4(&packet).is_none());
        let tcp = [0_u8; 20];
        assert!(parse_tcp(&tcp).is_none(), "data offset of zero");
    }

    #[test]
    fn fragments_are_refused() {
        let mut packet = [0_u8; 28];
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&28_u16.to_be_bytes());
        packet[6] = 0x20; // More fragments.
        assert!(parse_ipv4(&packet).is_none());
    }
}
