use std::net::SocketAddr;

fn basic() -> Result<(), std::io::Error> {
    let interface = quicsock::InterfaceIndex(12);
    let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

    let udp = quicsock::bind_udp(interface, local_addr)?;
    let tcp = quicsock::bind_tcp(interface, local_addr)?;
    let _ = (udp, tcp);
    Ok(())
}

#[cfg(feature = "discovery")]
fn discovery_example() {
    for (index, iface) in quicsock::discovery::list_interfaces() {
        println!("{index:?}: {} ({:?})", iface.name, iface.if_type);
    }
}

fn main() {
    let _ = basic();
    #[cfg(feature = "discovery")]
    discovery_example();
}
