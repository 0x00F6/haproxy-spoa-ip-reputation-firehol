//! Parser for FireHOL `.ipset` / `.netset` files.
//!
//! A file starts with a header of `# Key: value` comments followed by one IPv4 address or CIDR
//! network per line. Parsing works on borrowed slices of the file content: no per-line
//! allocation happens.

use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use mmdb_writer::ipnet::Ipv4Net;
use std::fmt;
use std::net::Ipv4Addr;

/// Header fields stored alongside every network of a file. Missing fields stay empty.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub source_file_date_rfc3339: String,
    pub list_source_url: String,
    pub maintainer_url: String,
    pub maintainer: String,
    pub category: String,
}

/// Networks sharing the same header values. The header normally precedes every network, so a
/// file has a single section; a header line appearing later starts a new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub metadata: Metadata,
    pub networks: Vec<Ipv4Net>,
}

/// A parsed list file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ipset {
    /// Base name of the file, e.g. `firehol_level1.netset`.
    pub file_name: String,
    pub sections: Vec<Section>,
}

impl Ipset {
    pub fn network_count(&self) -> usize {
        self.sections.iter().map(|s| s.networks.len()).sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderKey {
    Category,
    MaintainerUrl,
    Maintainer,
    ListSourceUrl,
    SourceFileDate,
}

impl HeaderKey {
    fn parse(key: &str) -> Option<Self> {
        Some(match key {
            "Category" => Self::Category,
            "Maintainer URL" => Self::MaintainerUrl,
            "Maintainer" => Self::Maintainer,
            "List source URL" => Self::ListSourceUrl,
            "Source File Date" => Self::SourceFileDate,
            _ => return None,
        })
    }

    fn value(self, raw: &str) -> Result<String> {
        match self {
            Self::SourceFileDate => parse_source_file_date(raw),
            _ => Ok(raw.to_owned()),
        }
    }

    fn slot(self, metadata: &mut Metadata) -> &mut String {
        match self {
            Self::Category => &mut metadata.category,
            Self::MaintainerUrl => &mut metadata.maintainer_url,
            Self::Maintainer => &mut metadata.maintainer,
            Self::ListSourceUrl => &mut metadata.list_source_url,
            Self::SourceFileDate => &mut metadata.source_file_date_rfc3339,
        }
    }
}

impl fmt::Display for HeaderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Category => "Category",
            Self::MaintainerUrl => "Maintainer URL",
            Self::Maintainer => "Maintainer",
            Self::ListSourceUrl => "List source URL",
            Self::SourceFileDate => "Source File Date",
        })
    }
}

/// Parses the content of the list file `file_name`. Fails on the first malformed line.
pub fn parse(file_name: &str, content: &str) -> Result<Ipset> {
    // Entries are ~13 bytes each; over-estimating slightly avoids reallocations.
    let mut networks: Vec<Ipv4Net> = Vec::with_capacity(content.len() / 12);
    let mut sections = Vec::with_capacity(1);
    let mut metadata = Metadata::default();

    for (index, raw_line) in content.lines().enumerate() {
        let line_number = index + 1;

        if let Some(comment) = raw_line.strip_prefix('#') {
            let Some((key, value)) = comment.trim().split_once(':') else {
                continue;
            };
            let Some(key) = HeaderKey::parse(key.trim_end()) else {
                continue;
            };
            let value = key
                .value(value.trim())
                .with_context(|| format!("{file_name}:{line_number}: invalid {key}"))?;
            let slot = key.slot(&mut metadata);
            if *slot != value {
                if !networks.is_empty() {
                    // The header changed after networks were read: freeze the current section.
                    sections.push(Section {
                        metadata: metadata.clone(),
                        networks: std::mem::take(&mut networks),
                    });
                }
                *key.slot(&mut metadata) = value;
            }
            continue;
        }

        let entry = raw_line.trim();
        if entry.is_empty() {
            continue;
        }
        let network = parse_network(entry)
            .with_context(|| format!("{file_name}:{line_number}: invalid entry {entry:?}"))?;
        networks.push(network);
    }

    if !networks.is_empty() {
        networks.shrink_to_fit();
        sections.push(Section { metadata, networks });
    }
    Ok(Ipset {
        file_name: file_name.to_owned(),
        sections,
    })
}

/// Parses `a.b.c.d/n` or a bare `a.b.c.d` (treated as `/32`).
fn parse_network(entry: &str) -> Result<Ipv4Net> {
    if entry.contains('/') {
        Ok(entry.parse::<Ipv4Net>()?)
    } else {
        Ok(Ipv4Net::from(entry.parse::<Ipv4Addr>()?))
    }
}

/// Converts `Thu Sep 10 23:59:49 UTC 2026` (the `date -u` format used by FireHOL) to RFC 3339.
fn parse_source_file_date(value: &str) -> Result<String> {
    NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S UTC %Y")
        .map(|datetime| datetime.and_utc().to_rfc3339())
        .with_context(|| format!("failed to parse datetime {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
#
# firehol_level1
#
# ipv4 hash:net ipset
#
# Maintainer      : FireHOL
# Maintainer URL  : http://iplists.firehol.org/
# List source URL : 
# Source File Date: Thu Sep 10 23:59:49 UTC 2026
#
# Category        : attacks
# Version         : 32851
#
#  http://iplists.firehol.org/?ipset=firehol_level1
#
0.0.0.0/8
1.10.16.0/20

  5.6.7.8  
";

    fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    #[test]
    fn parses_header_and_networks() {
        let ipset = parse("firehol_level1.netset", SAMPLE).unwrap();
        assert_eq!(ipset.file_name, "firehol_level1.netset");
        assert_eq!(ipset.network_count(), 3);
        assert_eq!(ipset.sections.len(), 1);

        let section = &ipset.sections[0];
        assert_eq!(
            section.metadata,
            Metadata {
                source_file_date_rfc3339: "2026-09-10T23:59:49+00:00".into(),
                list_source_url: String::new(),
                maintainer_url: "http://iplists.firehol.org/".into(),
                maintainer: "FireHOL".into(),
                category: "attacks".into(),
            }
        );
        assert_eq!(
            section.networks,
            [net("0.0.0.0/8"), net("1.10.16.0/20"), net("5.6.7.8/32")]
        );
    }

    #[test]
    fn missing_header_fields_stay_empty() {
        let ipset = parse("bare.ipset", "1.2.3.4\n").unwrap();
        assert_eq!(ipset.sections.len(), 1);
        assert_eq!(ipset.sections[0].metadata, Metadata::default());
        assert_eq!(ipset.sections[0].networks, [net("1.2.3.4/32")]);
    }

    #[test]
    fn empty_file_has_no_sections() {
        let ipset = parse("empty.ipset", "# Category: abuse\n\n").unwrap();
        assert!(ipset.sections.is_empty());
        assert_eq!(ipset.network_count(), 0);
    }

    #[test]
    fn header_change_after_networks_starts_a_new_section() {
        let content =
            "# Category: abuse\n1.1.1.1\n# Category: abuse\n2.2.2.2\n# Category: spam\n3.3.3.3\n";
        let ipset = parse("multi.ipset", content).unwrap();
        assert_eq!(ipset.sections.len(), 2);
        assert_eq!(ipset.sections[0].metadata.category, "abuse");
        assert_eq!(
            ipset.sections[0].networks,
            [net("1.1.1.1/32"), net("2.2.2.2/32")]
        );
        assert_eq!(ipset.sections[1].metadata.category, "spam");
        assert_eq!(ipset.sections[1].networks, [net("3.3.3.3/32")]);
    }

    #[test]
    fn malformed_entries_are_rejected() {
        for content in ["1.2.3\n", "1.2.3.4/33\n", "2001:db8::/32\n", "hello\n"] {
            let err = parse("bad.ipset", content).unwrap_err();
            assert!(err.to_string().contains("bad.ipset:1"), "{err:#}");
        }
        let err = parse("bad.ipset", "# Source File Date: yesterday\n").unwrap_err();
        assert!(
            err.to_string().contains("invalid Source File Date"),
            "{err:#}"
        );
    }

    #[test]
    fn source_file_date_matches_the_legacy_conversion() {
        for (raw, expected) in [
            ("Thu Sep 10 23:59:49 UTC 2026", "2026-09-10T23:59:49+00:00"),
            ("Fri Aug  7 10:10:14 UTC 2026", "2026-08-07T10:10:14+00:00"),
            ("Wed Jul  1 00:00:00 UTC 2026", "2026-07-01T00:00:00+00:00"),
        ] {
            assert_eq!(parse_source_file_date(raw).unwrap(), expected, "{raw}");
            // Reference implementation used before the rewrite.
            let legacy = chrono::DateTime::parse_from_str(
                &raw.replace(" UTC ", " +0000 "),
                "%a %b %d %H:%M:%S %z %Y",
            )
            .unwrap()
            .with_timezone(&chrono::Utc)
            .to_rfc3339();
            assert_eq!(legacy, expected, "{raw}");
        }
        assert!(parse_source_file_date("Thu Sep 10 23:59:49 CET 2026").is_err());
    }
}
