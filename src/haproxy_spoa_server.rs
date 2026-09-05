use haproxy_spoe::{Agent, Scope, TypedData};
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use crate::firehol_ip_reputation::Mmdb;
use anyhow::Result;
use lazy_static::lazy_static;
use prometheus::{IntCounter, IntCounterVec, register_int_counter, register_int_counter_vec};
use tokio::net::TcpListener;

lazy_static! {
    pub static ref FIREHOL_IP_BLOCKED_REQUESTS: IntCounterVec = register_int_counter_vec!(
        "firehol_ip_blocked_requests",
        "number of blocked ip requests",
        &["file_name", "maintainer", "category"]
    )
    .unwrap();
    pub static ref FIREHOL_IP_ALLOWES_REQUESTS: IntCounter = register_int_counter!(
        "firehol_ip_allowed_requests",
        "number of allowed ip requests"
    )
    .unwrap();
}

pub struct HaproxySpoaServer {
    mmdb: Arc<Mmdb>,
    addr: String,
    drop_by_categories: Arc<HashSet<String>>,
    drop_by_file_names: Arc<HashSet<String>>,
}

impl HaproxySpoaServer {
    pub fn new(
        mmdb: Arc<Mmdb>,
        addr: String,
        drop_by_categories: HashSet<String>,
        drop_by_file_names: HashSet<String>,
    ) -> Self {
        Self {
            mmdb,
            addr,
            drop_by_categories: Arc::new(drop_by_categories),
            drop_by_file_names: Arc::new(drop_by_file_names),
        }
    }

    pub async fn start(&self) -> Result<()> {
        let listener = TcpListener::bind(&self.addr).await?;
        info!("spoa listening on {}", self.addr);

        let mmdb = Arc::clone(&self.mmdb);
        let drop_by_categories = Arc::clone(&self.drop_by_categories);
        let drop_by_file_names = Arc::clone(&self.drop_by_file_names);

        let agent = Agent::new(move |req| {
            let Some(msg) = req.get_message("check-client-ip") else {
                error!("unknown SPOE message {:?}", req.messages);
                return;
            };

            let ip = match msg.get("ip") {
                Some(TypedData::IPv4(ip)) => IpAddr::V4(*ip),
                Some(TypedData::IPv6(ip)) => IpAddr::V6(*ip),
                Some(TypedData::String(ip)) => match ip.parse::<IpAddr>() {
                    Ok(ip) => ip,
                    Err(err) => {
                        warn!("error when parsing IP {ip}: {err:?}");
                        return;
                    }
                },
                msg => {
                    warn!("unimplemented message type: {msg:?}");
                    return;
                }
            };

            let should_drop = match mmdb.lookup(ip) {
                None => {
                    debug!("ip {ip} not found in mmdb file ");
                    false
                }
                Some(record) => {
                    debug!(?record, "ip {ip} found in mmdb file");

                    record.category.iter().enumerate().any(|(index, category)| {
                        match drop_by_categories.contains(category.as_str()) {
                            true => {
                                let file_name = record
                                    .file_name
                                    .get(index)
                                    .map(String::as_str)
                                    .unwrap_or("unknown");
                                let maintainer = record
                                    .maintainer
                                    .get(index)
                                    .map(String::as_str)
                                    .unwrap_or("unknown");
                                let source_file_date = record
                                    .source_file_date_rfc3339
                                    .get(index)
                                    .map(String::as_str)
                                    .unwrap_or("unknown");

                                warn!(
                                    file_name = ?file_name,
                                    maintainer = ?maintainer,
                                    source_file_date = ?source_file_date,
                                    "blocked IP {ip} due to category: \"{category}\"",
                                );
                                FIREHOL_IP_BLOCKED_REQUESTS
                                    .with_label_values(&[file_name, maintainer, category])
                                    .inc();
                                true
                            }
                            false => false,
                        }
                    }) || record
                        .file_name
                        .iter()
                        .enumerate()
                        .any(|(index, file_name)| {
                            match drop_by_file_names.contains(file_name.as_str()) {
                                true => {
                                    let category = record
                                        .category
                                        .get(index)
                                        .map(String::as_str)
                                        .unwrap_or("unknown");
                                    let maintainer = record
                                        .maintainer
                                        .get(index)
                                        .map(String::as_str)
                                        .unwrap_or("unknown");
                                    let source_file_date = record
                                        .source_file_date_rfc3339
                                        .get(index)
                                        .map(String::as_str)
                                        .unwrap_or("unknown");

                                    warn!(
                                        category = ?category,
                                        maintainer = ?maintainer,
                                        source_file_date = ?source_file_date,
                                        "blocked IP {ip} due to file_name: \"{file_name}\""
                                    );
                                    FIREHOL_IP_BLOCKED_REQUESTS
                                        .with_label_values(&[
                                            file_name.as_str(),
                                            maintainer,
                                            category,
                                        ])
                                        .inc();
                                    true
                                }
                                false => false,
                            }
                        })
                }
            };

            if !should_drop {
                FIREHOL_IP_ALLOWES_REQUESTS.inc();
            }

            req.set_var(Scope::Session, "ip_bad", TypedData::Boolean(should_drop));
        });

        agent.serve(listener).await?;

        Ok(())
    }
}
