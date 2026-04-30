use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::IpNet;

use super::limits::MAX_EXPANDED_IPS;

fn push_ip(out: &mut Vec<IpAddr>, seen: &mut HashSet<IpAddr>, ip: IpAddr) -> Result<(), String> {
    if out.len() >= MAX_EXPANDED_IPS {
        return Err(format!(
            "Too many IP addresses after expansion (max {MAX_EXPANDED_IPS})"
        ));
    }
    if seen.insert(ip) {
        out.push(ip);
    }
    Ok(())
}

fn expand_net(net: IpNet, out: &mut Vec<IpAddr>, seen: &mut HashSet<IpAddr>) -> Result<(), String> {
    match net {
        IpNet::V4(n) => {
            for ip in n.hosts() {
                push_ip(out, seen, IpAddr::V4(ip))?;
            }
        }
        IpNet::V6(n) => {
            let prefix = n.prefix_len() as u32;
            let host_bits = 128u32.saturating_sub(prefix);
            if host_bits > 16 {
                return Err(
                    "IPv6 range too large for this tool (max 16 host bits, e.g. /112)".to_string(),
                );
            }
            let count = 1usize
                .checked_shl(host_bits)
                .ok_or_else(|| "IPv6 range overflow".to_string())?;
            if count > MAX_EXPANDED_IPS {
                return Err(format!(
                    "Too many addresses in IPv6 range (max {MAX_EXPANDED_IPS})"
                ));
            }
            let base = u128::from(n.network());
            for i in 0..count as u128 {
                let addr = Ipv6Addr::from(base.wrapping_add(i));
                push_ip(out, seen, IpAddr::V6(addr))?;
            }
        }
    }
    Ok(())
}

fn parse_ipv4_dash_range(token: &str) -> Result<Option<Vec<IpAddr>>, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 4 {
        return Ok(None);
    }
    let a: u8 = parts[0]
        .parse()
        .map_err(|_| format!("Invalid IPv4 token {token}"))?;
    let b: u8 = parts[1]
        .parse()
        .map_err(|_| format!("Invalid IPv4 token {token}"))?;
    let c: u8 = parts[2]
        .parse()
        .map_err(|_| format!("Invalid IPv4 token {token}"))?;
    let last = parts[3];
    let Some((lo_s, hi_s)) = last.split_once('-') else {
        return Ok(None);
    };
    let lo: u8 = lo_s
        .parse()
        .map_err(|_| format!("Invalid range in {token}"))?;
    let hi: u8 = hi_s
        .parse()
        .map_err(|_| format!("Invalid range in {token}"))?;
    if lo > hi {
        return Err(format!("Invalid range in {token}"));
    }
    let count = (hi - lo) as usize + 1;
    if count > MAX_EXPANDED_IPS {
        return Err(format!("Range in {token} exceeds max {MAX_EXPANDED_IPS}"));
    }
    let mut v = Vec::with_capacity(count);
    for d in lo..=hi {
        v.push(IpAddr::V4(Ipv4Addr::new(a, b, c, d)));
    }
    Ok(Some(v))
}

pub fn parse_targets_to_ips(input: &str) -> Result<Vec<IpAddr>, String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for raw in input.split(|c: char| c == ',' || c == '\n' || c == ';') {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }

        if let Ok(net) = token.parse::<IpNet>() {
            expand_net(net, &mut out, &mut seen)?;
            continue;
        }

        if let Ok(ip) = token.parse::<IpAddr>() {
            push_ip(&mut out, &mut seen, ip)?;
            continue;
        }

        if let Some(ips) = parse_ipv4_dash_range(token)? {
            for ip in ips {
                push_ip(&mut out, &mut seen, ip)?;
            }
            continue;
        }

        return Err(format!("Unrecognized target: {token}"));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_ip() {
        let ips = parse_targets_to_ips("10.0.0.1").unwrap();
        assert_eq!(ips.len(), 1);
    }

    #[test]
    fn cidr_v4() {
        let ips = parse_targets_to_ips("10.0.0.0/30").unwrap();
        assert_eq!(ips.len(), 2);
    }

    #[test]
    fn dash_range() {
        let ips = parse_targets_to_ips("192.168.1.1-3").unwrap();
        assert_eq!(ips.len(), 3);
    }
}
