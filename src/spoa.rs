//! HAProxy SPOE agent: answers `check-client-ip` messages with the `ip_bad` session variable.

use crate::metrics;
use crate::mmdb::Mmdb;
use anyhow::{Context, Result};
use geoip2::FireholEntry;
use haproxy_spoe::{Agent, Request, Scope, TypedData};
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpSocket};
use tracing::{debug, error, info, warn};

const MESSAGE_NAME: &str = "check-client-ip";
const IP_ARGUMENT: &str = "ip";
const RESULT_VARIABLE: &str = "ip_bad";
const UNKNOWN: &str = "unknown";
/// Pending-connection queue of the SPOE listener (HAProxy opens a handful of connections).
const LISTEN_BACKLOG: u32 = 1024;

/// Decides whether an IP must be dropped, based on the FireHOL lists it appears in.
pub struct IpFilter {
    mmdb: Arc<Mmdb>,
    drop_categories: HashSet<String>,
    drop_file_names: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    Category,
    FileName,
}

/// Position, in the record's parallel arrays, of the list that caused a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Match {
    index: usize,
    reason: Reason,
}

impl IpFilter {
    pub fn new(
        mmdb: Arc<Mmdb>,
        drop_categories: HashSet<String>,
        drop_file_names: HashSet<String>,
    ) -> Self {
        Self {
            mmdb,
            drop_categories,
            drop_file_names,
        }
    }

    /// SPOE handler: reads the `ip` argument of `check-client-ip` and sets `ip_bad`.
    pub fn handle(&self, req: &mut Request) {
        let Some(msg) = req.get_message(MESSAGE_NAME) else {
            error!("unknown SPOE message {:?}", req.messages);
            return;
        };

        let ip = match msg.get(IP_ARGUMENT) {
            Some(TypedData::IPv4(ip)) => IpAddr::V4(*ip),
            Some(TypedData::IPv6(ip)) => IpAddr::V6(*ip),
            Some(TypedData::String(raw)) => match raw.parse() {
                Ok(ip) => ip,
                Err(err) => {
                    warn!("error when parsing IP {raw:?}: {err}");
                    return;
                }
            },
            other => {
                warn!("unimplemented ip argument type: {other:?}");
                return;
            }
        };

        let should_drop = self.should_drop(ip);
        if !should_drop {
            metrics::IP_ALLOWED_REQUESTS.inc();
        }
        req.set_var(
            Scope::Session,
            RESULT_VARIABLE,
            TypedData::Boolean(should_drop),
        );
    }

    /// Returns `true` when `ip` belongs to a dropped category or list file.
    pub fn should_drop(&self, ip: IpAddr) -> bool {
        let verdict = self.mmdb.lookup(ip, |entry| {
            debug!(?entry, "ip {ip} found in mmdb");
            match self.find_match(entry) {
                Some(hit) => {
                    self.record_block(ip, entry, hit);
                    true
                }
                None => false,
            }
        });
        verdict.unwrap_or_else(|| {
            debug!("ip {ip} not found in mmdb");
            false
        })
    }

    /// A category match takes precedence over a file-name match; the first hit wins.
    fn find_match(&self, entry: &FireholEntry<'_>) -> Option<Match> {
        let by_category = entry
            .category
            .iter()
            .position(|category| self.drop_categories.contains(*category))
            .map(|index| Match {
                index,
                reason: Reason::Category,
            });
        by_category.or_else(|| {
            entry
                .file_name
                .iter()
                .position(|file_name| self.drop_file_names.contains(*file_name))
                .map(|index| Match {
                    index,
                    reason: Reason::FileName,
                })
        })
    }

    fn record_block(&self, ip: IpAddr, entry: &FireholEntry<'_>, hit: Match) {
        let file_name = field(&entry.file_name, hit.index);
        let maintainer = field(&entry.maintainer, hit.index);
        let category = field(&entry.category, hit.index);
        let source_file_date = field(&entry.source_file_date_rfc3339, hit.index);

        match hit.reason {
            Reason::Category => warn!(
                file_name,
                maintainer, source_file_date, "blocked IP {ip} due to category: \"{category}\"",
            ),
            Reason::FileName => warn!(
                category,
                maintainer, source_file_date, "blocked IP {ip} due to file_name: \"{file_name}\"",
            ),
        }
        metrics::IP_BLOCKED_REQUESTS
            .with_label_values(&[file_name, maintainer, category])
            .inc();
    }
}

/// Element `index` of a record array, or `"unknown"` when the arrays are not aligned.
fn field<'a>(values: &[&'a str], index: usize) -> &'a str {
    values.get(index).copied().unwrap_or(UNKNOWN)
}

/// Serves the SPOE protocol on `addr` until the listener fails.
pub async fn serve(addr: &str, filter: IpFilter) -> Result<()> {
    let listener = bind(addr).await?;
    info!("spoa listening on {addr}");
    serve_listener(listener, filter).await
}

/// Binds the SPOE listener with `TCP_NODELAY` enabled.
///
/// Accepted connections inherit the option on Linux. Without it, when HAProxy pipelines several
/// NOTIFY frames, Nagle's algorithm holds every ACK segment after the first one until the peer
/// acknowledges the previous segment, which can add up to 40 ms (delayed ACK) per batch.
pub async fn bind(addr: &str) -> Result<TcpListener> {
    let resolved = tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("failed to resolve spoa listen address {addr}"))?
        .next()
        .with_context(|| format!("spoa listen address {addr} resolved to nothing"))?;
    let socket = if resolved.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }
    .context("failed to create spoa listening socket")?;
    socket
        .set_reuseaddr(true)
        .context("failed to set SO_REUSEADDR on the spoa listener")?;
    socket
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY on the spoa listener")?;
    socket
        .bind(resolved)
        .with_context(|| format!("failed to bind spoa listener on {addr}"))?;
    socket
        .listen(LISTEN_BACKLOG)
        .with_context(|| format!("failed to listen on {addr}"))
}

/// Serves the SPOE protocol on an already bound listener until it fails.
pub async fn serve_listener(listener: TcpListener, filter: IpFilter) -> Result<()> {
    Agent::new(move |req| filter.handle(req))
        .serve(listener)
        .await
        .context("spoa server failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{TempDir, build_test_db};
    use std::net::Ipv4Addr;

    fn filter(categories: &[&str], file_names: &[&str], mmdb: Arc<Mmdb>) -> IpFilter {
        IpFilter::new(
            mmdb,
            categories.iter().map(ToString::to_string).collect(),
            file_names.iter().map(ToString::to_string).collect(),
        )
    }

    fn entry<'a>(files: &[&'a str], categories: &[&'a str]) -> FireholEntry<'a> {
        FireholEntry {
            file_name: files.to_vec(),
            category: categories.to_vec(),
            ..FireholEntry::default()
        }
    }

    #[test]
    fn category_match_takes_precedence_over_file_name() {
        let filter = filter(&["spam"], &["a.ipset"], Arc::new(Mmdb::new()));
        let record = entry(&["a.ipset", "b.ipset"], &["abuse", "spam"]);
        assert_eq!(
            filter.find_match(&record),
            Some(Match {
                index: 1,
                reason: Reason::Category
            })
        );
    }

    #[test]
    fn file_name_match_is_used_when_no_category_matches() {
        let filter = filter(&["attacks"], &["b.ipset"], Arc::new(Mmdb::new()));
        let record = entry(&["a.ipset", "b.ipset"], &["abuse", "spam"]);
        assert_eq!(
            filter.find_match(&record),
            Some(Match {
                index: 1,
                reason: Reason::FileName
            })
        );
        assert_eq!(filter.find_match(&entry(&["c.ipset"], &["abuse"])), None);
    }

    #[test]
    fn should_drop_uses_the_loaded_database() {
        let dir = TempDir::new("spoa-drop");
        let mmdb = Arc::new(Mmdb::new());
        mmdb.load(&build_test_db(dir.path())).unwrap();

        let overlap = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
        let abuse_only = IpAddr::V4(Ipv4Addr::new(10, 2, 0, 1));
        let unlisted = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));

        let by_category = filter(&["spam"], &[], Arc::clone(&mmdb));
        assert!(by_category.should_drop(overlap));
        assert!(!by_category.should_drop(abuse_only));
        assert!(!by_category.should_drop(unlisted));

        let by_file = filter(&[], &["abuse.ipset"], Arc::clone(&mmdb));
        assert!(by_file.should_drop(overlap));
        assert!(by_file.should_drop(abuse_only));
        assert!(!by_file.should_drop(unlisted));

        let nothing = filter(&["attacks"], &["other.ipset"], mmdb);
        assert!(!nothing.should_drop(overlap));
    }
}
