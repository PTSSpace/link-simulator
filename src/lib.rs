/// Link Simulator
///
/// Copyright (C) 2019 PTScientists GmbH
///
/// 17.04.2019    Eric Reinthal

extern crate rand;

use mio::{Events, Poll, PollOpt, Ready, Token};
use rand::Rng;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time;

#[macro_use]
mod error;
use error::err_msg;
use error::LinkResult;

mod network_fifo;
use network_fifo::NetworkFIFO;

/// Represents a link between two endpoints
pub struct Link {
    name: String,
    route: Route,
    udp_fifo: Arc<Mutex<NetworkFIFO>>,
    bit_error_rate: f64,
    delay_ms: u32,
    max_bandwidth: f64,
    statistic: Arc<Mutex<LinkStatistic>>,
    discard_bitflip_packet: bool,
}

impl Link {
    /// Creates a link between two UDP endpoints with the following features:
    /// - name for identification
    /// - rate control, exceeding data will be dropped
    /// - adds constant delay of specified amount
    /// - mutates incoming data stream based on a constant bit error rate
    /// - keeps track of link performance in a statistic
    pub fn new(
        name: String,
        address_in: &str,
        address_out: &str,
        bit_error_rate: f64,
        delay_ms: u32,
        max_bandwidth: f64,
        statistic: Arc<Mutex<LinkStatistic>>,
        discard_bitflip_packet: bool,
    ) -> LinkResult<Link> {
        if bit_error_rate < 0.0 {
            return link_error!(
                "({}) Bit error rate must be non-negative, was {}",
                name,
                bit_error_rate
            );
        }
        if max_bandwidth <= 0.0 {
            return link_error!(
                "({}) Maximum bandwidth must be positive, was {}",
                name,
                max_bandwidth
            );
        }
        let route = match Route::new(address_in, address_out) {
            Ok(x) => x,
            Err(e) => return link_error!("({}) {}", name, e),
        };
        // see reasoning for buffer size in NetworkFIFO doc string
        let buffer_size = 1.1 * max_bandwidth * (delay_ms as f64) / 8000.0;
        let udp_fifo = Arc::new(Mutex::new(NetworkFIFO::new(buffer_size as usize)));
        match name.len() {
            0 => link_error!("Link name must be at least one character, was '{}'", name),
            1...14 => Ok(Link {
                name,
                route,
                udp_fifo,
                bit_error_rate,
                delay_ms,
                max_bandwidth,
                statistic,
                discard_bitflip_packet,
            }),
            _ => link_error!("Link name too long (10 char max), was '{}'", name),
        }
    }

    // getter methods
	
    pub fn name(&self) -> &str {
        &self.name.as_ref()
    }

    pub fn route(&self) -> &Route {
        &self.route
    }

    pub fn bit_error_rate(&self) -> f64 {
        self.bit_error_rate
    }

    pub fn delay_ms(&self) -> u32 {
        self.delay_ms
    }

    pub fn max_bandwidth(&self) -> f64 {
        self.max_bandwidth
    }
}

/// Describes data sent over a particular link
/// all units in [bytes] or [bytes/s]
#[derive(Default, Clone, Copy)]
pub struct LinkStatistic {
    pub received_packets: u64,
    pub received_bytes: u64,
    pub forwarded_packets: u64,
    pub forwarded_bytes: u64,
    pub dropped_packets: u64,
    pub dropped_bytes: u64,
    pub forwarding_rate: u32,
    pub buffer_occupancy: f32,
    pub bit_flips: u64,
    pub discarded_packets: u64,
}

/// converts large amount of bytes into string of KB, MB and GB
/// note: does only print the order (K,M,G), not the 'B'
/// thus it can by used for absolute data and data rates
pub fn bytes_to_readable(x: u64) -> String {
    match x {
        x @ 0...999 => format!("{} ", x),
        x @ 1000...999999 => format!("{:.1} K", x as f32 / 1000.0),
        x @ 1000000...999999999 => format!("{:.1} M", x as f32 / 1000000.0),
        x => format!("{:.1} G", x as f32 / 1000000000.0),
    }
}

impl fmt::Display for LinkStatistic {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "recv: {}B ({}), drop: {}B ({}), forw: {}B ({}), rate: {}bps, buffer: {:.1}%, flips: {}, discards: {}",
            bytes_to_readable(self.received_bytes),
            self.received_packets,
            bytes_to_readable(self.dropped_bytes),
            self.dropped_packets,
            bytes_to_readable(self.forwarded_bytes),
            self.forwarded_packets,
            bytes_to_readable(self.forwarding_rate as u64 *8), // rate in bits / second
            self.buffer_occupancy*100.0, // % output
            self.bit_flips,
            self.discarded_packets,
        )
    }
}

/// Updates the network FIFO for the receiving thread
/// i.e. updates the read and write index, the buffer pointer and the full-flag
fn update_fifo(
    fifo_mutex: &Arc<Mutex<NetworkFIFO>>,
    start_idx: usize,
    mut free_idx: usize,
    num: usize,
    buffer_size: usize,
) -> (bool, usize) {
    let mut is_full = false;
    free_idx += num;
    if start_idx == free_idx {
        is_full = true;
    } else if start_idx > free_idx && start_idx - free_idx < network_fifo::MAX_FRAME_SIZE {
        is_full = true;
    } else if free_idx + network_fifo::MAX_FRAME_SIZE > buffer_size {
        free_idx = 0;
        if start_idx < network_fifo::MAX_FRAME_SIZE {
            is_full = true;
        }
    }
    // update fifo
    {
        let mut fifo = fifo_mutex.lock().unwrap();
        fifo.set_full(is_full);
        fifo.set_free_idx(free_idx);
        fifo.datagrams_add(num);
    }
    (is_full, free_idx)
}

/// loops forever, processing incoming messages
/// should be run inside a thread in a Link
fn run_inbound(
    running: Arc<AtomicBool>,
    fifo_mutex: Arc<Mutex<NetworkFIFO>>,
    address_in: std::net::SocketAddr,
    stat_mutex: Arc<Mutex<LinkStatistic>>,
    bit_error_rate: f64,
    discard_bitflip_packet: bool,
) -> LinkResult<()> {
    // setup
    let socket: mio::net::UdpSocket = match mio::net::UdpSocket::bind(&address_in) {
        Ok(x) => x,
        Err(e) => return link_error!("{}", e),
    };

    let buffer_size;
    {
        let fifo = fifo_mutex.lock().unwrap();
        buffer_size = fifo.buffer_size(); // is constant
    }

    // used for discarding input while fifo full
    let mut temp_buffer = [0u8; network_fifo::MAX_FRAME_SIZE];

    // socket setup
    let poll = Poll::new()?;
    poll.register(&socket, Token(0), Ready::readable(), PollOpt::edge())?;
    let mut events = Events::with_capacity(1024);

    let mut buffer: &mut [u8];
    let mut start_idx = 0usize;
    let mut free_idx = 0usize;

    // loop forever
    while running.load(Ordering::SeqCst) {
        // check fifo status
        let mut is_full;
        {
            let fifo = fifo_mutex.lock().unwrap();
            is_full = fifo.is_full();
        }

        // use actual ring buffer or spare (discarding) buffer
        if is_full {
            buffer = &mut temp_buffer; // use discarding buffer
        } else {
            {
                let mut fifo = fifo_mutex.lock().unwrap();
                start_idx = fifo.start_idx();
                free_idx = fifo.free_idx();
                unsafe {
                    // creates mutable reference to shared buffer
                    // the ring buffer layout guarantees only this thread is accessing
                    // this particular slice of the buffer at this instant
                    buffer = std::slice::from_raw_parts_mut(
                        &mut fifo.buffer()[free_idx] as *mut u8,
                        network_fifo::MAX_FRAME_SIZE,
                    );
                }
            }
        }

        // process new incoming messages
        poll.poll(&mut events, Some(time::Duration::from_millis(10)))
            .unwrap();
        // process all events
        for _ in events.iter() {
            // there may be multiple packets waiting on the line per event
            // looping here until WouldBlock error caught, i.e. no waiting packets available
            loop {
                match socket.recv(&mut buffer) {
                    Ok(num) => {
                        // message received
                        if is_full {
                            // println!("ADD DISCARD");
                            // discard message
                            let mut stat = stat_mutex.lock().unwrap();
                            stat.dropped_packets += 1;
                            stat.dropped_bytes += num as u64;
                            stat.received_packets += 1;
                            stat.received_bytes += num as u64;
                        } else {
                            // apply bit errors
                            let bit_flips = apply_ber(&mut buffer[..num], bit_error_rate);

                            if discard_bitflip_packet && bit_flips > 0 {
                                let mut stat = stat_mutex.lock().unwrap();
                                stat.received_packets += 1;
                                stat.received_bytes += num as u64;
                                stat.bit_flips += bit_flips as u64;
                                stat.discarded_packets += 1;
                                continue;
                            }

                            // put new message in fifo buffer
                            let result =
                                update_fifo(&fifo_mutex, start_idx, free_idx, num, buffer_size);
                            is_full = result.0;
                            free_idx = result.1;

                            // update statistic
                            let buffer_occupancy;
                            if start_idx == free_idx {
                                if is_full {
                                    buffer_occupancy = 1.0;
                                } else {
                                    buffer_occupancy = 0.0;
                                }
                            } else if start_idx < free_idx {
                                buffer_occupancy =
                                    (free_idx - start_idx) as f32 / buffer_size as f32;
                            } else {
                                buffer_occupancy =
                                    1.0 - (start_idx - free_idx) as f32 / buffer_size as f32;
                            }
                            {
                                let mut stat = stat_mutex.lock().unwrap();
                                stat.received_packets += 1;
                                stat.received_bytes += num as u64;
                                stat.buffer_occupancy = buffer_occupancy;
                                stat.bit_flips += bit_flips as u64;
                            }

                            // prepare buffer for next packet
                            if is_full {
                                buffer = &mut temp_buffer; // use discarding buffer
                            } else {
                                {
                                    let mut fifo = fifo_mutex.lock().unwrap();
                                    start_idx = fifo.start_idx();
                                    free_idx = fifo.free_idx();
                                    unsafe {
                                        // creates mutable reference to shared buffer
                                        // the ring buffer layout guarantees only this thread is accessing
                                        // this particular slice of the buffer at this instant
                                        buffer = std::slice::from_raw_parts_mut(
                                            &mut fifo.buffer()[free_idx] as *mut u8,
                                            network_fifo::MAX_FRAME_SIZE,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        // no new msg on socket
                        // note: (WouldBlock on Unix, TimedOut on Windows)
                        break;
                    }
                    Err(e) => {
                        // abort due to socket error
                        running.store(false, Ordering::SeqCst);
                        return link_error!("Socket error: {}", e);
                    }
                }
            }
        }
    }

    Ok(())
}

/// loops forever, forwarding outgoing messages
/// should be run inside a thread in a Link
fn run_outbound(
    running: Arc<AtomicBool>,
    fifo_mutex: Arc<Mutex<NetworkFIFO>>,
    address_out: std::net::SocketAddr,
    stat_mutex: Arc<Mutex<LinkStatistic>>,
    delay: time::Duration,
) -> LinkResult<()> {
    // create socket with binding to arbitrary free port
    let socket: mio::net::UdpSocket =
        match mio::net::UdpSocket::bind(&"127.0.0.1:0".parse().unwrap()) {
            Ok(x) => x,
            Err(e) => {
                running.store(false, Ordering::SeqCst);
                return link_error!("{}", e);
            }
        };

    // socket setup
    let poll = Poll::new()?;
    poll.register(&socket, Token(0), Ready::writable(), PollOpt::edge())?;
    let mut events = Events::with_capacity(1024);

    // used for link statistics
    let mut rate_calculator = network_fifo::RateCalculator::new();

    let buffer_size;
    let buffer;
    {
        let mut fifo = fifo_mutex.lock().unwrap();
        buffer_size = fifo.buffer_size();
        unsafe {
            // creates mutable reference to shared buffer
            // the ring buffer layout guarantees only this thread is accessing
            // this particular slice of the buffer at this instant
            // (whole buffer reference created, slice selected in forwarding logic further down)
            buffer = std::slice::from_raw_parts_mut(&mut fifo.buffer()[0] as *mut u8, buffer_size);
        }
    }

    // data ready to be forwarded
    let mut forwarding_len;
    // data ready to be forwarded at beginning of buffer (ring buffer fully cycled)
    let mut forwarding_len_second;
    // buffer cycle indicator
    let mut use_second;

    // for statistics
    let mut forwarded_total_packets;
    let mut forwarded_total_bytes;

    // buffer information
    let mut start_idx;
    let mut free_idx;
    let mut is_full;

    // data is sent out in batches if multiple packets are ready
    let batch_size = 950usize;

    // loop forever
    while running.load(Ordering::SeqCst) {
        forwarding_len = 0usize;
        forwarding_len_second = 0usize;
        forwarded_total_packets = 0u64;
        forwarded_total_bytes = 0u64;
        use_second = false;
        // check if messages available
        {
            let mut fifo = fifo_mutex.lock().unwrap();
            start_idx = fifo.start_idx();
            free_idx = fifo.free_idx();
            is_full = fifo.is_full();
            loop {
                match fifo.datagrams_peek() {
                    None => break,
                    Some(m) => {
                        if m.created.elapsed() >= delay {
                            // all messages in buffer which shall be forwarded
                            if start_idx + forwarding_len
                                > buffer_size - network_fifo::MAX_FRAME_SIZE
                            {
                                use_second = true;
                            }
                            if !use_second {
                                forwarding_len += m.len;
                            } else {
                                forwarding_len_second += m.len;
                            }
                            forwarded_total_bytes += m.len as u64;
                            fifo.datagrams_remove();
                            forwarded_total_packets += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
        }

        // no messages waiting
        if forwarded_total_bytes == 0 {
            // no waiting messages at this point
            thread::yield_now();
            // update rate since receiver and forwarder can be 'idling' for a while
            {
                let mut stat = stat_mutex.lock().unwrap();
                stat.forwarding_rate = rate_calculator.get_rate();
            }
            continue;
        }

        // forward waiting message
        while forwarding_len > 0 || forwarding_len_second > 0 {
            poll.poll(&mut events, Some(time::Duration::from_millis(5)))
                .unwrap();
            for _ in events.iter() {
                let begin;
                let end;
                if forwarding_len > batch_size {
                    begin = start_idx;
                    end = start_idx + batch_size;
                    forwarding_len -= batch_size;
                    start_idx += batch_size;
                } else if forwarding_len > 0 {
                    begin = start_idx;
                    end = start_idx + forwarding_len;
                    start_idx += forwarding_len;
                    forwarding_len = 0;
                } else {
                    if use_second {
                        // buffer cycle
                        use_second = false;
                        start_idx = 0;
                    }
                    if forwarding_len_second > batch_size {
                        begin = start_idx;
                        end = start_idx + batch_size;
                        forwarding_len_second -= batch_size;
                        start_idx += batch_size;
                    } else if forwarding_len_second > 0 {
                        begin = start_idx;
                        end = start_idx + forwarding_len_second;
                        start_idx += forwarding_len_second;
                        forwarding_len_second = 0;
                    } else {
                        break;
                    }
                }
                if let Err(e) = socket.send_to(&buffer[begin..end], &address_out) {
                    running.store(false, Ordering::SeqCst);
                    return link_error!("Socket error: {}", e);
                }
            }
        }

        // calculate fifo information
        if is_full {
            if start_idx == free_idx {
                is_full = false;
            } else if start_idx > free_idx && start_idx - free_idx >= network_fifo::MAX_FRAME_SIZE {
                is_full = false;
            } else if start_idx < free_idx
                && (start_idx >= network_fifo::MAX_FRAME_SIZE
                    || free_idx + network_fifo::MAX_FRAME_SIZE < buffer_size)
            {
                is_full = false;
            }
        }

        // update fifo
        {
            let mut fifo = fifo_mutex.lock().unwrap();
            fifo.set_full(is_full);
            fifo.set_start_idx(start_idx);
        }

        // update statistics
        let buffer_occupancy;
        if start_idx == free_idx {
            if is_full {
                buffer_occupancy = 1.0;
            } else {
                buffer_occupancy = 0.0;
            }
        } else if start_idx < free_idx {
            buffer_occupancy = (free_idx - start_idx) as f32 / buffer_size as f32;
        } else {
            buffer_occupancy = 1.0 - ((start_idx - free_idx) as f32 / buffer_size as f32);
        }
        rate_calculator.add(forwarded_total_bytes as u32);
        {
            let mut stat = stat_mutex.lock().unwrap();
            stat.forwarded_packets += forwarded_total_packets;
            stat.forwarded_bytes += forwarded_total_bytes;
            stat.forwarding_rate = rate_calculator.get_rate();
            stat.buffer_occupancy = buffer_occupancy;
        }
    }

    Ok(())
}

/// applies a given BER (bit error rate) to a data buffer
///
/// data: bits are mutated depending on BER
/// ber: bit error rate
/// returns number of flipped bits
fn apply_ber(data: &mut [u8], ber: f64) -> u32 {
    if ber <= 0.0 {
        return 0;
    }
    // applies bit flips to whole buffer (bit-wise) based on BER
    // TODO: optimize
    let mut cnt = 0;
    for i in 0..data.len() - 1 {
        for k in 0..7 {
            if rand::thread_rng().gen_weighted_bool((1.0 / ber) as u32) {
                data[i] ^= 1u8 << k;
                cnt += 1;
            }
        }
    }
    cnt
}

impl Link {
    /// Loops forever, forwarding data on the route
    /// note: function does not return!
    ///
    /// running: flag for handling termination, set by sigint handler
    pub fn start(&mut self, running: Arc<AtomicBool>) -> LinkResult<()> {
        // child thread crash indicator
        let thread_crashed = Arc::new(AtomicBool::new(false));

        // message receive thread
        let recv_running = running.clone();
        let recv_fifo = self.udp_fifo.clone();
        let recv_stat = self.statistic.clone();
        let addr_in = self.route.inbound.clone();
        let recv_crashed = thread_crashed.clone();
        let ber = self.bit_error_rate;
        let discard_bitflip_packet = self.discard_bitflip_packet;
        let recv_thread = thread::spawn(move || {
            match run_inbound(recv_running, recv_fifo, addr_in, recv_stat, ber, discard_bitflip_packet) {
                Ok(_) => (),
                Err(e) => {
                    recv_crashed.store(true, Ordering::SeqCst);
                    println!("(Receiver Error): {}", e);
                }
            }
        });

        // message forwarding thread
        let forwarding_running = running.clone();
        let forwarding_fifo = self.udp_fifo.clone();
        let forwarding_stat = self.statistic.clone();
        let addr_out = self.route.outbound.clone();
        let forwarding_crashed = thread_crashed.clone();
        let delay_duration = time::Duration::new(
            (self.delay_ms / 1000) as u64,
            ((self.delay_ms % 1000) * 1000000) as u32,
        );
        let forwarding_thread = thread::spawn(move || {
            match run_outbound(
                forwarding_running,
                forwarding_fifo,
                addr_out,
                forwarding_stat,
                delay_duration,
            ) {
                Ok(_) => (),
                Err(e) => {
                    forwarding_crashed.store(true, Ordering::SeqCst);
                    println!("(Forwarder Error): {}", e);
                }
            }
        });

        forwarding_thread.join().unwrap();
        recv_thread.join().unwrap();

        if thread_crashed.load(Ordering::SeqCst) {
            return link_error!("Runtime error"); // error message was printed beforehand from thread
        }

        Ok(())
    }
}

/// Route struct defines a data forwarding route between two endpoints
/// (inbound and outbound)
pub struct Route {
    pub inbound: std::net::SocketAddr,
    pub outbound: std::net::SocketAddr,
}

impl Route {
    pub fn new(address_in: &str, address_out: &str) -> LinkResult<Route> {
        let inbound = match parse_socket_address(address_in) {
            Ok(x) => x,
            Err(e) => return link_error!("Inbound '{}' ({})", address_in, e),
        };
        let outbound = match parse_socket_address(address_out) {
            Ok(x) => x,
            Err(e) => return link_error!("Outbound '{}' ({})", address_out, e),
        };
        Ok(Route { inbound, outbound })
    }
}

/// parses a string (IPv4:port) into SocketAddrV4
///
/// address: String representation of SocketAddrV4, expected format:
/// xxx.xxx.xxx.xxx:port (IPv4:port)
fn parse_socket_address(address: &str) -> LinkResult<std::net::SocketAddr> {
    let mut ip_port: Vec<&str> = address.split(":").collect();
    if ip_port.len() != 2 {
        return link_error!("Invalid format, expected exactly one colon");
    }
    let port = ip_port[1].parse::<u16>()?;
    if ip_port[0] == "localhost" {
        ip_port[0] = "127.0.0.1"; // rust net struct cannot parse localhost
    }
    let ip: std::net::IpAddr = ip_port[0].parse()?;
    Ok(std::net::SocketAddr::new(ip, port))
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} to {}", self.inbound, self.outbound)
    }
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "[{}] {} | {} {} ",
            self.name, self.route, self.bit_error_rate, self.delay_ms
        )
    }
}
