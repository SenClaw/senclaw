//! Git sync operations for marketplace sources. Mirrors `src-old/marketplace/GitSync.ts`.

use std::path::Path;

use anyhow::{Context, Result};

/// Clone or pull a git repository.
///
/// If the repository already exists at local_path, performs a fetch + checkout + pull.
/// Otherwise, clones the repository with the specified branch and shallow depth.
pub fn clone_or_pull(url: &str, branch: &str, local_path: &Path) -> Result<()> {
    let is_existing_repo = local_path.join(".git").exists();

    if is_existing_repo {
        pull_existing(url, branch, local_path)?;
    } else {
        clone_fresh(url, branch, local_path)?;
    }

    Ok(())
}

fn pull_existing(_url: &str, branch: &str, local_path: &Path) -> Result<()> {
    let repo = git2::Repository::open(local_path)
        .with_context(|| format!("Failed to open existing repo at {:?}", local_path))?;

    // Fetch from origin
    let mut origin = repo
        .find_remote("origin")
        .context("Failed to find origin remote")?;

    origin
        .fetch(&[branch], None, None)
        .context("Failed to fetch from origin")?;

    // Checkout the branch
    let obj = repo
        .revparse_single(&format!("origin/{}", branch))
        .with_context(|| format!("Failed to find origin/{}", branch))?;

    repo.checkout_tree(&obj, None)
        .context("Failed to checkout tree")?;

    repo.set_head(&format!("refs/heads/{}", branch))
        .context("Failed to set HEAD")?;

    // Pull (merge)
    let mut remote = repo
        .find_remote("origin")
        .context("Failed to find origin remote")?;

    remote
        .fetch(&[branch], None, None)
        .context("Failed to fetch during pull")?;

    let fetch_head = repo
        .find_reference("FETCH_HEAD")
        .context("Failed to find FETCH_HEAD")?;

    let fetch_commit = repo
        .reference_to_annotated_commit(&fetch_head)
        .context("Failed to get fetch commit")?;

    let analysis = repo.merge_analysis(&[&fetch_commit])?;
    if analysis.0.is_up_to_date() {
        // Already up to date, nothing to do
        return Ok(());
    }

    if analysis.0.is_fast_forward() {
        // Fast-forward merge
        let mut reference = repo
            .find_reference(&format!("refs/heads/{}", branch))
            .context("Failed to find branch reference")?;

        reference
            .set_target(fetch_commit.id(), "Fast-forward pull")
            .context("Failed to set reference target")?;

        repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))
            .context("Failed to checkout head")?;
    } else if analysis.0.is_normal() {
        // Normal merge - not supported in this simple implementation
        anyhow::bail!("Normal merge required, not supported in simple pull");
    }

    Ok(())
}

fn clone_fresh(url: &str, branch: &str, local_path: &Path) -> Result<()> {
    // Remove existing directory if it exists (but not a git repo)
    if local_path.exists() {
        std::fs::remove_dir_all(local_path)
            .with_context(|| format!("Failed to remove existing directory {:?}", local_path))?;
    }

    // Create parent directory
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create parent directory {:?}", parent))?;
    }

    // Full clone. (This is deliberate for marketplace sources, which are small
    // and whose `pull_existing` path fetches by refspec — see
    // `clone_shallow` for the variant used where history is dead weight.)
    git2::Repository::clone(url, local_path)
        .with_context(|| format!("Failed to clone {} to {:?}", url, local_path))?;

    // Checkout the specified branch
    let repo = git2::Repository::open(local_path)
        .with_context(|| format!("Failed to open cloned repo at {:?}", local_path))?;

    // Find the remote branch
    let obj = repo
        .revparse_single(&format!("origin/{}", branch))
        .with_context(|| format!("Failed to find origin/{}", branch))?;

    repo.checkout_tree(&obj, None)
        .context("Failed to checkout tree")?;

    repo.set_head(&format!("refs/heads/{}", branch))
        .context("Failed to set HEAD")?;

    Ok(())
}

/// Clone one ref at `depth`, discarding history.
///
/// For a content repository — a pattern library, a prompt collection — the
/// history is dead weight: cloning `danielmiessler/fabric` in full takes
/// **~400 seconds**, against roughly a tenth of that for the working tree
/// alone. Nothing in SenClaw reads a pattern's git log.
///
/// Kept separate from [`clone_or_pull`] rather than replacing it: a shallow
/// repository cannot be deepened later, and the marketplace's `pull_existing`
/// path assumes a normal clone.
pub fn clone_shallow(url: &str, git_ref: &str, local_path: &Path, depth: i32) -> Result<()> {
    if local_path.exists() {
        std::fs::remove_dir_all(local_path)
            .with_context(|| format!("Failed to remove existing directory {:?}", local_path))?;
    }
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create parent directory {:?}", parent))?;
    }

    let mut fetch = git2::FetchOptions::new();
    fetch.depth(depth);

    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fetch);
    // A tag and a branch both go here: libgit2 resolves either against the
    // remote, and a sha is the one form that needs a full clone — which is
    // why `sync_source` falls back when this fails.
    builder.branch(git_ref);

    builder
        .clone(url, local_path)
        .with_context(|| format!("Failed to shallow-clone {url} at {git_ref}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    #[ignore] // Requires network access
    fn test_clone_fresh() {
        let temp_dir = TempDir::new().unwrap();
        let local_path = temp_dir.path().join("test-repo");

        let result = clone_fresh("https://github.com/midea-ai/SenClaw", "main", &local_path);

        assert!(result.is_ok());
        assert!(local_path.join(".git").exists());
    }
}
