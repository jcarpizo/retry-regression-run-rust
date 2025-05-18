use anyhow::{Result, Context};
use curl::easy::Easy;
use regex::Regex;
use serde::{Deserialize, de::DeserializeOwned};
use tokio::time::{self, Duration};
use urlencoding::encode;
use colored::Colorize;
use once_cell::sync::Lazy;
use reqwest::Client;
use tokio::time::sleep;
use tokio::sync::Semaphore;
use std::{
    env,
    io::{stdout, Write},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

const USERNAME: &str = "jasper";
const TOKEN: &str = "";

const JENKINS_URLS: [&str; 4] = [
];

static EXCLUDED_MODULES: Lazy<Vec<&str>> = Lazy::new(|| vec![
]);

static EXCLUDED_JOB_NAMES: Lazy<Vec<&str>> = Lazy::new(|| vec![
]);

static EXCLUDED_SUITE_START: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(_start_test_suite|-test-suite-start|-post-deployment)").unwrap()
});

static REMOVED_SUFFIX_SPACE_HASH_NUMBER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\s+#\d+$").unwrap()
});

static CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .user_agent("jenkins-monitor/1.0")
        .build()
        .unwrap()
});

#[derive(Debug, Deserialize)]
struct JobList {
    jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct Job {
    fullDisplayName: String,
    disabled: bool,
    inQueue: bool,
    url: String,
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct JobResult {
    result: Option<String>,
    inProgress: bool,
}

#[derive(Clone)]
struct Config {
    retry: bool,
    base_url: &'static str,
    retry_interval: u64,
}

mod jenkins {
    use super::*;

    pub fn create_easy_handle(url: &str) -> Result<Easy> {
        let mut handle = Easy::new();
        handle.url(url)?;
        handle.username(USERNAME)?;
        handle.password(TOKEN)?;
        Ok(handle)
    }

    pub async fn fetch_json<T: DeserializeOwned>(url: &str) -> Result<T> {
        let response = CLIENT.get(url)
            .basic_auth(USERNAME, Some(TOKEN))
            .send()
            .await
            .with_context(|| format!("Failed to fetch JSON from {}", url))?
            .text()
            .await
            .with_context(|| format!("Failed to read response from {}", url))?;

        serde_json::from_str(&response)
            .or_else(|_| serde_json::from_str(r#"{"result": null, "inProgress": true}"#).map_err(Into::into))
    }

    pub async fn get_all_listed_jobs(config: &Config) -> Result<JobList> {
        let url = format!(
            "{}/api/json?tree=jobs[name,fullDisplayName,disabled,inQueue,url]",
            config.base_url
        );
        fetch_json(&url).await
    }

    pub async fn get_job_status_details(config: &Config, job_name: &str) -> Result<JobResult> {
        let url = format!("{}/job/{}/lastBuild/api/json", config.base_url, encode(job_name));
        fetch_json(&url).await
    }

    pub async fn post_retry_failed_job(config: &Config, job_name: &str, job_url: &str) -> Result<()> {
        let url = format!("{}/job/{}/buildWithParameters", config.base_url, encode(job_name));
        let mut handle = create_easy_handle(&url)?;
        handle.post(true)?;
        handle.perform()?;
        println!(
            "=> {}: {} => {}",
            "Retried".white(),
            job_name.green(),
            job_url.italic().bright_black(),
        );
        Ok(())
    }
}

fn clear_screen() {
    print!("\x1B[2J\x1B[H");
    let _ = stdout().flush();
}

fn parse_arguments() -> Arc<Config> {
    let args: Vec<String> = env::args().collect();

    let url_index = args.windows(2)
        .find(|w| w[0] == "--url")
        .and_then(|w| w[1].parse::<usize>().ok())
        .map(|i| i.min(JENKINS_URLS.len() - 1))
        .unwrap_or(0);

    let retry_interval = args.windows(2)
        .find(|w| w[0] == "--interval")
        .and_then(|w| w[1].parse::<u64>().ok())
        .unwrap_or(300);

    let retry = args.contains(&"--retry".to_string());

    Arc::new(Config {
        retry,
        base_url: JENKINS_URLS[url_index],
        retry_interval,
    })
}

fn print_job_status_details(index: usize, job_name: &str, job_url: &str, status: &str) {
    match status {
        "FAILURE" => println!(
            "{}. {} => {}",
            index,
            job_name.red(),
            job_url.italic().bright_black()
        ),
        "IN_PROGRESS" => println!(
            "{}. {} => {}",
            index,
            job_name.yellow(),
            job_url.italic().bright_black(),
        ),
        _ => {}
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = parse_arguments();
    let mut interval = time::interval(Duration::from_secs(config.retry_interval));
    let counter = Arc::new(AtomicUsize::new(1));

    loop {
        interval.tick().await;
        clear_screen();
        counter.store(1, Ordering::Relaxed);
        println!(
            "{}\n🟢 🚀 SA Regression RUN - Retry Failed Jobs Tool: {} | Refresh Interval in Seconds: {} | Is Retry: {} : {} 🦀\n{}",
            "=".repeat(171).green().bold(),
            config.base_url.green().bold(),
            config.retry_interval,
            config.retry.to_string().green().bold(),
            "Language: Rust",
            "=".repeat(171).green().bold(),
        );

        let jobs = jenkins::get_all_listed_jobs(&config).await?.jobs;

        let filtered_jobs = jobs.into_iter().filter(|j| {
            !j.disabled &&
                !j.inQueue &&
                !EXCLUDED_SUITE_START.is_match(&j.fullDisplayName) &&
                !REMOVED_SUFFIX_SPACE_HASH_NUMBER.is_match(&j.fullDisplayName) &&
                !EXCLUDED_JOB_NAMES.contains(&j.fullDisplayName.as_str()) &&
                !EXCLUDED_MODULES.iter().any(|prefix| j.fullDisplayName.contains(prefix))
        });

        let semaphore = Arc::new(Semaphore::new(5)); // limit to 5 concurrent jobs
        let mut futures = vec![];

        for job in filtered_jobs {
            let config = config.clone();
            let counter = counter.clone();
            let semaphore = semaphore.clone();
            let job_name = job.fullDisplayName.clone();
            let job_url = job.url.clone();

            let permit = semaphore.acquire_owned().await?;

            futures.push(tokio::spawn(async move {
                let _permit = permit; // hold semaphore until this task finishes

                sleep(Duration::from_millis(100)).await; // delay before fetching

                match jenkins::get_job_status_details(&config, &job_name).await {
                    Ok(status) => {
                        let result = status.result.as_deref();
                        if result == Some("FAILURE") {
                            let i = counter.fetch_add(1, Ordering::Relaxed);
                            print_job_status_details(i, &job_name, &job_url, "FAILURE");

                            if config.retry {
                                let _ = jenkins::post_retry_failed_job(&config, &job_name,  &job_url).await;
                            }
                        } else if result.is_none() && status.inProgress {
                            let i = counter.fetch_add(1, Ordering::Relaxed);
                            print_job_status_details(i, &job_name, &job_url, "IN_PROGRESS");
                        }
                    }
                    Err(e) => eprintln!("⚠️  {} => {}", job_name, e),
                }
            }));
        }

        futures::future::join_all(futures).await;
    }
}
