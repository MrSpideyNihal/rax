//! Connects the router to real sockets.
//!
//! One worker thread owns the router. Each TCP connection adds a reader and a
//! writer thread, because plain blocking sockets keep the code small and add
//! no dependencies, and a game holds only a handful of connections.

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs, UdpSocket,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::nat::{Backend, DnsQuery, Nat, TcpKey};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const UDP_IDLE: Duration = Duration::from_secs(120);
/// Bytes a connection may hold for the guest before its reader pauses.
const TCP_BACKLOG: usize = 2 * 1024 * 1024;
const BACKLOG_POLL: Duration = Duration::from_millis(2);
const READ_SIZE: usize = 64 * 1024;
const MAX_DATAGRAM: usize = 65_536;

#[derive(Debug)]
enum Message {
    Frame(Vec<u8>),
    Connected(TcpKey, Option<TcpStream>),
    TcpData(TcpKey, Vec<u8>),
    TcpClosed(TcpKey),
    Udp(u16, SocketAddrV4, Vec<u8>),
    UdpIdle(u16),
    Dns(DnsQuery, Vec<Ipv4Addr>),
}

struct Link {
    stream: TcpStream,
    writer: Sender<Vec<u8>>,
    backlog: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
}

struct Sockets {
    messages: Sender<Message>,
    links: HashMap<TcpKey, Link>,
    udp: HashMap<u16, UdpSocket>,
}

fn connect(key: TcpKey, messages: &Sender<Message>) {
    let result = TcpStream::connect_timeout(&SocketAddr::V4(key.remote), CONNECT_TIMEOUT);
    let stream = result.ok().inspect(|stream| {
        let _ = stream.set_nodelay(true);
    });
    let _ = messages.send(Message::Connected(key, stream));
}

fn read_loop(
    key: TcpKey,
    mut stream: TcpStream,
    messages: &Sender<Message>,
    backlog: &AtomicUsize,
    closed: &AtomicBool,
) {
    let mut buffer = vec![0_u8; READ_SIZE];
    loop {
        while backlog.load(Ordering::Relaxed) > TCP_BACKLOG {
            if closed.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(BACKLOG_POLL);
        }
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                backlog.fetch_add(count, Ordering::Relaxed);
                let data = buffer[..count].to_vec();
                if messages.send(Message::TcpData(key, data)).is_err() {
                    return;
                }
            }
        }
    }
    let _ = messages.send(Message::TcpClosed(key));
}

fn write_loop(mut stream: TcpStream, data: &Receiver<Vec<u8>>) {
    while let Ok(chunk) = data.recv() {
        if stream.write_all(&chunk).is_err() {
            return;
        }
    }
    let _ = stream.shutdown(Shutdown::Write);
}

impl Sockets {
    fn adopt(&mut self, key: TcpKey, stream: &TcpStream) -> Option<()> {
        let reader = stream.try_clone().ok()?;
        let writer_stream = stream.try_clone().ok()?;
        let (writer, data) = mpsc::channel();
        let backlog = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let messages = self.messages.clone();
        let (reader_backlog, reader_closed) = (Arc::clone(&backlog), Arc::clone(&closed));
        std::thread::spawn(move || {
            read_loop(key, reader, &messages, &reader_backlog, &reader_closed);
        });
        std::thread::spawn(move || write_loop(writer_stream, &data));
        self.links.insert(
            key,
            Link {
                stream: stream.try_clone().ok()?,
                writer,
                backlog,
                closed,
            },
        );
        Some(())
    }

    fn udp_socket(&mut self, guest_port: u16) -> Option<&UdpSocket> {
        if !self.udp.contains_key(&guest_port) {
            let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).ok()?;
            socket.set_read_timeout(Some(UDP_IDLE)).ok()?;
            let reader = socket.try_clone().ok()?;
            let messages = self.messages.clone();
            std::thread::spawn(move || udp_loop(guest_port, &reader, &messages));
            self.udp.insert(guest_port, socket);
        }
        self.udp.get(&guest_port)
    }
}

fn udp_loop(guest_port: u16, socket: &UdpSocket, messages: &Sender<Message>) {
    let mut buffer = vec![0_u8; MAX_DATAGRAM];
    loop {
        match socket.recv_from(&mut buffer) {
            Ok((count, SocketAddr::V4(remote))) => {
                let data = buffer[..count].to_vec();
                if messages
                    .send(Message::Udp(guest_port, remote, data))
                    .is_err()
                {
                    return;
                }
            }
            Ok(_) => {}
            // Windows reports a remote's closed port as an error on this
            // socket, which says nothing about later datagrams.
            Err(error) if error.kind() == ErrorKind::ConnectionReset => {}
            Err(_) => {
                let _ = messages.send(Message::UdpIdle(guest_port));
                return;
            }
        }
    }
}

fn lookup(name: &str) -> Vec<Ipv4Addr> {
    (name, 0_u16)
        .to_socket_addrs()
        .map(|addresses| {
            addresses
                .filter_map(|address| match address.ip() {
                    IpAddr::V4(ip) => Some(ip),
                    IpAddr::V6(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Backend for Sockets {
    fn tcp_connect(&mut self, key: TcpKey) {
        let messages = self.messages.clone();
        std::thread::spawn(move || connect(key, &messages));
    }

    fn tcp_send(&mut self, key: TcpKey, data: Vec<u8>) {
        if let Some(link) = self.links.get(&key) {
            let _ = link.writer.send(data);
        }
    }

    fn tcp_finish(&mut self, key: TcpKey) {
        if let Some(link) = self.links.get(&key) {
            let _ = link.stream.shutdown(Shutdown::Write);
        }
    }

    fn tcp_abort(&mut self, key: TcpKey) {
        if let Some(link) = self.links.remove(&key) {
            link.closed.store(true, Ordering::Relaxed);
            let _ = link.stream.shutdown(Shutdown::Both);
        }
    }

    fn tcp_consumed(&mut self, key: TcpKey, bytes: usize) {
        if let Some(link) = self.links.get(&key) {
            // Only this thread subtracts, and the reader only adds, so the
            // value cannot fall below what was just read.
            let held = link.backlog.load(Ordering::Relaxed);
            link.backlog.fetch_sub(bytes.min(held), Ordering::Relaxed);
        }
    }

    fn udp_send(&mut self, guest_port: u16, remote: SocketAddrV4, data: Vec<u8>) {
        if let Some(socket) = self.udp_socket(guest_port) {
            let _ = socket.send_to(&data, remote);
        }
    }

    fn dns_lookup(&mut self, query: DnsQuery) {
        let messages = self.messages.clone();
        std::thread::spawn(move || {
            let addresses = lookup(&query.name);
            let _ = messages.send(Message::Dns(query, addresses));
        });
    }
}

/// A running network: frames to and from the guest.
#[derive(Clone, Debug)]
pub struct Network {
    messages: Sender<Message>,
    /// Frames waiting for the guest.
    pub incoming: Arc<Mutex<VecDeque<Vec<u8>>>>,
    /// Set when `incoming` has gained frames.
    pub ready: Arc<AtomicBool>,
}

impl Network {
    /// Starts the router thread.
    #[must_use]
    pub fn start() -> Self {
        let (messages, inbox) = mpsc::channel();
        let incoming = Arc::new(Mutex::new(VecDeque::new()));
        let ready = Arc::new(AtomicBool::new(false));
        let sockets = Sockets {
            messages: messages.clone(),
            links: HashMap::new(),
            udp: HashMap::new(),
        };
        let (shared, flag) = (Arc::clone(&incoming), Arc::clone(&ready));
        std::thread::spawn(move || worker(Nat::new(sockets), &inbox, &shared, &flag));
        Self {
            messages,
            incoming,
            ready,
        }
    }

    /// Passes a frame from the guest to the router.
    pub fn send_frame(&self, frame: Vec<u8>) {
        let _ = self.messages.send(Message::Frame(frame));
    }
}

fn worker(
    mut nat: Nat<Sockets>,
    inbox: &Receiver<Message>,
    incoming: &Mutex<VecDeque<Vec<u8>>>,
    ready: &AtomicBool,
) {
    while let Ok(message) = inbox.recv() {
        match message {
            Message::Frame(frame) => nat.handle_frame(&frame),
            Message::Connected(key, stream) => {
                let adopted = stream
                    .as_ref()
                    .and_then(|stream| nat.backend_mut().adopt(key, stream));
                nat.tcp_connected(key, adopted.is_some());
            }
            Message::TcpData(key, data) => nat.tcp_received(key, &data),
            Message::TcpClosed(key) => nat.tcp_host_finished(key),
            Message::Udp(port, remote, data) => nat.udp_received(port, remote, &data),
            Message::UdpIdle(port) => {
                nat.backend_mut().udp.remove(&port);
            }
            Message::Dns(query, addresses) => nat.dns_answer(&query, &addresses),
        }
        let mut delivered = false;
        if let Ok(mut queue) = incoming.lock() {
            while let Some(frame) = nat.next_output() {
                queue.push_back(frame);
                delivered = true;
            }
        }
        if delivered {
            ready.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_network_ignores_frames() {
        let network = Network::start();
        network.send_frame(vec![0; 10]);
        assert!(network.incoming.lock().unwrap().is_empty());
    }

    #[test]
    fn numeric_names_resolve_without_a_network() {
        assert_eq!(lookup("127.0.0.1"), [Ipv4Addr::LOCALHOST]);
    }
}
