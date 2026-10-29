//! Which addresses a request to somebody else's URL may reach: the public
//! internet only. One copy for every side that connects to an address a
//! person or an app chose — the api's fetches (`brains::webtext`) and a
//! daemon's `mafold app call`, which runs on its owner's machine, next to
//! their router and whatever listens on localhost.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// One address: may a request to somebody else's URL connect to it?
pub fn ip_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !ipv4_is_private(v4),
        IpAddr::V6(v6) => !ipv6_is_private(v6),
    }
}

/// A URL's host as written: an IP literal by [`ip_is_public`], a name unless
/// it is one of the names that mean "this machine / this network". A name
/// that RESOLVES inside is caught where the connection is made.
pub fn host_is_public(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip_is_public(ip);
    }
    !host.is_empty()
        && host != "localhost"
        && !host.ends_with(".local")
        && !host.ends_with(".internal")
        && !host.ends_with(".localhost")
}

pub fn ipv4_is_private(v4: Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local() // 169.254/16(含云元数据 169.254.169.254)
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || a == 0
        || a >= 240 // 240/4 保留段
        // 100.64/10 CGNAT(阿里云元数据 100.100.100.200 就在里面)
        || (a == 100 && (b & 0xc0) == 0x40)
        // 192.0.0/24 协议分配段、198.18/15 基准测试段:一并当内网拒
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b & 0xfe) == 18)
}

pub fn ipv6_is_private(v6: Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
        return true;
    }
    let s = v6.segments();
    // fc00::/7 唯一本地地址(ULA,含 AWS 元数据 fd00:ec2::254)、fe80::/10 链路本地、
    // fec0::/10 旧站点本地
    if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 || (s[0] & 0xffc0) == 0xfec0 {
        return true;
    }
    // 内嵌 IPv4 的几种写法,都按内嵌的那个 IPv4 判定,别让 [::ffff:127.0.0.1] 这类穿过去:
    // ::ffff:a.b.c.d(IPv4-mapped)与 ::a.b.c.d(IPv4-compatible,to_ipv4 两种都认)
    if let Some(v4) = v6.to_ipv4() {
        return ipv4_is_private(v4);
    }
    // 64:ff9b::/96 NAT64
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return ipv4_is_private(embedded_v4(s[6], s[7]));
    }
    // 2002::/16 6to4:第 2、3 段是那个 IPv4
    if s[0] == 0x2002 {
        return ipv4_is_private(embedded_v4(s[1], s[2]));
    }
    false
}

fn embedded_v4(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_literals_that_mean_inside_are_not_public() {
        for h in ["localhost", "LOCALHOST", "printer.local", "db.internal", "a.localhost", "127.0.0.1", "[::1]", "10.0.0.8", "192.168.1.1", "169.254.169.254", "[::ffff:127.0.0.1]", ""] {
            assert!(!host_is_public(h), "{h}");
        }
        for h in ["board.mafold.app", "8.8.8.8", "[2606:4700::1111]"] {
            assert!(host_is_public(h), "{h}");
        }
    }
}
