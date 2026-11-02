//! Prints the public address STUN servers see for one UDP socket, the same way a call
//! discovers its server-reflexive candidate. Two sockets show whether the NAT keeps the
//! port per socket (endpoint-independent mapping, hole punching works) or changes it per
//! destination (symmetric NAT: needs TURN).
//!
//!   cargo run -p turbo-pancake-media-transport --example stun_probe [host:port ...]

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use turbo_pancake_media_transport::UdpPeer;

fn main() {
    let mut servers: Vec<String> = std::env::args().skip(1).collect();
    if servers.is_empty() {
        servers = vec![
            "stun.l.google.com:19302".into(),
            "stun.cloudflare.com:3478".into(),
        ];
    }
    for server in &servers {
        let addrs: Vec<SocketAddr> = match server.to_socket_addrs() {
            Ok(a) => a.filter(SocketAddr::is_ipv4).collect(),
            Err(e) => {
                println!("{server}: resolve failed: {e}");
                continue;
            }
        };
        let mut peer = UdpPeer::bind("0.0.0.0:0".parse().unwrap()).expect("bind");
        let t = Instant::now();
        let mapped = peer.add_server_reflexive(&addrs, Duration::from_secs(2));
        println!(
            "{server} ({}): local {} -> public {} in {} ms",
            addrs.first().map_or("-".into(), |a| a.to_string()),
            peer.local_addr(),
            mapped.map_or("none".into(), |a| a.to_string()),
            t.elapsed().as_millis()
        );
    }
}
