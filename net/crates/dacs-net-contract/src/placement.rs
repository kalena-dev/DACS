//! Placement checks for Person 4's compiler pool and Linux compile guests.

use std::net::SocketAddr;

use crate::{
    CompilerEndpoint, CompilerRole, ContractError, MembershipState, NetworkMembership,
    is_overlay_ip,
};

/// Resolve a compile server's registration address against the endpoints
/// netadmin currently publishes (`GET /v1/compiler-endpoints`).
///
/// sccache-dist binds a server's JWT to this exact `public_addr`
/// (`sccache-dist auth generate-jwt-hs256-server-token --server ADDR`). The
/// address must be a private overlay address and a currently published
/// `server` endpoint. A host NAT/LAN address does not make a guest reachable,
/// and overlay IPs change on re-enrollment, so a stale address is refused and
/// its token must be reissued.
pub fn published_compiler_server<'a>(
    endpoints: &'a [CompilerEndpoint],
    public_addr: &str,
) -> Result<&'a CompilerEndpoint, ContractError> {
    let addr: SocketAddr = public_addr
        .trim()
        .parse()
        .map_err(|_| ContractError::Mismatch("compile server address must be IP:port"))?;
    if !is_overlay_ip(addr.ip()) {
        return Err(ContractError::Forbidden(
            "compile server address must be a private overlay address, not a host NAT/LAN or public address",
        ));
    }
    endpoints
        .iter()
        .find(|e| e.role == CompilerRole::Server && e.socket_addr() == addr)
        .ok_or(ContractError::Mismatch(
            "address is not a currently published compile server endpoint; re-enrollment changes the overlay IP, so reissue its address-bound token",
        ))
}

/// A guest (VM / Linux compile environment) must be its own overlay node:
/// joined under its own identity, with a node and overlay addresses distinct
/// from its physical host. Installing the client only on the host, or reaching
/// the guest through host NAT, fails this check.
pub fn check_guest_node(
    physical: &NetworkMembership,
    guest: &NetworkMembership,
) -> Result<(), ContractError> {
    let MembershipState::Joined {
        node_id: guest_node,
        ..
    } = guest.state
    else {
        return Err(ContractError::Mismatch(
            "guest has not joined the overlay as its own node",
        ));
    };
    if guest.device_id == physical.device_id || guest.host_name == physical.host_name {
        return Err(ContractError::Mismatch(
            "guest must have its own device identity and host name",
        ));
    }
    if guest.overlay_ips.is_empty() || !guest.overlay_ips.iter().all(|ip| is_overlay_ip(*ip)) {
        return Err(ContractError::Mismatch(
            "guest reports no overlay address of its own",
        ));
    }
    if let MembershipState::Joined { node_id, .. } = physical.state
        && node_id == guest_node
    {
        return Err(ContractError::Mismatch(
            "guest resolves to its physical host's node",
        ));
    }
    if guest
        .overlay_ips
        .iter()
        .any(|ip| physical.overlay_ips.contains(ip))
    {
        return Err(ContractError::Mismatch(
            "guest shares an overlay address with its physical host",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeviceId, HostName};

    fn ep(role: CompilerRole, ip: &str, port: u16) -> CompilerEndpoint {
        CompilerEndpoint {
            host_name: HostName::parse("compile-guest").unwrap(),
            role,
            overlay_ip: ip.parse().unwrap(),
            port,
        }
    }

    #[test]
    fn server_address_must_be_a_published_overlay_server() {
        let published = [
            ep(CompilerRole::Scheduler, "100.64.0.6", 10600),
            ep(CompilerRole::Server, "100.64.0.6", 10501),
        ];
        assert_eq!(
            published_compiler_server(&published, "100.64.0.6:10501").unwrap(),
            &published[1]
        );
        // Scheduler port on the same node is not a server registration.
        assert!(matches!(
            published_compiler_server(&published, "100.64.0.6:10600"),
            Err(ContractError::Mismatch(_))
        ));
        // Address from before re-enrollment (the WSL run saw .5 -> .6).
        assert!(matches!(
            published_compiler_server(&published, "100.64.0.5:10501"),
            Err(ContractError::Mismatch(_))
        ));
        // Host NAT / LAN forwarding to a guest.
        assert!(matches!(
            published_compiler_server(&published, "192.168.1.20:10501"),
            Err(ContractError::Forbidden(_))
        ));
        assert!(matches!(
            published_compiler_server(&published, "compile-guest:10501"),
            Err(ContractError::Mismatch(_))
        ));
    }

    fn member(device: &str, host: &str, state: MembershipState, ips: &[&str]) -> NetworkMembership {
        NetworkMembership {
            device_id: DeviceId(device.into()),
            host_name: HostName::parse(host).unwrap(),
            state,
            overlay_ips: ips.iter().map(|ip| ip.parse().unwrap()).collect(),
            tags: vec![],
            last_seen: None,
        }
    }

    fn joined(node_id: u64) -> MembershipState {
        MembershipState::Joined {
            node_id,
            online: true,
        }
    }

    #[test]
    fn guest_must_be_its_own_node() {
        let host = member("pc-1", "alice-pc", joined(3), &["100.64.0.3"]);
        let guest = member("vm-1", "compile-guest", joined(7), &["100.64.0.5"]);
        assert_eq!(check_guest_node(&host, &guest), Ok(()));

        // Host not on the overlay itself: the guest can still be its own node.
        let off = member("pc-1", "alice-pc", MembershipState::NotEnrolled, &[]);
        assert_eq!(check_guest_node(&off, &guest), Ok(()));

        let not_joined = member(
            "vm-1",
            "compile-guest",
            MembershipState::KeyExpired { auth_key_id: 9 },
            &[],
        );
        let same_node = member("vm-1", "compile-guest", joined(3), &["100.64.0.5"]);
        let shared_ip = member("vm-1", "compile-guest", joined(7), &["100.64.0.3"]);
        let no_ip = member("vm-1", "compile-guest", joined(7), &[]);
        let nat_ip = member("vm-1", "compile-guest", joined(7), &["192.168.122.10"]);
        let same_host = member("vm-1", "alice-pc", joined(7), &["100.64.0.5"]);
        for bad in [not_joined, same_node, shared_ip, no_ip, nat_ip, same_host] {
            assert!(
                matches!(
                    check_guest_node(&host, &bad),
                    Err(ContractError::Mismatch(_))
                ),
                "{bad:?}"
            );
        }
    }
}
