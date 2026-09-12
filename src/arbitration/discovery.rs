//! mDNS peer discovery -- advertises this instance and browses for
//! others via the `mdns-sd` crate (pure Rust, does not need
//! `avahi-daemon` installed or running). See the design doc's
//! "Discovery" section for why mDNS over plain UDP broadcast, and for
//! the honest caveat on what it does and doesn't guarantee across
//! subnets.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};

use super::protocol::{PROTO_VERSION, SERVICE_TYPE};

/// Live peer table, kept up to date by draining `mdns-sd`'s event
/// receiver on every poll. Keyed by the service's mDNS fullname
/// (unique per instance) rather than hostname, since two instances
/// could in principle share a hostname (unlikely, but the fullname is
/// what mDNS itself already guarantees unique) -- `peers()` collapses
/// this to the addresses callers actually need.
pub struct Discovery {
    _daemon: ServiceDaemon,
    events: mdns_sd::Receiver<ServiceEvent>,
    known: HashMap<String, SocketAddr>,
    /// This instance's own registered fullname -- see `peers()`'s
    /// self-filtering.
    own_fullname: String,
}

impl Discovery {
    /// Registers this instance's own service and starts browsing for
    /// peers. `hostname` is used both as the advertised instance name
    /// and as the basis for this service's own (distinct-from-the-
    /// system's) mDNS host record -- see the `host_fqdn` comment below
    /// for why it isn't literally `hostname`.
    pub fn start(port: u16, hostname: &str, bind_interface: &str) -> mdns_sd::Result<Self> {
        let daemon = ServiceDaemon::new()?;

        // Multi-homed machines (found live: a desktop with several
        // libvirt bridge interfaces alongside its real Wi-Fi NIC) can
        // have mdns-sd auto-select or fan out across an interface that
        // never actually reaches the LAN the other instance is on --
        // restrict to exactly one when configured, rather than trusting
        // auto-selection across everything the machine has.
        if !bind_interface.is_empty() {
            if let Err(e) = daemon.disable_interface(IfKind::All) {
                tracing::warn!("[arbitration] disable_interface(All) failed: {e}");
            }
            if let Err(e) = daemon.enable_interface(IfKind::Name(bind_interface.to_string())) {
                tracing::warn!(
                    "[arbitration] enable_interface({bind_interface:?}) failed: {e}"
                );
            }
        }

        let instance_name = format!("{hostname}-{}", std::process::id());
        // Deliberately NOT "<hostname>.local." -- avahi-daemon (running
        // on every Omarchy machine already, for other reasons) already
        // owns that exact host record, and a second, independent mDNS
        // stack (this one) probing/announcing the same name is exactly
        // the kind of same-name conflict RFC 6762 tells implementations
        // to back off from -- found live: registration silently
        // produced a service nothing could discover, no error logged,
        // consistent with a probe conflict on the shared hostname
        // record rather than anything wrong with the service record
        // itself. A distinct host label sidesteps the conflict
        // entirely instead of trying to coordinate with avahi-daemon's
        // own registration.
        let host_fqdn = format!("{hostname}-novad-arbitration.local.");
        let properties = [("proto_version", PROTO_VERSION.to_string())];
        // `()` for the address argument means "no addresses at all,"
        // not "auto-detect" -- found live: registering with it produced
        // a service with an empty address set that could never be
        // announced ("no valid addrs on interface X" for every
        // interface, nothing else logged as an error). The actual
        // auto-detect opt-in is this separate builder call, which
        // makes the daemon fill in this machine's own address(es) per
        // interface at registration time (see `register_service` /
        // `is_addr_auto` in mdns-sd's own source).
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name,
            &host_fqdn,
            (),
            port,
            &properties[..],
        )?
        .enable_addr_auto();
        // mDNS has no built-in notion of "don't tell me about my own
        // service" -- browsing sees every instance advertising this
        // service type, this one included. Captured before `register`
        // moves `service`, and compared against every `ServiceResolved`
        // event in `peers()` so this instance never becomes its own
        // "peer" (found live: without this, a machine's peer table
        // included itself, which would have meant announcing to and
        // arbitrating against its own copy of its own announcement).
        let own_fullname = service.get_fullname().to_string();
        daemon.register(service)?;

        let events = daemon.browse(SERVICE_TYPE)?;

        Ok(Self {
            _daemon: daemon,
            events,
            known: HashMap::new(),
            own_fullname,
        })
    }

    /// Drains any discovery events that have arrived since the last
    /// call (non-blocking) and returns the current known peer
    /// addresses. Called once per arbitration round, not continuously
    /// polled -- mDNS resolution happening in the background between
    /// wake-word detections is exactly what keeps this cheap at
    /// detection time.
    pub fn peers(&mut self) -> Vec<SocketAddr> {
        while let Ok(event) = self.events.try_recv() {
            match event {
                ServiceEvent::ServiceResolved(resolved) => {
                    if resolved.fullname == self.own_fullname {
                        continue; // this machine's own advertisement, not a peer
                    }
                    // A resolved service can carry several addresses at
                    // once (IPv4, ULA/link-local IPv6, loopback if the
                    // two instances happen to share a host) -- prefer
                    // IPv4 when there's a choice: link-local IPv6
                    // (`fe80::...`) needs a zone/scope id to actually
                    // route a `send_to` correctly, which a bare
                    // `SocketAddr` can't carry, and plain LAN IPv4 has
                    // no such wrinkle. Falls back to whatever's
                    // available (including link-local) rather than
                    // discarding a peer entirely if IPv4 isn't offered.
                    let chosen = resolved
                        .addresses
                        .iter()
                        .find(|a| a.to_ip_addr().is_ipv4())
                        .or_else(|| resolved.addresses.iter().next());
                    if let Some(addr) = chosen {
                        self.known.insert(
                            resolved.fullname.clone(),
                            SocketAddr::new(addr.to_ip_addr(), resolved.port),
                        );
                    }
                }
                ServiceEvent::ServiceRemoved(_ty, fullname) => {
                    self.known.remove(&fullname);
                }
                _ => {}
            }
        }
        self.known.values().copied().collect()
    }

    /// One-shot blocking wait for the *first* discovery pass to
    /// settle, used only at daemon startup so the very first wake-word
    /// detection after a fresh boot isn't arbitrating against an
    /// empty peer table just because mDNS hasn't had a chance to hear
    /// back yet. Short and best-effort -- if nothing resolves in this
    /// window, `peers()` calls during real detections keep trying on
    /// their own regardless.
    pub fn warm_up(&mut self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            self.peers();
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
