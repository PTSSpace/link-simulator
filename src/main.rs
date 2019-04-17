/// Link Simulator
///
/// Copyright (C) 2019 PTScientists GmbH
///
/// 17.04.2019    Eric Reinthal

extern crate ctrlc;
extern crate link;
extern crate term;

use link::Link;
use link::LinkStatistic;
use mio::net;
use rand::Rng;
use std::env;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time;

const USAGE: &str = "USAGE:
    link_simulator [options] <file>

    options:
        -i              Print link info on startup
        -s              Print link statistics to stdout
                        (only works on terminals with 60+ chars per line)
                        (does not work on Windows)
        -gLINK_NAME     Send link statistics to Grafana for specified link
                        (repeat this option if needed)
        -G              Send statistics of all links to Grafana
        -p              Discard packet if bit is flipped

    <file> must have the following format:
        link_name address_in address_out bit_error_rate delay_ms max_bandwidth
        ...
        ...
    (lines starting with '#' are ignored)
    line format:
        link_name:          String (without spaces)
        address_{in/out}:   IPv4:Port
        bit_error_rate:     float (0.0 ... 1.0(
        delay_ms:           integer [ms]
        max_bandwidth:      float [bps]
    example:
        Test_route localhost:2000 localhost:2001 1e-5 2000 1000000";
		
const INFLUXDB_ADDRESS: &str = "127.0.0.1:8100";

// config arguments per link definition, see USAGE above
const ARGS_PER_LINK: usize = 6;

fn main() {
    // SIGINT capture setup
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    // read configuration file
    let config = read_input();
    if config.is_none() {
        println!("{}", USAGE);
        std::process::exit(1);
    }
    let config = config.unwrap();
    let active_links = config.measurable_links;
    let num_links = active_links.len();

    if config.print_info {
        for l in active_links.iter() {
            println!(
                "Link {}: {}\n\t(BER: {}, delay: {}ms, max. bandwidth: {}bps)",
                l.link.name(),
                l.link.route(),
                l.link.bit_error_rate(),
                l.link.delay_ms(),
                link::bytes_to_readable(l.link.max_bandwidth() as u64)
            );
            if l.grafana_output {
                println!("\tSent to Grafana");
            }
        }
    }

    // grafana setup
    // must match input address / port of Influx DB
    let grafana_output = INFLUXDB_ADDRESS.parse().unwrap();
    // udp socket for sending messages to grafana-dump
    let grafana_socket = net::UdpSocket::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
    let mut grafana_stat = Vec::new();
    // generate unique session id
    let grafana_session: u16 = rand::thread_rng().gen();
    if config.send_to_grafana {
        // send initial link info
        for l in active_links.iter().filter(|x| x.grafana_output) {
            let influx = format!(r#"link_config,name={},session_tag={} name="{}",session={},inbound="{}",outbound="{}",ber={},delay={}i,bandwidth={}"#,
                                 l.link.name(),
                                 grafana_session,
                                 l.link.name(),
                                 grafana_session,
                                 l.link.route().inbound,
                                 l.link.route().outbound,
                                 l.link.bit_error_rate(),
                                 l.link.delay_ms(),
                                 l.link.max_bandwidth());
            grafana_socket
                .send_to(&influx.as_bytes()[..influx.len()], &grafana_output)
                .unwrap();
            grafana_stat.push(Statistic::new(
                l.link.name().to_string(),
                l.statistic.clone(),
                true,
            ));
        }
    }

    // start forwarding threads
    let mut link_threads = Vec::with_capacity(num_links);
    let mut statistics = Vec::with_capacity(num_links);
    for active_link in active_links {
        let link_stat = active_link.statistic;
        statistics.push(Statistic::new(
            active_link.link.name().to_string(),
            link_stat,
            active_link.grafana_output,
        ));
        let mut link = active_link.link;
        let r = running.clone();
        let r_stop = running.clone();
        link_threads.push(thread::spawn(move || {
            match link.start(r) {
                Ok(_) => (),
                Err(e) => {
                    println!("Link '{}' crashed: {}", link.name(), e);
                    r_stop.store(false, Ordering::SeqCst);
                }
            };
        }));
    }

    // print statistics to stdout
    let mut thread_stat_printer: Option<thread::JoinHandle<()>> = None;
    if config.print_statistic {
        let running_stat = running.clone();
        thread_stat_printer = Some(thread::spawn(move || {
            // continuous route statistics output
            let mut terminal = term::stdout().unwrap();
            for _ in 0..num_links {
                println!(); // prepare output lines
            }
            while running_stat.load(Ordering::SeqCst) {
                for _ in 0..num_links {
                    terminal.cursor_up().unwrap();
                }
                for statistic in &mut statistics {
                    // used to acquire stat lock as short as possible
                    // i.e. not during println
                    let link_statistic: LinkStatistic;
                    {
                        link_statistic = statistic.link_statistic.lock().unwrap().clone();
                    }
                    terminal.delete_line().unwrap();
                    println!("[{:<14}]  {}", statistic.name, link_statistic);
                }
                thread::sleep(time::Duration::from_millis(500));
            }
        }));
    }

    // send statistics to grafana
    let mut thread_send_grafana: Option<thread::JoinHandle<()>> = None;
    if config.send_to_grafana {
        let running_grafana = running.clone();
        thread_send_grafana = Some(thread::spawn(move || {
            // run forever
            while running_grafana.load(Ordering::SeqCst) {
                // for all grafana-active links
                for stat in &grafana_stat {
                    // copy in order to lock as short as possible
                    let s: LinkStatistic;
                    {
                        s = stat.link_statistic.lock().unwrap().clone();
                    }
                    // influx DB string
                    let influx = format!(r#"link_stat,session={},name={} recv_b={}i,recv_p={}i,drop_b={}i,drop_p={}i,forw_b={}i,forw_p={}i,rate={},buffer={},flips={}i,discards={}i"#,
                                         grafana_session, stat.name, s.received_bytes, s.received_packets, s.dropped_bytes, s.dropped_packets, s.forwarded_bytes, s.forwarded_packets, s.forwarding_rate*8, s.buffer_occupancy, s.bit_flips, s.discarded_packets);
                    grafana_socket
                        .send_to(&influx.as_bytes()[..influx.len()], &grafana_output)
                        .unwrap();
                }
                thread::sleep(time::Duration::from_millis(500));
            }
        }));
    }

    // gracefully terminate
    if let Some(t) = thread_stat_printer {
        t.join().unwrap();
    }
    if let Some(t) = thread_send_grafana {
        t.join().unwrap();
    }
    for t in link_threads {
        t.join().unwrap();
    }
}

/// associates a link with a LinkStatistic and a name, used for terminal output
/// and possibly sending status data (network metadata) at a later point
struct MeasurableLink {
    pub link: Link,
    pub statistic: Arc<Mutex<LinkStatistic>>,
    pub grafana_output: bool,
}

impl MeasurableLink {
    fn new(
        link: Link,
        statistic: Arc<Mutex<LinkStatistic>>,
        grafana_output: bool,
    ) -> MeasurableLink {
        MeasurableLink {
            link,
            statistic,
            grafana_output,
        }
    }
}

/// associates a LinkStatistic with a name and a field for calculating the rate
struct Statistic {
    pub name: String,
    pub link_statistic: Arc<Mutex<LinkStatistic>>,
    pub grafana_output: bool,
}

impl Statistic {
    fn new(
        name: String,
        link_statistic: Arc<Mutex<LinkStatistic>>,
        grafana_output: bool,
    ) -> Statistic {
        Statistic {
            name,
            link_statistic,
            grafana_output,
        }
    }
}

/// configuration from command line parameters
struct Configuration {
    pub measurable_links: Vec<MeasurableLink>,
    pub print_info: bool,
    pub print_statistic: bool,
    pub send_to_grafana: bool,
    pub discard_bitflip_packet: bool,
}

impl Configuration {
    fn new(
        measurable_links: Vec<MeasurableLink>,
        print_info: bool,
        print_statistic: bool,
        send_to_grafana: bool,
        discard_bitflip_packet: bool,
    ) -> Configuration {
        Configuration {
            measurable_links,
            print_info,
            print_statistic,
            send_to_grafana,
            discard_bitflip_packet,
        }
    }
}

/// parse input file and create link structs
fn read_input() -> Option<Configuration> {
    // see USAGE definition above
    // parse options, then file name
    let mut print_link_info = false;
    let mut print_link_statistic = false;
    let mut grafana_send_all = false;
    let mut grafana_output = Vec::new();
    let mut discard_bitflip_packet = false;
    let mut file_name = String::from("");

    // arguments iterator, skip first (program name)
    let mut args = env::args();
    if args.len() < 2 {
        return None;
    }
    args.next();

    // parse arguments
    while file_name.as_str() == "" {
        match args.next() {
            None => break,
            Some(arg) => {
                let mut chars = arg.chars();
                if chars.next().unwrap() == '-' {
                    // parse options
                    if arg == "-i" {
                        // print to info
                        print_link_info = true;
                    } else if arg == "-s" {
                        // print link statistic
                        print_link_statistic = true;
                    } else if arg == "-G" {
                        grafana_send_all = true;
                    } else if arg == "-p" {
                        discard_bitflip_packet = true;
                    } else {
                        match chars.next() {
                            None => {
                                // invalid argument
                                println!("Invalid argument: {}", arg);
                                return None;
                            }
                            // send to grafana
                            Some(c) => {
                                if c == 'g' {
                                    // just save name of link, skip '-g'
                                    grafana_output.push(arg[2..].to_string());
                                } else {
                                    // invalid argument
                                    println!("Invalid argument: {}", arg);
                                    return None;
                                }
                            }
                        }
                    }
                } else {
                    // parse config filename
                    file_name = arg.to_string();
                }
            }
        }
    }

    if file_name.as_str() == "" {
        println!("No configuration file provided");
        return None;
    }

    let grafana_count = grafana_output.len();
    if grafana_send_all && grafana_count > 0 {
        println!("Either '-G' or some '-g' arguments may be provided, not both");
        return None;
    }

    // result vector
    let mut links = Vec::new();

    let contents = match fs::read_to_string(&file_name) {
        Ok(x) => x,
        Err(e) => {
            println!("Could not open file '{}': {}", file_name, e);
            std::process::exit(1);
        }
    };

    for line in contents
        .lines()
        .filter(|x| x.len() > 0) // empty lines
        .filter(|x| x.chars().next().unwrap() != '#')
    // comment lines
    {
        let components: Vec<&str> = line.split_whitespace().collect();
        if components.len() != ARGS_PER_LINK {
            println!("'{}'\nLine invalid, aborting", line);
            return None;
        }
        let ber = match components[3].parse::<f64>() {
            Ok(x) => x,
            Err(_) => {
                println!(
                    "'{}'\n4th argument (bit error rate) must be numeric, aborting",
                    line
                );
                return None;
            }
        };
        let delay = match components[4].parse::<u32>() {
            Ok(x) => x,
            Err(_) => {
                println!("'{}'\n5th argument (delay) must be numeric, aborting", line);
                return None;
            }
        };
        let max_bw = match components[5].parse::<f64>() {
            Ok(x) => x,
            Err(_) => {
                println!(
                    "'{}'\n6th argument (max. bandwidth) must be numeric, aborting",
                    line
                );
                return None;
            }
        };

        let stat_local = Arc::new(Mutex::new(Default::default()));
        let stat_link = stat_local.clone();
        let link = match Link::new(
            components[0].to_string(),
            &components[1],
            &components[2],
            ber,
            delay,
            max_bw,
            stat_link,
            discard_bitflip_packet,
        ) {
            Ok(x) => x,
            Err(e) => {
                println!("{}", e);
                return None;
            }
        };

        // check if this link will be sent to grafana
        let mut grafana_flag = false;
        if let Some(_) = grafana_output
            .iter()
            .find(|&x| x == &components[0].to_string())
        {
            grafana_flag = true;
            grafana_output.retain(move |x| x != &components[0].to_string());
        }

        // push a link with its accompanying statistics reference
        links.push(MeasurableLink::new(
            link,
            stat_local,
            grafana_send_all || grafana_flag,
        ));
    }

    if links.len() == 0 {
        println!("No links defined in configuration file");
        return None;
    }

    if grafana_output.len() != 0 {
        println!("Invalid links specified for Grafana output:");
        for l in grafana_output {
            println!(" - {}", l);
        }
        return None;
    }

    Some(Configuration::new(
        links,
        print_link_info,
        print_link_statistic,
        grafana_send_all || grafana_count > 0,
        discard_bitflip_packet,
    ))
}
