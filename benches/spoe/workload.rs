//! Request mix of `tests/benchmark_spoa.py`: IPv4 or IPv6 with equal probability, sampled
//! uniformly over the whole address space (reserved ranges included), or a fixed list of
//! addresses (`BENCH_IPS=1.2.3.4,2001:db8::1`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Small deterministic PRNG (SplitMix64), so every run replays the same addresses.
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-ish value in `0..bound` (the modulo bias is irrelevant here).
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

pub fn random_ipv4(rng: &mut SplitMix64) -> Ipv4Addr {
    Ipv4Addr::from(rng.next_u64() as u32)
}

pub fn random_ipv6(rng: &mut SplitMix64) -> Ipv6Addr {
    Ipv6Addr::from((u128::from(rng.next_u64()) << 64) | u128::from(rng.next_u64()))
}

/// IPv4 or IPv6 with equal probability, like `getrandbits(1)` in the Python client.
pub fn random_ip(rng: &mut SplitMix64) -> IpAddr {
    if rng.next_u64() & 1 == 1 {
        IpAddr::V6(random_ipv6(rng))
    } else {
        IpAddr::V4(random_ipv4(rng))
    }
}

#[derive(Debug, Clone)]
pub enum Workload {
    Random,
    Fixed(Vec<IpAddr>),
}

impl Workload {
    /// `BENCH_IPS` (comma-separated) selects fixed addresses; otherwise random ones.
    pub fn from_env() -> Self {
        match std::env::var("BENCH_IPS") {
            Ok(list) if !list.trim().is_empty() => Self::Fixed(
                list.split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(|item| {
                        item.parse().unwrap_or_else(|err| {
                            panic!("BENCH_IPS: invalid address {item:?}: {err}")
                        })
                    })
                    .collect(),
            ),
            _ => Self::Random,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Random => "random IPv4/IPv6 (50/50, full address ranges)".to_owned(),
            Self::Fixed(ips) => {
                let list: Vec<String> = ips.iter().map(ToString::to_string).collect();
                format!("fixed: {}", list.join(", "))
            }
        }
    }

    /// Address for request number `sequence` of a worker.
    pub fn next_ip(&self, rng: &mut SplitMix64, sequence: u64) -> IpAddr {
        match self {
            Self::Random => random_ip(rng),
            Self::Fixed(ips) => ips[(sequence % ips.len() as u64) as usize],
        }
    }
}
