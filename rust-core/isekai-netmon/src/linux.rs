//! Linux backend: a raw `AF_NETLINK`/`NETLINK_ROUTE` socket subscribed to
//! the link/address/route multicast groups (`RTMGRP_LINK` |
//! `RTMGRP_IPV4_IFADDR` | `RTMGRP_IPV6_IFADDR` | `RTMGRP_IPV4_ROUTE` |
//! `RTMGRP_IPV6_ROUTE`). No root/`CAP_NET_ADMIN` is required to *read* these
//! multicast groups (same as e.g. `ip monitor`), only to *modify* routing
//! state.
//!
//! Those groups fire on far more than what matters to a connection (every
//! docker/veth creation, VPN route push, IPv6 address-lifetime refresh on
//! each router advertisement), so a notification is only a *poke*: the
//! shared [`crate::snapshot::run_debounced_worker`] debounces the burst,
//! then takes a fresh [`NetworkSnapshot`] via netlink *dump* requests
//! (`RTM_GETLINK`/`RTM_GETADDR`/`RTM_GETROUTE` on a separate short-lived
//! socket, parsed by [`parse_dump`]) and emits an `InterfaceChange` only if
//! the reachability-relevant state actually differs (see `snapshot.rs`).
//!
//! No async-friendly netlink API exists without extra dependencies, so this
//! owns a dedicated background thread doing blocking reads. The notification
//! socket's `SO_RCVTIMEO` is [`TICK`] so the thread periodically checks its
//! stop flag; `Drop` only sets that flag (the thread owns and closes its own
//! fd), so dropping the monitor never blocks.
//!
//! Defines its own minimal `sockaddr_nl` shape (`SockaddrNl` below) rather
//! than using `libc::sockaddr_nl` directly: that struct's padding field is
//! private in recent `libc` versions, so it isn't constructible from
//! outside the crate via a struct literal. The netlink `sockaddr_nl` ABI
//! (`family`/`pad`/`pid`/`groups`, 12 bytes) is a stable kernel UAPI shape.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::snapshot::{is_trackable_address, run_debounced_worker, NetworkSnapshot, Tick, TICK};
use crate::{NetworkChangeCause, NetworkChangeEvent, NetworkChangeMonitor};

/// `<linux/netlink.h>`'s `NETLINK_ROUTE` (always `0`).
const NETLINK_ROUTE: libc::c_int = 0;

// Kernel UAPI constants (`<linux/netlink.h>`, `<linux/rtnetlink.h>`,
// `<linux/if.h>`), hardcoded for the same reason as `SockaddrNl`.
const NLMSG_HDRLEN: usize = 16;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_GETROUTE: u16 = 26;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const RT_SCOPE_LINK: u8 = 253;
const RT_TABLE_MAIN: u32 = 254;
const RTN_UNICAST: u8 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_MULTIPATH: u16 = 9;
const RTA_TABLE: u16 = 15;
const IFINFOMSG_LEN: usize = 16;
const IFADDRMSG_LEN: usize = 8;
const RTMSG_LEN: usize = 12;
const RTNEXTHOP_LEN: usize = 8;

/// Minimal, hand-rolled `struct sockaddr_nl` (see module docs).
#[repr(C)]
struct SockaddrNl {
    nl_family: libc::sa_family_t,
    nl_pad: u16,
    nl_pid: u32,
    nl_groups: u32,
}

/// Owns a netlink fd and closes it on drop.
struct NetlinkFd(RawFd);

impl Drop for NetlinkFd {
    fn drop(&mut self) {
        // SAFETY: the fd is owned exclusively by this value.
        unsafe { libc::close(self.0) };
    }
}

/// Opens an `AF_NETLINK`/`NETLINK_ROUTE` socket (close-on-exec, so it never
/// leaks into a spawned child) bound to `groups`, with `SO_RCVTIMEO` set to
/// `recv_timeout`.
fn open_netlink(groups: u32, recv_timeout: Duration) -> Result<NetlinkFd, String> {
    // SAFETY: plain socket(2) with constant arguments; checked below.
    let fd: RawFd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, NETLINK_ROUTE) };
    if fd < 0 {
        return Err(format!("socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE) failed: {}", std::io::Error::last_os_error()));
    }
    let fd = NetlinkFd(fd);
    let addr = SockaddrNl { nl_family: libc::AF_NETLINK as libc::sa_family_t, nl_pad: 0, nl_pid: 0, nl_groups: groups };
    // SAFETY: `fd` is a fresh socket; `addr` is fully initialized and its
    // size is passed alongside it.
    let bind_result = unsafe {
        libc::bind(fd.0, &addr as *const SockaddrNl as *const libc::sockaddr, std::mem::size_of::<SockaddrNl>() as libc::socklen_t)
    };
    if bind_result < 0 {
        return Err(format!("bind(AF_NETLINK) failed: {}", std::io::Error::last_os_error()));
    }
    let timeout = libc::timeval {
        tv_sec: recv_timeout.as_secs() as libc::time_t,
        tv_usec: recv_timeout.subsec_micros() as libc::suseconds_t,
    };
    // SAFETY: valid fd, fully initialized timeval. Failure is non-fatal.
    unsafe {
        libc::setsockopt(
            fd.0,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &timeout as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }
    Ok(fd)
}

pub struct LinuxNetworkChangeMonitor {
    receiver: mpsc::UnboundedReceiver<NetworkChangeEvent>,
    stop: Arc<AtomicBool>,
}

impl LinuxNetworkChangeMonitor {
    pub fn new() -> Result<Self, String> {
        let groups = (libc::RTMGRP_LINK
            | libc::RTMGRP_IPV4_IFADDR
            | libc::RTMGRP_IPV6_IFADDR
            | libc::RTMGRP_IPV4_ROUTE
            | libc::RTMGRP_IPV6_ROUTE) as u32;
        let notify = open_netlink(groups, TICK)?;

        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("isekai-netmon-linux".to_string())
            .spawn(move || {
                let notify = notify;
                let mut buf = vec![0u8; 8192];
                run_debounced_worker(
                    &worker_stop,
                    || {
                        // SAFETY: `notify.0` is valid for this thread's
                        // whole lifetime; `buf` is a writable buffer of the
                        // given length. Contents are not parsed — a
                        // notification only pokes the worker.
                        let n = unsafe { libc::recv(notify.0, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
                        if n > 0 {
                            return Tick::Poke;
                        }
                        let err = std::io::Error::last_os_error();
                        match err.raw_os_error() {
                            Some(libc::EAGAIN) | Some(libc::EINTR) => Tick::Idle,
                            // ENOBUFS: the kernel dropped notifications
                            // because we fell behind — something changed.
                            Some(libc::ENOBUFS) => Tick::Poke,
                            _ => {
                                // Any other error: back off instead of
                                // spinning on it, but keep going.
                                log::debug!("isekai-netmon: netlink recv failed: {err}");
                                std::thread::sleep(TICK);
                                Tick::Idle
                            }
                        }
                    },
                    take_snapshot,
                    || tx.send(NetworkChangeEvent { cause: NetworkChangeCause::InterfaceChange }).is_ok(),
                );
            })
            .map_err(|e| format!("failed to spawn the netlink-monitor thread: {e}"))?;

        Ok(Self { receiver: rx, stop })
    }
}

#[async_trait]
impl NetworkChangeMonitor for LinuxNetworkChangeMonitor {
    async fn next_change(&mut self) -> Option<NetworkChangeEvent> {
        self.receiver.recv().await
    }
}

impl Drop for LinuxNetworkChangeMonitor {
    fn drop(&mut self) {
        // The worker thread owns its fd and exits (closing it) within one
        // TICK; not joining keeps drop non-blocking.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Takes a fresh snapshot via three netlink dump requests on a short-lived
/// socket.
fn take_snapshot() -> Result<NetworkSnapshot, String> {
    let fd = open_netlink(0, Duration::from_secs(1))?;
    let mut snapshot = NetworkSnapshot::default();
    for (seq, (msg_type, body_len)) in [(RTM_GETLINK, IFINFOMSG_LEN), (RTM_GETADDR, IFADDRMSG_LEN), (RTM_GETROUTE, RTMSG_LEN)]
        .into_iter()
        .enumerate()
    {
        dump(&fd, msg_type, body_len, seq as u32 + 1, &mut snapshot)?;
    }
    Ok(snapshot)
}

fn dump(fd: &NetlinkFd, msg_type: u16, body_len: usize, seq: u32, snapshot: &mut NetworkSnapshot) -> Result<(), String> {
    // nlmsghdr + a zeroed family-specific header (family = AF_UNSPEC).
    let total = NLMSG_HDRLEN + body_len;
    let mut req = vec![0u8; total];
    req[0..4].copy_from_slice(&(total as u32).to_ne_bytes());
    req[4..6].copy_from_slice(&msg_type.to_ne_bytes());
    req[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    req[8..12].copy_from_slice(&seq.to_ne_bytes());
    let kernel = SockaddrNl { nl_family: libc::AF_NETLINK as libc::sa_family_t, nl_pad: 0, nl_pid: 0, nl_groups: 0 };
    // SAFETY: valid fd; `req` and `kernel` are fully initialized and their
    // sizes are passed alongside them.
    let sent = unsafe {
        libc::sendto(
            fd.0,
            req.as_ptr() as *const libc::c_void,
            req.len(),
            0,
            &kernel as *const SockaddrNl as *const libc::sockaddr,
            std::mem::size_of::<SockaddrNl>() as libc::socklen_t,
        )
    };
    if sent < 0 {
        return Err(format!("netlink dump request failed: {}", std::io::Error::last_os_error()));
    }
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: valid fd and writable buffer of the given length.
        let n = unsafe { libc::recv(fd.0, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n <= 0 {
            return Err(format!("netlink dump read failed: {}", std::io::Error::last_os_error()));
        }
        if parse_dump(&buf[..n as usize], seq, snapshot)? {
            return Ok(());
        }
    }
}

/// Parses one datagram of a dump response into `snapshot`. Returns `true`
/// once `NLMSG_DONE` for `seq` has been seen. Messages with another `seq`
/// (e.g. a stray multicast notification) are skipped.
fn parse_dump(mut data: &[u8], seq: u32, snapshot: &mut NetworkSnapshot) -> Result<bool, String> {
    while data.len() >= NLMSG_HDRLEN {
        let len = u32::from_ne_bytes(data[0..4].try_into().unwrap()) as usize;
        if len < NLMSG_HDRLEN || len > data.len() {
            return Err(format!("malformed netlink message length {len}"));
        }
        let msg_type = u16::from_ne_bytes(data[4..6].try_into().unwrap());
        let msg_seq = u32::from_ne_bytes(data[8..12].try_into().unwrap());
        let body = &data[NLMSG_HDRLEN..len];
        if msg_seq == seq {
            match msg_type {
                NLMSG_DONE => return Ok(true),
                NLMSG_ERROR => {
                    let errno = body.get(0..4).map(|b| i32::from_ne_bytes(b.try_into().unwrap())).unwrap_or(0);
                    if errno != 0 {
                        return Err(format!("netlink dump error {}", std::io::Error::from_raw_os_error(-errno)));
                    }
                    return Ok(true);
                }
                RTM_NEWLINK => parse_link(body, snapshot),
                RTM_NEWADDR => parse_addr(body, snapshot),
                RTM_NEWROUTE => parse_route(body, snapshot),
                _ => {}
            }
        }
        let aligned = (len + 3) & !3;
        data = data.get(aligned..).unwrap_or(&[]);
    }
    Ok(false)
}

/// Iterates `rtattr`s (`{u16 len, u16 type, payload}`, 4-byte aligned).
fn attrs(mut data: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        if data.len() < 4 {
            return None;
        }
        let len = u16::from_ne_bytes(data[0..2].try_into().unwrap()) as usize;
        let ty = u16::from_ne_bytes(data[2..4].try_into().unwrap());
        if len < 4 || len > data.len() {
            return None;
        }
        let payload = &data[4..len];
        let aligned = (len + 3) & !3;
        data = data.get(aligned..).unwrap_or(&[]);
        Some((ty & 0x3fff, payload))
    })
}

fn ip_from(payload: &[u8]) -> Option<IpAddr> {
    match payload.len() {
        4 => Some(IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(payload).ok()?))),
        16 => Some(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(payload).ok()?))),
        _ => None,
    }
}

fn parse_link(body: &[u8], snapshot: &mut NetworkSnapshot) {
    if body.len() < IFINFOMSG_LEN {
        return;
    }
    let index = i32::from_ne_bytes(body[4..8].try_into().unwrap());
    let flags = u32::from_ne_bytes(body[8..12].try_into().unwrap());
    if index > 0 && flags & IFF_LOOPBACK == 0 && flags & IFF_RUNNING != 0 {
        snapshot.running.insert(index as u32);
    }
}

fn parse_addr(body: &[u8], snapshot: &mut NetworkSnapshot) {
    if body.len() < IFADDRMSG_LEN {
        return;
    }
    let scope = body[3];
    let index = u32::from_ne_bytes(body[4..8].try_into().unwrap());
    if scope >= RT_SCOPE_LINK {
        return;
    }
    let mut address = None;
    let mut local = None;
    for (ty, payload) in attrs(&body[IFADDRMSG_LEN..]) {
        match ty {
            IFA_ADDRESS => address = ip_from(payload),
            IFA_LOCAL => local = ip_from(payload),
            _ => {}
        }
    }
    // For point-to-point IPv4 links IFA_ADDRESS is the peer; IFA_LOCAL is ours.
    if let Some(addr) = local.or(address) {
        if is_trackable_address(&addr) {
            snapshot.addresses.insert((index, addr));
        }
    }
}

fn parse_route(body: &[u8], snapshot: &mut NetworkSnapshot) {
    if body.len() < RTMSG_LEN {
        return;
    }
    let dst_len = body[1];
    let mut table = body[4] as u32;
    let route_type = body[7];
    if dst_len != 0 || route_type != RTN_UNICAST {
        return;
    }
    let mut oif = None;
    let mut gateway = None;
    let mut nexthops = Vec::new();
    for (ty, payload) in attrs(&body[RTMSG_LEN..]) {
        match ty {
            RTA_TABLE if payload.len() >= 4 => table = u32::from_ne_bytes(payload[0..4].try_into().unwrap()),
            RTA_OIF if payload.len() >= 4 => oif = Some(u32::from_ne_bytes(payload[0..4].try_into().unwrap())),
            RTA_GATEWAY => gateway = ip_from(payload),
            RTA_MULTIPATH => nexthops = parse_multipath(payload),
            _ => {}
        }
    }
    if table != RT_TABLE_MAIN {
        return;
    }
    match (oif, nexthops.is_empty()) {
        (Some(oif), _) => {
            snapshot.default_routes.insert((oif, gateway));
        }
        // A route using a nexthop object (RTA_NH_ID) carries neither; still
        // record that a default route exists so its appearance/disappearance
        // counts.
        (None, true) => {
            snapshot.default_routes.insert((0, gateway));
        }
        (None, false) => {}
    }
    for hop in nexthops {
        snapshot.default_routes.insert(hop);
    }
}

/// `RTA_MULTIPATH`: a sequence of `rtnexthop {u16 len, u8 flags, u8 hops,
/// i32 ifindex}` each followed by its own attributes.
fn parse_multipath(mut data: &[u8]) -> Vec<(u32, Option<IpAddr>)> {
    let mut hops = Vec::new();
    while data.len() >= RTNEXTHOP_LEN {
        let len = u16::from_ne_bytes(data[0..2].try_into().unwrap()) as usize;
        if len < RTNEXTHOP_LEN || len > data.len() {
            break;
        }
        let ifindex = i32::from_ne_bytes(data[4..8].try_into().unwrap());
        let gateway = attrs(&data[RTNEXTHOP_LEN..len]).find(|(ty, _)| *ty == RTA_GATEWAY).and_then(|(_, p)| ip_from(p));
        if ifindex > 0 {
            hops.push((ifindex as u32, gateway));
        }
        let aligned = (len + 3) & !3;
        data = data.get(aligned..).unwrap_or(&[]);
    }
    hops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nlmsg(msg_type: u16, seq: u32, body: &[u8]) -> Vec<u8> {
        let len = NLMSG_HDRLEN + body.len();
        let mut m = vec![0u8; len];
        m[0..4].copy_from_slice(&(len as u32).to_ne_bytes());
        m[4..6].copy_from_slice(&msg_type.to_ne_bytes());
        m[8..12].copy_from_slice(&seq.to_ne_bytes());
        m[NLMSG_HDRLEN..].copy_from_slice(body);
        while m.len() % 4 != 0 {
            m.push(0);
        }
        m
    }

    fn rtattr(ty: u16, payload: &[u8]) -> Vec<u8> {
        let len = 4 + payload.len();
        let mut a = Vec::new();
        a.extend_from_slice(&(len as u16).to_ne_bytes());
        a.extend_from_slice(&ty.to_ne_bytes());
        a.extend_from_slice(payload);
        while a.len() % 4 != 0 {
            a.push(0);
        }
        a
    }

    #[test]
    fn parses_links_addresses_and_default_routes() {
        let mut data = Vec::new();
        // eth0 (index 2) running; lo (index 1) loopback; veth (index 9) down.
        let link = |index: i32, flags: u32| {
            let mut b = vec![0u8; IFINFOMSG_LEN];
            b[4..8].copy_from_slice(&index.to_ne_bytes());
            b[8..12].copy_from_slice(&flags.to_ne_bytes());
            b
        };
        data.extend(nlmsg(RTM_NEWLINK, 7, &link(2, IFF_RUNNING | 0x1)));
        data.extend(nlmsg(RTM_NEWLINK, 7, &link(1, IFF_RUNNING | IFF_LOOPBACK)));
        data.extend(nlmsg(RTM_NEWLINK, 7, &link(9, 0x1)));
        // Global and link-scope addresses on eth0.
        let addr = |index: u32, scope: u8, ip: &[u8]| {
            let mut b = vec![0u8; IFADDRMSG_LEN];
            b[3] = scope;
            b[4..8].copy_from_slice(&index.to_ne_bytes());
            b.extend(rtattr(IFA_LOCAL, ip));
            b
        };
        data.extend(nlmsg(RTM_NEWADDR, 7, &addr(2, 0, &[192, 168, 1, 20])));
        data.extend(nlmsg(RTM_NEWADDR, 7, &addr(2, RT_SCOPE_LINK, &[169, 254, 0, 1])));
        // Default route via eth0 in main, a default in another table, and a
        // non-default route.
        let route = |dst_len: u8, table: u8, oif: u32| {
            let mut b = vec![0u8; RTMSG_LEN];
            b[1] = dst_len;
            b[4] = table;
            b[7] = RTN_UNICAST;
            b.extend(rtattr(RTA_OIF, &oif.to_ne_bytes()));
            b.extend(rtattr(RTA_GATEWAY, &[192, 168, 1, 1]));
            b
        };
        data.extend(nlmsg(RTM_NEWROUTE, 7, &route(0, RT_TABLE_MAIN as u8, 2)));
        data.extend(nlmsg(RTM_NEWROUTE, 7, &route(0, 100, 5)));
        data.extend(nlmsg(RTM_NEWROUTE, 7, &route(24, RT_TABLE_MAIN as u8, 2)));
        // A message for another sequence number is ignored.
        data.extend(nlmsg(RTM_NEWLINK, 99, &link(3, IFF_RUNNING)));
        data.extend(nlmsg(NLMSG_DONE, 7, &[0u8; 4]));

        let mut s = NetworkSnapshot::default();
        assert!(parse_dump(&data, 7, &mut s).unwrap());
        assert_eq!(s.running.iter().copied().collect::<Vec<u32>>(), vec![2u32]);
        let addrs: Vec<(u32, IpAddr)> = s.addresses.iter().cloned().collect();
        assert_eq!(addrs, vec![(2u32, IpAddr::from([192, 168, 1, 20]))]);
        let routes: Vec<(u32, Option<IpAddr>)> = s.default_routes.iter().cloned().collect();
        assert_eq!(routes, vec![(2u32, Some(IpAddr::from([192, 168, 1, 1])))]);
    }

    #[test]
    fn incomplete_dump_is_not_done_and_malformed_lengths_are_errors() {
        let mut s = NetworkSnapshot::default();
        assert!(!parse_dump(&nlmsg(RTM_NEWLINK, 1, &[0u8; IFINFOMSG_LEN]), 1, &mut s).unwrap());
        let mut bad = nlmsg(RTM_NEWLINK, 1, &[0u8; IFINFOMSG_LEN]);
        bad[0..4].copy_from_slice(&1000u32.to_ne_bytes());
        assert!(parse_dump(&bad, 1, &mut s).is_err());
    }

    #[test]
    fn a_real_snapshot_can_be_taken_without_privileges() {
        take_snapshot().expect("netlink dumps must not require root");
    }

    #[test]
    fn new_succeeds_and_drop_completes_promptly_without_a_real_network_event() {
        let started = std::time::Instant::now();
        let monitor = LinuxNetworkChangeMonitor::new().expect(
            "binding an AF_NETLINK/NETLINK_ROUTE socket to read-only multicast groups \
             must not require root (same as `ip monitor`)",
        );
        drop(monitor);
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(2), "Drop must not block: took {elapsed:?}");
    }

    #[tokio::test]
    async fn next_change_does_not_resolve_spuriously_within_a_short_window() {
        let mut monitor = LinuxNetworkChangeMonitor::new().expect("socket setup must succeed in this sandbox");
        tokio::select! {
            _ = monitor.next_change() => panic!(
                "no real network change happened during this test; next_change() must not resolve on its own"
            ),
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
    }
}
