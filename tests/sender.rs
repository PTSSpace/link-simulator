/// Link Simulator
///
/// Copyright (C) 2019 PTScientists GmbH
///
/// 17.04.2019    Eric Reinthal

use std::net;
use std::thread;
use std::time;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

extern crate term;

// must be an inbound address of an active link
const TARGET_ADDRESS:&str = "127.0.0.1:2000";

/// Functional Test support program (sender)
/// emits data at predefined rate, printing total sent data once per second
/// configure kbit sending rate with cmd line argument
///
/// Note: this is not compiled as a classical rust-test since it is used as a
/// normal binary with all optimizations turned on (--release build)
fn main() {
    // configuration
    if std::env::args().len() < 2 {
        println!("Sending bitrate in [kbps] has to be provided as first argument");
        return;
    }
    let mut it = std::env::args();
    it.next(); // skip first (program name)
    let kbit: f32 = it.next().unwrap().parse().unwrap();
    let pack_size = 600;

    // SIGINT capture setup
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    }).expect("Error setting Ctrl-C handler");

    let interval: f32 = 8.0 * pack_size as f32 / kbit;
    println!(
        "Sending {} byte each {} (rate: {} bps)",
        pack_size,
        interval,
        pack_size as f32 * 8.0 * 1000.0 / interval
    );
    let buf = vec![0u8; pack_size];
    let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let target: net::SocketAddr = TARGET_ADDRESS.parse().unwrap();
    let mut total = 0u64;
    let mut last = time::Instant::now();
    let sleep_time = time::Duration::from_millis(interval as u64);

    println!("Total sent:");
    println!();
    let mut terminal = term::stdout().unwrap();
    while running.load(Ordering::SeqCst) {
        let now = time::Instant::now();
        socket.send_to(&buf, target).unwrap();
        total += pack_size as u64;
        if last.elapsed() > time::Duration::from_secs(1) {
            last = time::Instant::now();
            terminal.cursor_up().unwrap();
            terminal.delete_line().unwrap();
            println!("{:.3} KB", total as f64 / 1000f64);
        }
        if sleep_time > now.elapsed() {
            thread::sleep(sleep_time - now.elapsed());
        }
    }
    println!("\nShutdown. Total data sent: {}byte", total);
}
