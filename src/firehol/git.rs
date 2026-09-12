//! Git synchronisation of the FireHOL blocklist repository.

use anyhow::{Context, Result};
use git2::build::RepoBuilder;
use git2::{CertificateCheckStatus, Commit, FetchOptions, RemoteCallbacks, Repository, ResetType};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tracing::info;

const REMOTE_NAME: &str = "origin";

/// A local clone tracking one branch of a remote repository.
pub struct GitRepository {
    path: PathBuf,
    url: String,
    branch: String,
}

impl GitRepository {
    pub fn new(path: PathBuf, url: String, branch: String) -> Self {
        Self { path, url, branch }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Opens the local clone and fetches the tracked branch, or clones it when the directory
    /// does not exist yet (or is empty, e.g. a freshly mounted volume).
    pub fn open_or_clone(&self) -> Result<Repository> {
        if !is_missing_or_empty_dir(&self.path)? {
            info!("opening existing repository {}", self.path.display());
            let repo = Repository::open(&self.path)
                .with_context(|| format!("failed to open repository {}", self.path.display()))?;
            self.fetch(&repo)?;
            return Ok(repo);
        }

        info!(
            "cloning {} (branch {}) into {}",
            self.url,
            self.branch,
            self.path.display()
        );
        RepoBuilder::new()
            .fetch_options(fetch_options())
            .branch(&self.branch)
            .clone(&self.url, &self.path)
            .with_context(|| format!("failed to clone {}", self.url))
    }

    /// Commit at the tip of the fetched remote branch.
    pub fn remote_tip<'repo>(&self, repo: &'repo Repository) -> Result<Commit<'repo>> {
        let name = self.remote_ref_name();
        let oid = repo
            .find_reference(&name)
            .with_context(|| format!("remote branch not found: {name}"))?
            .target()
            .with_context(|| format!("{name} is not a direct reference"))?;
        repo.find_commit(oid)
            .with_context(|| format!("commit {oid} not found"))
    }

    /// Points the local branch (and `HEAD`) at `commit` and hard-resets the working tree to it.
    pub fn checkout(&self, repo: &Repository, commit: &Commit<'_>) -> Result<()> {
        let local = self.local_ref_name();
        repo.reference(&local, commit.id(), true, "update to remote tip")
            .with_context(|| format!("failed to update {local}"))?;
        repo.set_head(&local)
            .with_context(|| format!("failed to point HEAD at {local}"))?;

        info!(
            "hard resetting {} to {:.7}",
            self.branch,
            commit.id().to_string()
        );
        repo.reset(commit.as_object(), ResetType::Hard, None)
            .context("failed to hard reset the working tree")
    }

    fn fetch(&self, repo: &Repository) -> Result<()> {
        let mut remote = repo
            .find_remote(REMOTE_NAME)
            .or_else(|_| repo.remote(REMOTE_NAME, &self.url))
            .with_context(|| format!("failed to get or create remote {REMOTE_NAME}"))?;

        // `+` forces the update: FireHOL rewrites the branch history on every publication.
        let refspec = format!("+refs/heads/{}:{}", self.branch, self.remote_ref_name());
        info!("fetching {} from {}", self.branch, self.url);
        remote
            .fetch(&[refspec.as_str()], Some(&mut fetch_options()), None)
            .with_context(|| format!("failed to fetch {} from {}", self.branch, self.url))
    }

    fn remote_ref_name(&self) -> String {
        format!("refs/remotes/{REMOTE_NAME}/{}", self.branch)
    }

    fn local_ref_name(&self) -> String {
        format!("refs/heads/{}", self.branch)
    }
}

/// `true` when `path` does not exist or is a directory without entries.
fn is_missing_or_empty_dir(path: &Path) -> Result<bool> {
    match fs::read_dir(path) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(true),
        Err(err) if err.kind() == ErrorKind::NotADirectory => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn fetch_options() -> FetchOptions<'static> {
    let mut callbacks = RemoteCallbacks::new();
    // TLS certificate validation is bypassed on purpose: the statically linked (musl) build
    // embeds OpenSSL without a known CA bundle location, so validation would always fail there.
    callbacks.certificate_check(|_, _| Ok(CertificateCheckStatus::CertificateOk));
    let mut options = FetchOptions::new();
    options.remote_callbacks(callbacks);
    options
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    #[test]
    fn detects_missing_and_empty_directories() {
        let dir = TempDir::new("git-empty");
        assert!(is_missing_or_empty_dir(&dir.path().join("missing")).unwrap());
        assert!(is_missing_or_empty_dir(dir.path()).unwrap());

        fs::write(dir.path().join("file"), b"x").unwrap();
        assert!(!is_missing_or_empty_dir(dir.path()).unwrap());
        assert!(!is_missing_or_empty_dir(&dir.path().join("file")).unwrap());
    }
}
