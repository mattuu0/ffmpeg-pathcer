"""mdns_discovery.py

Finds the Android receiver's LAN IPv4 address via mDNS/NSD, so the user
doesn't have to look it up manually (Fire TV's network settings screen,
router admin page, `adb shell ip addr`, etc.) and pass it as --dest.

The Android side (android-viewer/tv/.../NsdAdvertiser.kt) registers itself
as an NSD service of type `_latencypoc._udp.` with instance name
`latencypoc-viewer`, advertising the exact UDP port it's listening on for
RTP. This module just resolves that same service back to an address using
`zeroconf`, the same mDNS/DNS-SD implementation this pairs with on the
Android side (Android's NsdManager is backed by the platform's own
mDNSResponder-equivalent, which speaks the same protocol zeroconf does).
"""

import ipaddress
import time

from zeroconf import IPVersion, ServiceBrowser, ServiceStateChange, Zeroconf

SERVICE_TYPE = "_latencypoc._udp.local."
INSTANCE_NAME_PREFIX = "latencypoc-viewer"


class DiscoveryResult:
    def __init__(self, address: str, port: int, name: str):
        self.address = address
        self.port = port
        self.name = name


def discover_receiver(timeout_sec: float = 10.0) -> DiscoveryResult | None:
    """Blocks until the Android receiver's NSD service is found (or timeout),
    returning its first IPv4 address and advertised port."""
    zc = Zeroconf()
    found: list[DiscoveryResult] = []

    def on_state_change(zeroconf: Zeroconf, service_type: str, name: str, state_change: ServiceStateChange) -> None:
        if state_change is not ServiceStateChange.Added:
            return
        if not name.startswith(INSTANCE_NAME_PREFIX):
            return
        info = zeroconf.get_service_info(service_type, name, timeout=3000)
        if info is None:
            return
        # addresses_by_version(V4Only) matches this project's IPv4-only
        # scope -- the sender/receiver pair never needs IPv6 here, and mixed
        # A/AAAA results would otherwise require picking one arbitrarily.
        for raw in info.addresses_by_version(IPVersion.V4Only):
            address = str(ipaddress.ip_address(raw))
            found.append(DiscoveryResult(address=address, port=info.port, name=name))
            break

    browser = ServiceBrowser(zc, SERVICE_TYPE, handlers=[on_state_change])
    try:
        deadline = time.monotonic() + timeout_sec
        while time.monotonic() < deadline and not found:
            time.sleep(0.1)
    finally:
        browser.cancel()
        zc.close()

    return found[0] if found else None
