//! Telegram data centres: which DC a [`Sender`](crate::Sender) talks to,
//! and where it listens.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

use crate::DcAddr;

/// One DC: its id and address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DcOption {
    pub id: i32,
    pub addr: DcAddr,
}

impl DcOption {
    /// A production DC at its static address: `help.getConfig`'s `static`
    /// options, as grammers-session 0.11.0 ships them
    /// (`dc_options.rs` `KNOWN_DC_OPTIONS`). `None` for ids other than 1–5.
    #[must_use]
    pub fn production(id: i32) -> Option<Self> {
        let (v4, v6_group) = match id {
            1 => ([149, 154, 175, 53], [0x2001, 0xb28, 0xf23d, 0xf001]),
            2 => ([149, 154, 167, 41], [0x2001, 0x67c, 0x4e8, 0xf002]),
            3 => ([149, 154, 175, 100], [0x2001, 0xb28, 0xf23d, 0xf003]),
            4 => ([149, 154, 167, 92], [0x2001, 0x67c, 0x4e8, 0xf004]),
            5 => ([91, 108, 56, 104], [0x2001, 0xb28, 0xf23f, 0xf005]),
            _ => return None,
        };
        let [a, b, c, d] = v6_group;
        Some(Self {
            id,
            addr: DcAddr {
                ipv4: SocketAddrV4::new(Ipv4Addr::from(v4), 443),
                ipv6: SocketAddrV6::new(Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0xa), 443, 0, 0),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_table_has_dcs_one_to_five() {
        assert!(DcOption::production(0).is_none());
        assert!(DcOption::production(6).is_none());
        let dc2 = DcOption::production(2).map(|dc| dc.addr.ipv4.to_string());
        assert_eq!(dc2.as_deref(), Some("149.154.167.41:443"));
        for id in 1..=5 {
            assert_eq!(DcOption::production(id).map(|dc| dc.id), Some(id));
        }
    }
}
