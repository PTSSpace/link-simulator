/// Link Simulator
///
/// Copyright (C) 2019 PTScientists GmbH
///
/// 17.04.2019    Eric Reinthal

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time;

/// largest expected packet on any link
pub const MAX_FRAME_SIZE: usize = 2000;

/// metadata for a single network package
pub struct Datagram {
    pub len: usize,
    pub created: time::Instant,
}

/// stores network packages in a FIFO structure, keeping track of buffer usage and the single
/// packages itself
///
/// package metadata is stored in a vector, while the data itself is put into a ringbuffer
/// the exact location of a certain packet in the buffer can only be determined with reference
/// to the other packages, e.g.
/// start of package(2) = start_idx + len(package(0)) + len(package(1))
/// (just as an example how storage works, FIFO just provides push() and remove())
///
/// data packets are not split over the ring buffer start/end, a would-overlapping package is
/// put at the very first location in the buffer. This leads to 'buffer_full' even if some bytes
/// (<MAX_FRAME_SIZE) are not occupied at the end of the buffer.
///
/// Also 'buffer_full' is flagged if there are less than MAX_FRAME_SIZE bytes available ahead of
/// data-start pointer
///
/// By this, it is guaranteed that any incoming network package not exceeding MAX_FRAME_SIZE will
/// fit into the buffer, as long as 'buffer_full' is not true
pub struct NetworkFIFO {
    datagrams: Vec<Datagram>,
    buffer: Vec<u8>,
    buffer_size: usize,
    start_idx: usize,
    free_idx: usize,
    is_full: bool,
}

impl Datagram {
    pub fn new(len: usize) -> Datagram {
        Datagram {
            len,
            created: time::Instant::now(),
        }
    }
}

impl NetworkFIFO {
    /// creates a new NetworkFIFO
    ///
    /// buffer_size (byte): should be adjusted to the delay and bandwidth in the network
    /// plus some overhead to accommodate for additional delays in the network
    /// not part of the simulation environment
    /// e.g. delay 3s + 4Mbit connection => 12Mbit buffer needed + 10% overhead storage
    /// buffer_size => 1.65 Mb
    pub fn new(buffer_size: usize) -> NetworkFIFO {
        let buffer = vec![0u8; buffer_size];
        let datagrams = Vec::new();
        NetworkFIFO {
            datagrams,
            buffer,
            buffer_size,
            start_idx: 0,
            free_idx: 0,
            is_full: false,
        }
    }

    // getter methods
    pub fn is_full(&self) -> bool {
        self.is_full
    }
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }
    pub fn start_idx(&self) -> usize {
        self.start_idx
    }
    pub fn free_idx(&self) -> usize {
        self.free_idx
    }
    pub fn buffer(&mut self) -> &mut Vec<u8> {
        &mut self.buffer
    }

    // setter methods

    // exposing the following functions is potentially dangerous, but allows for more concurrency
    // between receiver and forwarder thread
    // OK as long as only used by the lib itself (and not in a public API to network_fifo)
    pub fn set_full(&mut self, is_full: bool) {
        self.is_full = is_full;
    }
    pub fn set_start_idx(&mut self, start_idx: usize) {
        self.start_idx = start_idx;
    }
    pub fn set_free_idx(&mut self, free_idx: usize) {
        self.free_idx = free_idx;
    }

    /// FIFO add element
    pub fn datagrams_add(&mut self, num_bytes: usize) {
        self.datagrams.push(Datagram::new(num_bytes));
    }

    /// FIFO look at first element, returns None if FIFO empty
    pub fn datagrams_peek(&self) -> Option<&Datagram> {
        self.datagrams.first()
    }

    /// FIFO get first element
    ///
    /// panics if FIFO is empty!
    pub fn datagrams_remove(&mut self) -> Datagram {
        self.datagrams.remove(0)
    }
}

/// calculates data rates based on incoming data
///
/// uses an own thread to keep track of 'outdated' data, i.e. data which was received
/// over one second ago
pub struct RateCalculator {
    rate: Arc<AtomicUsize>,
    accumulator: Arc<AtomicUsize>,
    handle: Option<thread::JoinHandle<()>>,
    tx: mpsc::Sender<()>,
}

impl RateCalculator {
    pub fn new() -> RateCalculator {
        let rate = Arc::new(AtomicUsize::new(0));
        let accumulator = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let rate_clone = rate.clone();
        let accumulator_clone = accumulator.clone();
        let handle = Some(thread::spawn(move || {
            let interval_ms: usize = 200;
            // sliding average filter
            const FILTER_LEN: usize = 15usize;
            let rate_duration = time::Duration::from_millis(interval_ms as u64);
            let mut filter = vec![0usize; FILTER_LEN];
            let mut idx = 0usize;
            // loop forever, update rate with accumulated data from last rate_duration interval
            // uses a sliding average filter for smooth rate calculation over one second
            loop {
                let now = time::Instant::now();
                match rx.try_recv() {
                    Ok(_) => break,
                    Err(mpsc::TryRecvError::Empty) => {
                        filter[idx] = accumulator_clone.swap(0usize, Ordering::SeqCst);
                        idx = (idx + 1) % FILTER_LEN;
                        let filter_sum: usize = filter.iter().sum();
                        rate_clone.store(
                            filter_sum * 1000 / (FILTER_LEN * interval_ms),
                            Ordering::SeqCst,
                        );
                        thread::sleep(rate_duration - now.elapsed());
                    }
                    Err(_) => break,
                }
            }
        }));
        RateCalculator {
            rate,
            accumulator,
            handle,
            tx,
        }
    }

    /// add incoming data
    pub fn add(&mut self, amount: u32) {
        self.accumulator
            .fetch_add(amount as usize, Ordering::SeqCst);
    }

    /// returns the number of bytes received in the last second
    ///
    /// return: the data rate in bytes/second
    pub fn get_rate(&self) -> u32 {
        self.rate.load(Ordering::SeqCst) as u32
    }
}

impl Drop for RateCalculator {
    /// custom destructor fot terminating child thread
    fn drop(&mut self) {
        self.tx.send(()).unwrap();
        self.handle.take().unwrap().join().unwrap();
    }
}
