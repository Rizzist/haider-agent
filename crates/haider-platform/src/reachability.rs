//! Conservative OS route-state attribution for provider blame clocks.
//!
//! This seam never performs a DNS lookup, ping, HTTP request, or any other
//! network probe. macOS reads `SCNetworkReachability` flags for the default
//! route, Linux asks the kernel route table with `RTM_GETROUTE`, Windows reads
//! Network List Manager connectivity, and Android reads `ConnectivityManager`
//! when its host has initialized `ndk-context` (otherwise the result is
//! `Unknown`).
//!
//! Only the negative is authoritative: [`RouteStatus::Unavailable`] means the
//! OS has positively reported that no usable route exists, so a provider idle
//! clock may pause without blaming the provider for the host's lost link.
//! `Available` does not claim that the provider, Internet, DNS, TLS, proxy, or
//! captive portal works, and every query failure becomes `Unknown`. Both
//! `Available` and `Unknown` therefore keep provider clocks counting down.
//! Keeping that asymmetry is essential: replacing this source with an active
//! probe could falsely pause a healthy run behind a captive portal, a slow
//! upstream, or a probe-specific outage.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const ROUTE_STATUS_CACHE_TTL: Duration = Duration::from_millis(250);

/// Conservative result of one local OS link/route inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteStatus {
    /// The OS currently exposes at least one usable route. The remote provider
    /// may still be unreachable, so blame clocks continue to run.
    Available,
    /// The OS positively reports no usable route. Provider blame clocks may
    /// pause, but an enclosing absolute run deadline must remain armed.
    Unavailable,
    /// The platform query was unavailable or inconclusive. Conservative
    /// fallback: provider blame clocks continue to run.
    Unknown,
}

#[derive(Debug, Clone, Copy)]
struct CachedRouteStatus {
    checked_at: Instant,
    status: RouteStatus,
}

/// Returns a short-lived cached view of the OS route state.
///
/// The cache keeps several simultaneous provider streams from repeatedly
/// opening the same kernel/COM route seam. A poisoned cache cannot create a
/// false pause: the query runs directly and an inconclusive result is
/// [`RouteStatus::Unknown`].
#[must_use]
pub fn route_status() -> RouteStatus {
    static CACHE: OnceLock<Mutex<Option<CachedRouteStatus>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(cached) = cache.lock()
        && let Some(cached) = *cached
        && cached.checked_at.elapsed() < ROUTE_STATUS_CACHE_TTL
    {
        return cached.status;
    }

    let status = platform_route_status();
    if let Ok(mut cached) = cache.lock() {
        *cached = Some(CachedRouteStatus {
            checked_at: Instant::now(),
            status,
        });
    }
    status
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn platform_route_status() -> RouteStatus {
    use std::ffi::c_void;

    type ReachabilityRef = *const c_void;
    type ReachabilityFlags = u32;

    #[link(name = "SystemConfiguration", kind = "framework")]
    unsafe extern "C" {
        fn SCNetworkReachabilityCreateWithAddress(
            allocator: *const c_void,
            address: *const libc::sockaddr,
        ) -> ReachabilityRef;
        fn SCNetworkReachabilityGetFlags(
            target: ReachabilityRef,
            flags: *mut ReachabilityFlags,
        ) -> u8;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: *const c_void);
    }

    const REACHABLE: ReachabilityFlags = 1 << 1;
    let check_address = |address: *const libc::sockaddr| {
        // SAFETY: every caller supplies a fully initialized sockaddr that
        // lives through this call. CoreFoundation owns the returned reference
        // until CFRelease below.
        let target = unsafe { SCNetworkReachabilityCreateWithAddress(std::ptr::null(), address) };
        if target.is_null() {
            return None;
        }
        let mut flags = 0;
        // SAFETY: `target` is a live create-rule reference and `flags` is writable.
        let succeeded = unsafe { SCNetworkReachabilityGetFlags(target, &mut flags) };
        // SAFETY: create-rule reference is released exactly once after its last use.
        unsafe { CFRelease(target) };
        (succeeded != 0).then_some(flags & REACHABLE != 0)
    };
    let ipv4 = libc::sockaddr_in {
        sin_len: u8::try_from(std::mem::size_of::<libc::sockaddr_in>()).unwrap_or(u8::MAX),
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr { s_addr: 0 },
        sin_zero: [0; 8],
    };
    let ipv6 = libc::sockaddr_in6 {
        sin6_len: u8::try_from(std::mem::size_of::<libc::sockaddr_in6>()).unwrap_or(u8::MAX),
        sin6_family: libc::AF_INET6 as libc::sa_family_t,
        sin6_port: 0,
        sin6_flowinfo: 0,
        sin6_addr: libc::in6_addr { s6_addr: [0; 16] },
        sin6_scope_id: 0,
    };
    let ipv4 = check_address(std::ptr::from_ref(&ipv4).cast());
    let ipv6 = check_address(std::ptr::from_ref(&ipv6).cast());
    match (ipv4, ipv6) {
        (Some(true), _) | (_, Some(true)) => RouteStatus::Available,
        (Some(false), Some(false)) => RouteStatus::Unavailable,
        // A definitive negative requires both address-family queries to
        // succeed. One failed query could conceal a healthy single-stack path.
        _ => RouteStatus::Unknown,
    }
}

#[cfg(all(target_os = "linux", not(target_os = "android")))]
#[allow(unsafe_code)]
fn platform_route_status() -> RouteStatus {
    use std::mem::size_of;

    const NETLINK_ROUTE: libc::c_int = 0;
    const RTM_GETROUTE: u16 = 26;
    const RTM_NEWROUTE: u16 = 24;
    const NLMSG_ERROR: u16 = 2;
    const NLMSG_DONE: u16 = 3;
    const NLMSG_OVERRUN: u16 = 4;
    const NLM_F_REQUEST: u16 = 1;
    const NLM_F_DUMP_INTR: u16 = 0x10;
    const NLM_F_DUMP: u16 = 0x300;
    const RTN_UNICAST: u8 = 1;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NetlinkHeader {
        length: u32,
        message_type: u16,
        flags: u16,
        sequence: u32,
        port_id: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RouteMessage {
        family: u8,
        destination_prefix_len: u8,
        source_prefix_len: u8,
        tos: u8,
        table: u8,
        protocol: u8,
        scope: u8,
        kind: u8,
        flags: u32,
    }

    #[repr(C)]
    struct RouteRequest {
        header: NetlinkHeader,
        route: RouteMessage,
    }

    const fn aligned(length: usize) -> usize {
        (length + 3) & !3
    }

    // SAFETY: libc socket returns a fresh descriptor or -1; every successful
    // descriptor is closed exactly once below.
    let socket = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, NETLINK_ROUTE) };
    if socket < 0 {
        return RouteStatus::Unknown;
    }
    let request = RouteRequest {
        header: NetlinkHeader {
            length: u32::try_from(size_of::<RouteRequest>()).unwrap_or(u32::MAX),
            message_type: RTM_GETROUTE,
            flags: NLM_F_REQUEST | NLM_F_DUMP,
            sequence: 1,
            port_id: 0,
        },
        route: RouteMessage {
            family: libc::AF_UNSPEC as u8,
            destination_prefix_len: 0,
            source_prefix_len: 0,
            tos: 0,
            table: 0,
            protocol: 0,
            scope: 0,
            kind: 0,
            flags: 0,
        },
    };
    // SAFETY: zero is a valid kernel netlink address; the public fields below
    // direct this datagram to the kernel (pid/groups zero).
    let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    kernel.nl_pid = 0;
    kernel.nl_groups = 0;
    // SAFETY: request is a valid initialized byte region of the supplied size,
    // and `kernel` is a fully initialized AF_NETLINK destination.
    let sent = unsafe {
        libc::sendto(
            socket,
            std::ptr::from_ref(&request).cast(),
            size_of::<RouteRequest>(),
            0,
            std::ptr::from_ref(&kernel).cast(),
            u32::try_from(size_of::<libc::sockaddr_nl>()).unwrap_or(u32::MAX),
        )
    };
    if sent != isize::try_from(size_of::<RouteRequest>()).unwrap_or(-1) {
        // SAFETY: descriptor is owned by this function.
        unsafe { libc::close(socket) };
        return RouteStatus::Unknown;
    }

    let receive_timeout = libc::timeval {
        tv_sec: 0,
        tv_usec: 100_000,
    };
    // SAFETY: the timeout is a valid timeval byte region. A failure makes the
    // query inconclusive instead of risking a blocking runtime thread.
    if unsafe {
        libc::setsockopt(
            socket,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            std::ptr::from_ref(&receive_timeout).cast(),
            u32::try_from(size_of::<libc::timeval>()).unwrap_or(u32::MAX),
        )
    } != 0
    {
        // SAFETY: descriptor is owned by this function.
        unsafe { libc::close(socket) };
        return RouteStatus::Unknown;
    }

    let mut buffer = [0_u8; 16 * 1024];
    let mut usable_route = false;
    let mut completed = false;
    let mut inconclusive = false;
    loop {
        let mut sender_length = u32::try_from(size_of::<libc::sockaddr_nl>()).unwrap_or(u32::MAX);
        // SAFETY: buffer is writable for its full advertised length. `kernel`
        // is no longer needed as the send destination and is writable storage
        // for the source address; `sender_length` advertises its exact size.
        let received = unsafe {
            libc::recvfrom(
                socket,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::MSG_TRUNC,
                (&raw mut kernel).cast(),
                &raw mut sender_length,
            )
        };
        if received < 0 {
            break;
        }
        if received == 0 {
            break;
        }
        let received = usize::try_from(received).unwrap_or_default();
        if received > buffer.len() {
            // MSG_TRUNC reports the full datagram length. Parsing the prefix
            // could miss the only usable route and invent an Unavailable.
            inconclusive = true;
            break;
        }
        if usize::try_from(sender_length).unwrap_or_default() != size_of::<libc::sockaddr_nl>()
            || kernel.nl_family != libc::AF_NETLINK as libc::sa_family_t
            || kernel.nl_pid != 0
            || kernel.nl_groups != 0
        {
            // Only a complete source address identifying the kernel may make
            // a route-table dump authoritative.
            inconclusive = true;
            break;
        }
        let mut offset = 0;
        while received.saturating_sub(offset) >= size_of::<NetlinkHeader>() {
            // SAFETY: the bounds check covers the fixed header.
            let header = unsafe {
                std::ptr::read_unaligned(buffer.as_ptr().add(offset).cast::<NetlinkHeader>())
            };
            let length = usize::try_from(header.length).unwrap_or_default();
            if length < size_of::<NetlinkHeader>() || length > received - offset {
                inconclusive = true;
                break;
            }
            if header.sequence != request.header.sequence || header.flags & NLM_F_DUMP_INTR != 0 {
                // A mismatched reply is not ours. DUMP_INTR explicitly means
                // objects may be missing, so it can never support Unavailable.
                inconclusive = true;
                break;
            }
            match header.message_type {
                NLMSG_DONE => {
                    let payload_length = length - size_of::<NetlinkHeader>();
                    if payload_length == 0 {
                        completed = true;
                    } else if payload_length >= size_of::<i32>() {
                        let payload_start = offset + size_of::<NetlinkHeader>();
                        let error = buffer
                            .get(payload_start..payload_start + size_of::<i32>())
                            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
                            .map(i32::from_ne_bytes);
                        if error == Some(0) {
                            completed = true;
                        } else {
                            inconclusive = true;
                            break;
                        }
                    } else {
                        inconclusive = true;
                        break;
                    }
                }
                NLMSG_ERROR | NLMSG_OVERRUN => {
                    inconclusive = true;
                    break;
                }
                RTM_NEWROUTE
                    if length >= size_of::<NetlinkHeader>() + size_of::<RouteMessage>() =>
                {
                    // SAFETY: message length covers the route payload.
                    let route = unsafe {
                        std::ptr::read_unaligned(
                            buffer
                                .as_ptr()
                                .add(offset + size_of::<NetlinkHeader>())
                                .cast::<RouteMessage>(),
                        )
                    };
                    // Presence is intentionally broader than a single RTA_OIF:
                    // valid multipath/policy routes can encode their output in
                    // nested attributes, and administrators may place usable
                    // policy routes in table 255. Any unicast route is enough
                    // to keep counting. This source may conservatively say
                    // Available, but must never invent a route-down pause.
                    if route.kind == RTN_UNICAST {
                        usable_route = true;
                    }
                }
                _ => {}
            }
            offset = offset.saturating_add(aligned(length));
        }
        if offset < received {
            // A trailing fragment too short to hold another header means the
            // dump was truncated or malformed. Never convert skipped route
            // data into an authoritative negative.
            inconclusive = true;
        }
        if completed || inconclusive {
            break;
        }
    }
    // SAFETY: descriptor is owned by this function and no longer used.
    unsafe { libc::close(socket) };
    if !completed || inconclusive {
        RouteStatus::Unknown
    } else if usable_route {
        RouteStatus::Available
    } else {
        RouteStatus::Unavailable
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn platform_route_status() -> RouteStatus {
    use windows::Win32::Networking::NetworkListManager::{
        INetworkListManager, NLM_CONNECTIVITY_DISCONNECTED, NetworkListManager,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::core::HRESULT;

    const RPC_E_CHANGED_MODE: HRESULT = HRESULT(0x8001_0106_u32 as i32);
    // SAFETY: COM is initialized for this thread for the duration of the local
    // interface use. A pre-existing apartment is retained on CHANGED_MODE.
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if initialized.is_err() && initialized != RPC_E_CHANGED_MODE {
        return RouteStatus::Unknown;
    }
    // SAFETY: the registered Network List Manager coclass supplies the
    // requested interface; the smart pointer releases it before uninitialize.
    let manager = unsafe {
        CoCreateInstance::<_, INetworkListManager>(&NetworkListManager, None, CLSCTX_ALL)
    };
    let status = manager
        .and_then(|manager| unsafe { manager.GetConnectivity() })
        .map_or(RouteStatus::Unknown, |connectivity| {
            if connectivity == NLM_CONNECTIVITY_DISCONNECTED {
                RouteStatus::Unavailable
            } else {
                RouteStatus::Available
            }
        });
    if initialized.is_ok() {
        // SAFETY: paired with the successful CoInitializeEx above on this thread.
        unsafe { CoUninitialize() };
    }
    status
}

#[cfg(target_os = "android")]
#[allow(unsafe_code)]
fn platform_route_status() -> RouteStatus {
    use jni::JavaVM;
    use jni::objects::{JObject, JValue};

    let Ok(context) = std::panic::catch_unwind(ndk_context::android_context) else {
        return RouteStatus::Unknown;
    };
    if context.vm().is_null() || context.context().is_null() {
        return RouteStatus::Unknown;
    }
    // SAFETY: ndk-context is initialized by the Android runtime and owns both
    // pointers for the lifetime of the activity. JavaVM only borrows the VM.
    let Ok(vm) = (unsafe { JavaVM::from_raw(context.vm().cast()) }) else {
        return RouteStatus::Unknown;
    };
    let Ok(mut environment) = vm.attach_current_thread() else {
        return RouteStatus::Unknown;
    };
    // Do not disturb an exception already owned by an embedding Java caller.
    if environment.exception_check() != Ok(false) {
        return RouteStatus::Unknown;
    }
    let status =
        environment.with_local_frame(8, |environment| -> jni::errors::Result<RouteStatus> {
            let Ok(service_name) = environment.new_string("connectivity") else {
                return Ok(RouteStatus::Unknown);
            };
            let service_name = JObject::from(service_name);
            // SAFETY: a valid attached JNIEnv exposes a valid JNI function table.
            // ndk-context's runtime-owned context reference remains live under its
            // activity-lifetime contract. NewLocalRef accepts that shared reference
            // and returns a unique, thread-local reference; NULL is checked before
            // the reference is wrapped exactly once. The enclosing local frame
            // deletes this local copy and every local returned below.
            let application_context = unsafe {
                let native_interface = environment.get_native_interface();
                let Some(new_local_ref) = (**native_interface).NewLocalRef else {
                    return Ok(RouteStatus::Unknown);
                };
                let local = new_local_ref(native_interface, context.context().cast());
                if local.is_null() {
                    return Ok(RouteStatus::Unknown);
                }
                JObject::from_raw(local)
            };
            let Ok(manager) = environment.call_method(
                &application_context,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&service_name)],
            ) else {
                return Ok(RouteStatus::Unknown);
            };
            let Ok(manager) = manager.l() else {
                return Ok(RouteStatus::Unknown);
            };
            if manager.is_null() {
                return Ok(RouteStatus::Unknown);
            }
            let Ok(network) = environment.call_method(
                manager,
                "getActiveNetwork",
                "()Landroid/net/Network;",
                &[],
            ) else {
                return Ok(RouteStatus::Unknown);
            };
            Ok(match network.l() {
                Ok(network) if network.is_null() => RouteStatus::Unavailable,
                Ok(_) => RouteStatus::Available,
                Err(_) => RouteStatus::Unknown,
            })
        });
    // JNI leaves Java exceptions pending. None existed on entry, so clear any
    // exception raised by this optional inspection before returning to a thread
    // that may already have been attached by the host runtime.
    let _ = environment.exception_clear();
    status.unwrap_or(RouteStatus::Unknown)
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    windows
)))]
fn platform_route_status() -> RouteStatus {
    RouteStatus::Unknown
}

#[cfg(test)]
mod tests {
    use super::RouteStatus;

    #[test]
    fn route_status_has_a_conservative_unknown_state() {
        assert_ne!(RouteStatus::Unknown, RouteStatus::Unavailable);
        assert_ne!(RouteStatus::Available, RouteStatus::Unavailable);
    }
}
