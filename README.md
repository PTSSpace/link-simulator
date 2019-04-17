# Link Simulator

Rust tool for simulating an arbitrary connection between two network endpoints.

Features:

- Any number of simultaneous connections with individual configurations
- Configurable delay per link
- Data mutation based on a fixed bit error rate to simulate space RF links
- Rate control (excess incoming bytes are dropped)
- Efficiency through zero-copy forwarding of all data packets
- Network configuration through config files
- Extensive network performance insight:
    * Received bytes & packets
    * Dropped bytes & packets
    * Forwarded bytes & packets
    * Actual bandwidth
    * Internal buffer occupancy
    * Mutated bits (flips)
- Optional Grafana integration for monitoring

## Build Instructions

Install Rust on your system, then simply execute

```
cargo build --release
```

in the project's root folder to build the executable and the test programs.

## Usage

Run the executable `link_simulator` (found in `/target/release`) with a routes
configuration file according to the following syntax:


```
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
    Test_route localhost:2000 localhost:2001 1e-5 2000 1000000
```

an example routes configuration file (`routes.config`) is provided in the
project directory.

### Grafana Integration

First, set up [InfluxDB](https://www.influxdata.com/) and [Grafana](https://grafana.com/)
- Influx DB must listen for input at UDP 127.0.0.1:8100
- Grafana must be connected to Influx DB and the provided config file must be loaded (`/grafana/link_simulator.json`)

## Testing locally

Terminal window #1:

```
./target/release/link_test_receiver
```

Terminal window #2:

```
./target/release/link_simulator -i routes.config
```

Terminal window #3:

```
./target/release/link_test_sender 800
```

The sender (#3) will continuously send packets over the `Test_route` link
at a rate of 800kbps. The link should indicate the reception and forwarding of
packets, a rate and applied bitflips (#2).
The receiver (#1) should also indicate received data 2 seconds after starting
the sender (configured delay in `routes.config`).

### Test success criteria

Terminate (Ctrl+C) the sender (#3), wait at least 2s (or other configured delay),
then terminate the receiver (#1).
- The link should indicate 0 dropped packets, if `routes.config` was left
unchanged
- Sender and receiver sent and received amount of data should match
- Bit flips in link and receiver should match
- Link received and forwarded data amount and packet count should match
