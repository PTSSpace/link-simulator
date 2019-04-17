/// Link Simulator
///
/// Copyright (C) 2019 PTScientists GmbH
///
/// 17.04.2019    Eric Reinthal

use std::net;
use std::time;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

extern crate term;

//must be an outbound address of the active link connected to the sender test counterpart
const SOURCE_ADDRESS:&str = "127.0.0.1:2001";

/// Functional Test support program (receiver)
/// reads data on a link, printing total sent data once per second
/// also counts bit flips, assuming all-zeros as nominal data
///
/// Note: this is not compiled as a classical rust-test since it is used as a
/// normal binary with all optimizations turned on (--release build)
fn main() {
    // SIGINT capture setup
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    }).expect("Error setting Ctrl-C handler");

    let mut buf = [0u8; 4000];
    let socket = net::UdpSocket::bind(SOURCE_ADDRESS)
        .expect(&format!("Could not bind to address {}", SOURCE_ADDRESS));
    socket.set_read_timeout(Some(time::Duration::from_millis(100))).unwrap();
    let mut total = 0u64;
    let mut flips = 0u64;
    let mut last = time::Instant::now();

    println!("Total received:");
    println!();
    let mut terminal = term::stdout().unwrap();
    while running.load(Ordering::SeqCst) {
        match socket.recv(&mut buf) {
            Ok(num) => {
                total += num as u64;
                // bit flips
                let _:Vec<&u8> = buf[..num].into_iter().filter(|x| **x>0)
                    .map(|x| {
                        let mut y:u8 = *x;
                        for _ in 0..8 {
                            flips += (y & 1u8) as u64;
                            y = y >> 1;
                        }
                        x
                    }).collect();
                // print status once per second
                if last.elapsed() > time::Duration::from_secs(1) {
                    last = time::Instant::now();
                    terminal.cursor_up().unwrap();
                    terminal.delete_line().unwrap();
                    println!("{:.3} KB flips: {}", total as f64 / 1000f64, flips);
                }
            },
			// do nothing on 'no input'
			// note: (WouldBlock on Unix, TimedOut on Windows)
            Err(ref e) if (e.kind() == std::io::ErrorKind::WouldBlock) ||
                (e.kind() == std::io::ErrorKind::TimedOut) => {},
            Err(e) => panic!("UDP socket error: {}", e),
        }

    }
    println!("\nShutdown. Total data received: {}byte, flips: {}", total, flips);
}
