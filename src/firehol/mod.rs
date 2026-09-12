//! FireHOL blocklists: git synchronisation, parsing and MMDB generation.

pub mod builder;
mod git;
pub mod ipset;

use self::git::GitRepository;
use self::ipset::Ipset;
use crate::display::{EpochSeconds, HumanCount, HumanSize};
use anyhow::{Context, Result, bail};
use git2::{ObjectType, Repository, Tree};
use rayon::prelude::*;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

const COUNTRY_DIR_SUFFIX: &str = "_country";

/// Keeps a local clone of the FireHOL repository in sync and rebuilds the MMDB from it.
pub struct FireholUpdater {
    repo: GitRepository,
    ignore_country: bool,
    /// Prevents overlapping runs (start-up vs. cron, or a slow run vs. the next tick), which
    /// would otherwise race on the repository and the temporary database file.
    update_lock: Mutex<()>,
}

impl FireholUpdater {
    pub fn new(
        repo_path: PathBuf,
        repo_url: String,
        repo_branch: String,
        ignore_country: bool,
    ) -> Self {
        Self {
            repo: GitRepository::new(repo_path, repo_url, repo_branch),
            ignore_country,
            update_lock: Mutex::new(()),
        }
    }

    /// Synchronises the repository and rebuilds `mmdb_path` when the blocklists are newer than
    /// the file. Returns `Ok(true)` when a new database was written.
    pub fn update_and_build_mmdb(&self, mmdb_path: &Path) -> Result<bool> {
        let _running = match self.update_lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => {
                warn!("firehol update already in progress, skipping this run");
                return Ok(false);
            }
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };

        let repo = self.repo.open_or_clone()?;
        let tip = self.repo.remote_tip(&repo)?;
        let commit_epoch = tip.time().seconds();
        let mmdb_epoch = modified_epoch(mmdb_path)?;
        if commit_epoch <= mmdb_epoch {
            info!(
                "commit {:.7} on {} ({}) is not newer than {} ({}), skipping update",
                tip.id().to_string(),
                self.repo.branch(),
                EpochSeconds(commit_epoch),
                mmdb_path.display(),
                EpochSeconds(mmdb_epoch),
            );
            return Ok(false);
        }

        self.repo.checkout(&repo, &tip)?;
        let tree = tip.tree().context("failed to read the commit tree")?;
        let files = self.collect_ipset_files(&repo, &tree)?;
        info!(
            "parsing {} ipset files from {}",
            HumanCount::from(files.len()),
            self.repo.path().display()
        );

        let ipsets = self.parse_files(&files);
        let network_count: usize = ipsets.iter().map(Ipset::network_count).sum();
        info!(
            "{} networks parsed from {} files",
            HumanCount::from(network_count),
            HumanCount::from(ipsets.len())
        );
        if network_count == 0 {
            bail!("no ipset entries found in {}", self.repo.path().display());
        }

        let build_epoch = UNIX_EPOCH
            .checked_add(Duration::from_secs(
                u64::try_from(commit_epoch).context("negative commit timestamp")?,
            ))
            .context("commit timestamp out of range")?;
        info!("creating FireHOL mmdb database...");
        let size = builder::write_mmdb(&ipsets, build_epoch, mmdb_path)?;
        info!("mmdb {} updated ({})", mmdb_path.display(), HumanSize(size));
        Ok(true)
    }

    /// Parses every file in parallel; unreadable or malformed files are skipped with a warning.
    fn parse_files(&self, files: &[PathBuf]) -> Vec<Ipset> {
        files
            .par_iter()
            .filter_map(|relative| {
                let path = self.repo.path().join(relative);
                let parsed = relative
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("file name is not valid UTF-8")
                    .and_then(|file_name| {
                        let content = fs::read_to_string(&path).context("cannot read file")?;
                        ipset::parse(file_name, &content)
                    });
                match parsed {
                    Ok(ipset) => {
                        debug!(
                            "parsed {} networks from {}",
                            ipset.network_count(),
                            path.display()
                        );
                        Some(ipset)
                    }
                    Err(err) => {
                        warn!("skipping {}: {err:#}", path.display());
                        None
                    }
                }
            })
            .collect()
    }

    /// Lists the `.ipset` / `.netset` blobs of `tree`, relative to the repository root.
    fn collect_ipset_files(&self, repo: &Repository, tree: &Tree<'_>) -> Result<Vec<PathBuf>> {
        let mut files = Vec::with_capacity(1024);
        self.collect_recursive(repo, tree, &mut PathBuf::new(), &mut files)?;
        Ok(files)
    }

    fn collect_recursive(
        &self,
        repo: &Repository,
        tree: &Tree<'_>,
        prefix: &mut PathBuf,
        files: &mut Vec<PathBuf>,
    ) -> Result<()> {
        for entry in tree.iter() {
            let name = entry.name().context("non UTF-8 entry name in git tree")?;
            match entry.kind() {
                Some(ObjectType::Tree) => {
                    if self.ignore_country && name.ends_with(COUNTRY_DIR_SUFFIX) {
                        debug!("ignoring directory {name}");
                        continue;
                    }
                    let subtree = entry
                        .to_object(repo)
                        .and_then(|object| object.peel_to_tree())
                        .with_context(|| format!("failed to read tree {name}"))?;
                    prefix.push(name);
                    let result = self.collect_recursive(repo, &subtree, prefix, files);
                    prefix.pop();
                    result?;
                }
                Some(ObjectType::Blob) if is_ipset_file(name) => files.push(prefix.join(name)),
                _ => {}
            }
        }
        Ok(())
    }
}

fn is_ipset_file(name: &str) -> bool {
    name.ends_with(".ipset") || name.ends_with(".netset")
}

/// Modification time of `path` as Unix seconds, or `0` when the file does not exist.
fn modified_epoch(path: &Path) -> Result<i64> {
    match fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .with_context(|| format!("failed to read mtime of {}", path.display()))?;
            Ok(system_time_to_epoch(modified))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            info!("mmdb file {} does not exist yet", path.display());
            Ok(0)
        }
        Err(err) => {
            Err(err).with_context(|| format!("failed to read metadata of {}", path.display()))
        }
    }
}

fn system_time_to_epoch(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    #[test]
    fn recognises_list_files_by_extension() {
        assert!(is_ipset_file("firehol_level1.netset"));
        assert!(is_ipset_file("abuseipdb_1d.ipset"));
        assert!(!is_ipset_file("README.md"));
        assert!(!is_ipset_file("list.ipset.bak"));
    }

    #[test]
    fn modified_epoch_is_zero_for_missing_files() {
        let dir = TempDir::new("epoch");
        assert_eq!(modified_epoch(&dir.path().join("missing")).unwrap(), 0);

        let file = dir.path().join("present");
        fs::write(&file, b"x").unwrap();
        let expected = system_time_to_epoch(fs::metadata(&file).unwrap().modified().unwrap());
        assert!(expected > 0);
        assert_eq!(modified_epoch(&file).unwrap(), expected);
    }

    #[test]
    fn system_time_conversion_saturates() {
        assert_eq!(system_time_to_epoch(UNIX_EPOCH), 0);
        assert_eq!(
            system_time_to_epoch(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            1_700_000_000
        );
        assert_eq!(system_time_to_epoch(UNIX_EPOCH - Duration::from_secs(1)), 0);
    }
}
