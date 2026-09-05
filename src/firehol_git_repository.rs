use crate::utils::{epoch_to_string, format_size, pretty_number};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use git2::{
    CertificateCheckStatus, FetchOptions, ObjectType, RemoteCallbacks, Repository,
    build::RepoBuilder,
};
use mmdb_writer::ipnet::IpNet;
use mmdb_writer::{IpVersion, MergeStrategy, RecordSize, Writer};
use rayon::prelude::*;
use serde::Serialize;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter};
use std::path::Path;
use std::time::SystemTime;
use tracing::{debug, info, warn};

#[derive(Default, Clone)]
struct FileMetadata {
    source_file_date_rfc3339: Option<String>,
    list_source_url: Option<String>,
    maintainer_url: Option<String>,
    maintainer: Option<String>,
    category: Option<String>,
}

#[derive(Serialize, Clone)]
struct FireholEntry {
    file_name: Vec<String>,
    source_file_date_rfc3339: Vec<String>,
    list_source_url: Vec<String>,
    maintainer_url: Vec<String>,
    maintainer: Vec<String>,
    category: Vec<String>,
}

struct Firehol {
    entry: FireholEntry,
    ip: IpNet,
}

pub struct FireholGitRepository {
    repo_path: String,
    repo_url: String,
    repo_branch: String,
    ignore_country: bool,
}

impl FireholGitRepository {
    pub fn new(
        repo_path: String,
        repo_url: String,
        ignore_country: bool,
        repo_branch: String,
    ) -> Self {
        Self {
            repo_path,
            repo_url,
            ignore_country,
            repo_branch,
        }
    }

    pub fn update_and_build_mmdb(&self, mmdb_path: &str) -> Result<()> {
        let last_commit_epoch = self.get_last_commit_epoch(&self.repo_branch)?;
        let mmdb_epoch = self.get_mmdb_build_epoch(mmdb_path)?;

        if last_commit_epoch <= mmdb_epoch as i64 {
            info!(
                "commit epoch {} on {} <= mmdb epoch {}, skipping update",
                epoch_to_string(last_commit_epoch),
                self.repo_url,
                epoch_to_string(mmdb_epoch as i64),
            );
            return Ok(());
        }

        let repo = self.init_repository()?;
        info!("repository {} ready!", &self.repo_path);

        info!("collecting .ipset and .netset files...");
        let tree = self.get_head_tree(&repo)?;
        let files = self.collect_files(&repo, &tree)?;
        info!(
            "{} files found in {}",
            pretty_number(files.len() as u32),
            &self.repo_path
        );

        info!("parsing {} files...", pretty_number(files.len() as u32));
        let entries: Vec<Firehol> = files
            .par_iter()
            .filter_map(|file_path| {
                let full_path = format!("{}/{}", self.repo_path, file_path);
                match self.parse_file(&full_path) {
                    Ok(entries) => Some(entries),
                    Err(e) => {
                        warn!("error parsing {full_path}: {e}");
                        None
                    }
                }
            })
            .flatten()
            .collect();

        info!(
            "{} entries parsed from {} files",
            pretty_number(entries.len() as u32),
            pretty_number(files.len() as u32)
        );

        if entries.is_empty() {
            return Err(anyhow::anyhow!("{}:{} no entries found", file!(), line!()));
        }

        info!("creating FireHol mmdb database...");
        let last_commit_system_time = SystemTime::UNIX_EPOCH
            .checked_add(std::time::Duration::from_secs(last_commit_epoch as u64))
            .with_context(|| format!("{}:{} invalid commit timestamp", file!(), line!()))?;

        let mut writer = Writer::builder("Firehol-DB")
            .ip_version(IpVersion::V4)
            .record_size(RecordSize::Bits28)
            .build_epoch(last_commit_system_time)
            .build();

        for firehol in &entries {
            writer
                .insert_merged(firehol.ip, &firehol.entry, MergeStrategy::DeepMerge)
                .with_context(|| format!("{}:{}", file!(), line!()))?;
        }

        let tmp_mmdb_path = Path::new(mmdb_path).with_extension("tmp");
        let file = File::create(&tmp_mmdb_path)
            .with_context(|| format!("failed to create {tmp_mmdb_path:?}"))?;

        info!("writing mmdb file to {tmp_mmdb_path:?} ...");
        writer
            .write_to(BufWriter::new(file))
            .with_context(|| format!("error writing mmdb file {tmp_mmdb_path:?}"))?;

        let metadata = fs::metadata(&tmp_mmdb_path)
            .with_context(|| format!("failed to get metadata of {tmp_mmdb_path:?}"))?;
        info!(
            "{tmp_mmdb_path:?} mmdb file created, {}",
            format_size(metadata.len())
        );

        if Path::new(mmdb_path).exists() {
            info!("remove old mmdb file {}", mmdb_path);
            fs::remove_file(mmdb_path)
                .with_context(|| format!("failed to remove old mmdb file {mmdb_path}"))?;
        }

        info!("rename mmdb file {tmp_mmdb_path:?} -> {mmdb_path:?}");
        fs::rename(&tmp_mmdb_path, mmdb_path)
            .with_context(|| format!("failed to rename {tmp_mmdb_path:?} to {mmdb_path:?}"))?;

        Ok(())
    }

    fn get_last_commit_epoch(&self, branch: &str) -> Result<i64> {
        let repo = Repository::init("/tmp/git2-empty")
            .with_context(|| "failed to init temporary repository")?;

        let mut remote = repo
            .remote_anonymous(&self.repo_url)
            .with_context(|| "failed to create remote")?;

        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(RemoteCallbacks::new());

        remote
            .connect(git2::Direction::Fetch)
            .with_context(|| "failed to connect to remote")?;

        let refs = remote
            .list()
            .with_context(|| "failed to list remote references")?;

        let branch_ref_names = [
            format!("refs/heads/{branch}"),
            format!("refs/remotes/origin/{branch}"),
        ];

        let found_ref = refs
            .iter()
            .find(|r| branch_ref_names.contains(&r.name().to_string()))
            .or_else(|| {
                refs.iter().find(|r| {
                    r.name().starts_with("refs/heads/") || r.name().starts_with("refs/remotes/")
                })
            })
            .with_context(|| {
                format!(
                    "branch '{branch}' not found. Available: {}",
                    refs.iter()
                        .filter_map(|r| r.name().strip_prefix("refs/heads/"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;

        let oid = found_ref.oid();
        let actual_branch = found_ref
            .name()
            .strip_prefix("refs/heads/")
            .or_else(|| found_ref.name().strip_prefix("refs/remotes/origin/"))
            .unwrap_or(branch)
            .to_string();

        remote
            .fetch(&[&actual_branch], Some(&mut fetch_options), None)
            .with_context(|| format!("failed to fetch branch {actual_branch}"))?;

        repo.find_commit(oid)
            .with_context(|| format!("failed to find commit {oid}"))
            .map(|c| c.time().seconds())
    }

    fn init_repository(&self) -> Result<Repository> {
        if Path::new(&self.repo_path).exists() {
            info!("existing repository found, opening...");
            let repo = Repository::open(&self.repo_path)?;
            info!("checking for updates...");
            self.fetch_updates(&repo)?;
            Ok(repo)
        } else {
            info!(
                "cloning repository: {} to {}",
                self.repo_url, self.repo_path
            );

            let mut callbacks = RemoteCallbacks::new();
            callbacks.certificate_check(|_, _| Ok(CertificateCheckStatus::CertificateOk));

            let mut fetch_options = FetchOptions::new();
            fetch_options.remote_callbacks(callbacks);

            let mut builder = RepoBuilder::new();
            builder.fetch_options(fetch_options);
            builder.branch(&self.repo_branch);

            builder
                .clone(&self.repo_url, Path::new(&self.repo_path))
                .map_err(Into::into)
        }
    }

    fn get_head_tree<'a>(&self, repo: &'a Repository) -> Result<git2::Tree<'a>> {
        let head = repo.head().with_context(|| "failed to get HEAD")?;
        let commit_id = head.target().with_context(|| "detached HEAD")?;
        let commit = repo.find_commit(commit_id)?;
        commit.tree().map_err(Into::into)
    }

    fn get_mmdb_build_epoch(&self, mmdb_path: &str) -> Result<u64> {
        let mmdb_path = Path::new(mmdb_path);
        if !mmdb_path.exists() {
            info!("mmdb file {mmdb_path:?} does not exist");
            return Ok(0);
        }
        let metadata = fs::metadata(mmdb_path)
            .with_context(|| format!("failed to get metadata for {mmdb_path:?}"))?;
        let modified = metadata.modified()?;
        modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .with_context(|| "failed to get modified time")
    }

    fn fetch_updates(&self, repo: &Repository) -> Result<()> {
        let mut remote = repo
            .find_remote("origin")
            .or_else(|_| repo.remote("origin", &self.repo_url))
            .with_context(|| "failed to get or create remote")?;

        let mut callbacks = RemoteCallbacks::new();
        callbacks.certificate_check(|_, _| Ok(CertificateCheckStatus::CertificateOk));

        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);

        let fetch_branches = [format!(
            "+refs/heads/{branch}:refs/remotes/origin/{branch}",
            branch = self.repo_branch
        )];

        remote
            .fetch(&fetch_branches, Some(&mut fetch_options), None)
            .with_context(|| format!("failed to fetch {}", self.repo_branch))?;

        let remote_ref_name = format!("refs/remotes/origin/{}", self.repo_branch);
        let remote_ref = repo
            .find_reference(&remote_ref_name)
            .with_context(|| format!("remote branch not found: {remote_ref_name}"))?;

        let remote_commit_id = remote_ref
            .target()
            .with_context(|| format!("invalid remote reference: {remote_ref_name}"))?;

        let remote_commit = repo
            .find_commit(remote_commit_id)
            .with_context(|| "failed to find remote commit")?;

        let local_ref_name = format!("refs/heads/{}", self.repo_branch);

        match repo.find_reference(&local_ref_name) {
            Ok(mut local_ref) => {
                local_ref
                    .set_target(remote_commit_id, "updating to remote")
                    .with_context(|| "failed to update local branch")?;
            }
            Err(_) => {
                repo.reference(
                    &local_ref_name,
                    remote_commit_id,
                    true,
                    "creating local branch",
                )
                .with_context(|| "failed to create local branch")?;
            }
        }

        info!(
            "hard resetting to {} ({})",
            self.repo_branch,
            &remote_commit_id.to_string()[..7]
        );

        repo.reset(remote_commit.as_object(), git2::ResetType::Hard, None)
            .with_context(|| "failed to reset to remote commit")?;

        info!("repository updated successfully!");
        Ok(())
    }

    fn collect_files(&self, repo: &Repository, tree: &git2::Tree) -> Result<Vec<String>> {
        let mut files = Vec::with_capacity(512);
        self.collect_files_recursive(repo, tree, "", &mut files)?;
        files.shrink_to_fit();
        Ok(files)
    }

    fn collect_files_recursive<'a>(
        &self,
        repo: &'a Repository,
        tree: &git2::Tree<'a>,
        prefix: &str,
        files: &mut Vec<String>,
    ) -> Result<()> {
        for entry in tree.iter() {
            let name = entry.name().context("Invalid entry name")?;

            match entry.kind() {
                Some(ObjectType::Tree) if !self.ignore_country || !name.ends_with("_country") => {
                    if self.ignore_country && name.ends_with("_country") {
                        debug!("ignore directory: {}", name);
                    } else {
                        let subtree = entry.to_object(repo)?.peel_to_tree()?;
                        let new_prefix = format!("{prefix}{name}/");
                        self.collect_files_recursive(repo, &subtree, &new_prefix, files)?;
                    }
                }
                Some(ObjectType::Blob) if name.ends_with(".ipset") || name.ends_with(".netset") => {
                    files.push(format!("{prefix}{name}"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn parse_file(&self, file_path: &str) -> Result<Vec<Firehol>> {
        let filename = Path::new(file_path)
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("invalid filename: {file_path}"))?
            .to_string();

        let file = File::open(file_path).with_context(|| format!("cannot open {file_path}"))?;
        let reader = BufReader::new(file);

        let mut metadata = FileMetadata::default();
        let mut entries = Vec::with_capacity(1024);

        for line_result in reader.lines() {
            let line =
                line_result.with_context(|| format!("error reading line from {file_path}"))?;

            if let Some(comment) = line.strip_prefix('#') {
                self.parse_metadata_line(comment.trim(), &mut metadata)?;
            } else {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let ip_net = self
                    .parse_ip_line(trimmed)
                    .with_context(|| format!("invalid line in {file_path}: {trimmed}"))?;

                entries.push(Firehol {
                    entry: FireholEntry {
                        file_name: vec![filename.clone()],
                        source_file_date_rfc3339: vec![
                            metadata
                                .source_file_date_rfc3339
                                .clone()
                                .unwrap_or_default(),
                        ],
                        list_source_url: vec![metadata.list_source_url.clone().unwrap_or_default()],
                        maintainer_url: vec![metadata.maintainer_url.clone().unwrap_or_default()],
                        maintainer: vec![metadata.maintainer.clone().unwrap_or_default()],
                        category: vec![metadata.category.clone().unwrap_or_default()],
                    },
                    ip: ip_net,
                });
            }
        }

        debug!("parsed {} entries from {}", entries.len(), file_path);
        Ok(entries)
    }

    #[inline]
    fn parse_ip_line(&self, line: &str) -> Result<IpNet> {
        line.split_once('/')
            .map(|(ip, prefix)| {
                IpNet::new(ip.parse()?, prefix.parse::<u8>()?)
                    .with_context(|| format!("invalid CIDR: {line}"))
            })
            .unwrap_or_else(|| {
                IpNet::new(line.parse()?, 32).with_context(|| format!("invalid IP: {line}"))
            })
    }

    fn parse_metadata_line(&self, line: &str, metadata: &mut FileMetadata) -> Result<()> {
        let Some((prefix, value)) = line.split_once(':') else {
            return Ok(());
        };

        let value = value.trim();
        let prefix = prefix.trim_end();

        match prefix {
            "Category" => metadata.category = Some(value.to_string()),
            "Maintainer URL" => metadata.maintainer_url = Some(value.to_string()),
            "Maintainer" => metadata.maintainer = Some(value.to_string()),
            "List source URL" => metadata.list_source_url = Some(value.to_string()),
            "Source File Date" => {
                let date = DateTime::parse_from_str(
                    &value.replace(" UTC ", " +0000 "),
                    "%a %b %d %H:%M:%S %z %Y",
                )
                .with_context(|| format!("failed to parse datetime: {value}"))?
                .with_timezone(&Utc);
                metadata.source_file_date_rfc3339 = Some(date.to_rfc3339());
            }
            _ => {}
        }
        Ok(())
    }
}
