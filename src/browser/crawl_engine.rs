//! Crawl engine — BFS-based deep crawl scheduler.
//!
//! Manages crawl jobs: starting from a URL, following links matching patterns,
//! respecting depth limits, rate limiting between requests.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

use super::types::*;

/// A crawl job's internal state.
#[derive(Debug, Clone)]
struct CrawlJobState {
    config: CrawlConfig,
    visited: HashSet<String>,
    queue: Vec<String>,
    results: Vec<CrawlPageResult>,
    pages_crawled: u16,
    status: JobStatus,
}

#[derive(Debug, Clone, PartialEq)]
enum JobStatus {
    Running,
    Paused,
    Completed,
    Stopped,
}

/// Thread-safe crawl engine.
#[derive(Clone)]
pub struct CrawlEngine {
    jobs: Arc<RwLock<HashMap<JobId, CrawlJobState>>>,
}

impl CrawlEngine {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a new crawl job.
    pub async fn create_job(&self, config: CrawlConfig) -> JobId {
        let job_id = config.job_id.clone();
        let mut jobs = self.jobs.write().await;
        jobs.insert(
            job_id.clone(),
            CrawlJobState {
                visited: HashSet::new(),
                queue: vec![config.start_url.clone()],
                results: Vec::new(),
                pages_crawled: 0,
                status: JobStatus::Running,
                config,
            },
        );
        job_id
    }

    /// Get the next URL to crawl for a job (BFS order).
    pub async fn next_url(&self, job_id: &str) -> Option<String> {
        let mut jobs = self.jobs.write().await;
        let job = jobs.get_mut(job_id)?;

        while let Some(url) = job.queue.pop() {
            if job.visited.contains(&url) {
                continue;
            }
            if job.pages_crawled >= job.config.max_pages {
                job.status = JobStatus::Completed;
                return None;
            }

            // Same-domain check
            if job.config.same_domain {
                if let (Some(start_host), Some(current_host)) =
                    (url_host(&job.config.start_url), url_host(&url))
                {
                    if start_host != current_host {
                        continue;
                    }
                }
            }

            // Exclude pattern check
            if job
                .config
                .exclude_patterns
                .iter()
                .any(|p| regex_match(p, &url))
            {
                continue;
            }

            // Link pattern check (if patterns specified, URL must match at least one)
            if !job.config.link_patterns.is_empty()
                && !job
                    .config
                    .link_patterns
                    .iter()
                    .any(|p| regex_match(p, &url))
            {
                continue;
            }

            job.visited.insert(url.clone());
            return Some(url);
        }

        None
    }

    /// Add a crawled page result to the job.
    pub async fn add_result(&self, job_id: &str, result: CrawlPageResult) {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(job_id) {
            job.results.push(result);
            job.pages_crawled += 1;
        }
    }

    /// Enqueue new URLs discovered on a page.
    pub async fn enqueue_urls(&self, job_id: &str, urls: Vec<String>) {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(job_id) {
            for url in urls {
                if !job.visited.contains(&url) && !job.queue.contains(&url) {
                    job.queue.push(url);
                }
            }
        }
    }

    /// Update crawl progress (called from extension progress events).
    pub async fn update_progress(
        &self,
        job_id: &str,
        pages_crawled: u16,
        pages_total: u16,
        _current_url: &str,
    ) {
        let jobs = self.jobs.read().await;
        if let Some(_job) = jobs.get(job_id) {
            tracing::debug!("[CrawlEngine] Job {job_id}: {pages_crawled}/{pages_total} pages");
        }
    }

    /// Mark a crawl job as completed.
    pub async fn mark_complete(&self, job_id: &str) {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(job_id) {
            job.status = JobStatus::Completed;
        }
    }

    /// Pause a crawl job.
    pub async fn pause(&self, job_id: &str) -> bool {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(job_id) {
            job.status = JobStatus::Paused;
            true
        } else {
            false
        }
    }

    /// Resume a crawl job.
    pub async fn resume(&self, job_id: &str) -> bool {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(job_id) {
            job.status = JobStatus::Running;
            true
        } else {
            false
        }
    }

    /// Stop a crawl job.
    pub async fn stop(&self, job_id: &str) -> bool {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(job_id) {
            job.status = JobStatus::Stopped;
            true
        } else {
            false
        }
    }

    /// Get the status of a crawl job.
    pub async fn get_status(&self, job_id: &str) -> Option<CrawlJobStatus> {
        let jobs = self.jobs.read().await;
        let job = jobs.get(job_id)?;
        let status_str = match job.status {
            JobStatus::Running => "running",
            JobStatus::Paused => "paused",
            JobStatus::Completed => "completed",
            JobStatus::Stopped => "stopped",
        };
        Some(CrawlJobStatus {
            job_id: job_id.to_owned(),
            status: status_str.to_owned(),
            pages_crawled: job.pages_crawled,
            pages_total: job.config.max_pages,
            results: job.results.clone(),
        })
    }

    /// List all active crawl job IDs.
    pub async fn list_jobs(&self) -> Vec<JobId> {
        self.jobs.read().await.keys().cloned().collect()
    }
}

impl Default for CrawlEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the host from a URL.
fn url_host(url: &str) -> Option<String> {
    let without_proto = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    without_proto.split('/').next().map(|s| s.to_lowercase())
}

/// Simple regex match (supports basic glob-style patterns and regex).
fn regex_match(pattern: &str, text: &str) -> bool {
    // Try regex first
    if let Ok(re) = regex::Regex::new(pattern) {
        return re.is_match(text);
    }
    // Fall back to substring match
    text.contains(pattern)
}
