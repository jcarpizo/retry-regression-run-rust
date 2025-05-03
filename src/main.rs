use anyhow::{Result};
use curl::easy::Easy;
use futures::stream::{FuturesUnordered, StreamExt};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{de::DeserializeOwned, Deserialize};
use std::{env, io::{stdout, Write}};
use colored::Colorize;
use tokio::time::{self, Duration};
use urlencoding::encode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ============================
// Constants & Lazy Statics
// ============================

const JENKINS_URLS: [&str; 2] = [
    "http://10.40.1.11:8080/view/Seeking%20API%20Test",
    "http://10.40.1.11:8080/view/Seeking%20Functional",
];

const USERNAME: &str = "jasper";
const TOKEN: &str = "11bbc6af0dd54621a97ecb0d601b95a8d1";

static EXCLUDE_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(_start_test_suite|-test-suite-start)").unwrap()
});

static EXCLUDED_NAMES: Lazy<Vec<&str>> = Lazy::new(|| vec![
    "02-(SEEKING-AUTHENTICATION)-Admin-Login-Google-Account_Test",
    "01-(SEEKING-OTP-AUTHENTICATION)-OTP-Login-Passwordless_Test",
    "02-(SEEKING-OTP-AUTHENTICATION)-Extend-OTP-Expiry-Time_Test",
    "03-(SEEKING-OTP-AUTHENTICATION)-Join-Outsourcer_Test",
    "01-(SEEKING-AUTO-MODERATE-PAS)-Seeking-Onboarding-ProfileWall-Invalid-Username_Test",
    "02-(SEEKING-AUTO-MODERATE-PAS)-Admin-Sync-Profile-Moderate-Enabled_Test",
    "03-(SEEKING-AUTO-MODERATE-PAS)-Auto-Moderation-Settings-Toggle_Test",
    "01-(SEEKING-THANOS)-Thanos-Email-Suspend_Test",
    "02-(SEEKING-THANOS)-Thanos-IP-Suspend_Test",
    "03-(SEEKING-THANOS)-Thanos-Device-Fingerprinting-Suspend_Test",
    "04-(SEEKING-THANOS)-Thanos-Email-IP_Test",
    "05-(SEEKING-THANOS)-Thanos-Validation_Test",
    "06-00-(SEEKING-THANOS)-Thanos-FraudML-Auto-Moderation-Met-Criteria_Test",
    "06-01-(SEEKING-THANOS)-Thanos-FraudML-Auto-Moderation-Username-Not-Met-Criteria_Test",
    "06-02-(SEEKING-THANOS)-Thanos-FraudML-Auto-Moderation-Not-Met-Criteria-Selfie-Compare-Faces_Test",
    "06-03-(SEEKING-THANOS)-Thanos-FraudML-Auto-Moderation-Disabled_Test",
]);

// ============================
// Data Structures
// ============================

#[derive(Debug, Deserialize)]
struct JobList {
    jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct Job {
    name: String,
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

struct Config<'a> {
    retry: bool,
    base_url: &'a str,
    retry_interval: u64,
}

// ============================
// Jenkins API Utilities
// ============================

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
        let mut data = Vec::new();
        let mut handle = create_easy_handle(url)?;

        {
            let mut transfer = handle.transfer();
            transfer.write_function(|chunk| {
                data.extend_from_slice(chunk);
                Ok(chunk.len())
            })?;
            transfer.perform()?;
        }

        let body = std::str::from_utf8(&data)?;
        serde_json::from_str(body)
            .or_else(|_| {
                serde_json::from_str(r#"{"result": null, "inProgress": true}"#)
                    .map_err(Into::into)
            })
    }

    pub async fn get_jobs(config: &Config<'_>) -> Result<JobList> {
        let url = format!("{}/api/json?tree=jobs[name,disabled,inQueue,url]", config.base_url);
        fetch_json(&url).await
    }

    pub async fn get_job_status(config: &Config<'_>, job_name: &str) -> Result<JobResult> {
        let url = format!("{}/job/{}/lastBuild/api/json", config.base_url, encode(job_name));
        fetch_json(&url).await
    }

    pub async fn retry_job(config: &Config<'_>, job_name: &str) -> Result<()> {
        let url = format!("{}/job/{}/buildWithParameters", config.base_url, encode(job_name));
        let mut handle = create_easy_handle(&url)?;
        handle.post(true)?;
        handle.perform()?;
        println!("Retried job => {}", job_name.black().on_bright_green());
        Ok(())
    }
}

// ============================
// Helpers
// ============================

fn clear_screen() {
    #[cfg(windows)]
    let _ = Command::new("cmd").args(["/C", "cls"]).status();

    #[cfg(unix)]
    {
        print!("\x1B[2J\x1B[H");
        let _ = stdout().flush();
    }
}

fn parse_arguments() -> Config<'static> {
    let args: Vec<String> = env::args().collect();

    let url_index = args.windows(2)
        .find(|w| w[0] == "--url")
        .and_then(|w| w[1].parse::<usize>().ok())
        .map(|i| i.min(JENKINS_URLS.len() - 1))
        .unwrap_or(0);

    let retry_interval = args.windows(2)
        .find(|w| w[0] == "--interval")
        .and_then(|w| w[1].parse::<u64>().ok())
        .unwrap_or(180);

    let retry = args.contains(&"--retry".to_string());

    Config {
        retry,
        base_url: JENKINS_URLS[url_index],
        retry_interval,
    }
}

// ============================
// Main Logic
// ============================

#[tokio::main]
async fn main() -> Result<()> {
    let config = parse_arguments();
    let mut interval = time::interval(Duration::from_secs(config.retry_interval));
    let counter = Arc::new(AtomicUsize::new(1));

    loop {
        interval.tick().await;
        clear_screen();
        counter.store(0, Ordering::Relaxed);
        println!("🟢 Monitoring Jenkins: {}\n⏱ Interval: {} seconds\n", config.base_url, config.retry_interval);

        let jobs = jenkins::get_jobs(&config).await?.jobs;

        let filtered_jobs: Vec<_> = jobs.into_iter()
            .filter(|j| {
                !j.disabled &&
                    !j.inQueue &&
                    !EXCLUDE_REGEX.is_match(&j.name) &&
                    !EXCLUDED_NAMES.contains(&j.name.as_str())
            })
            .collect();

        let mut futures = FuturesUnordered::new();

        for job in filtered_jobs {
            let config = &config;
            let name = job.name.clone();
            let url = job.url.clone();
            let counter = Arc::clone(&counter);

            futures.push(async move {
                match jenkins::get_job_status(config, &name).await {
                    Ok(status) if matches!(status.result.as_deref(), Some("FAILURE")) => {
                        let current = counter.fetch_add(1, Ordering::Relaxed);
                        println!("{}. job: {} => url: {}", current, name.red(), url.italic().yellow());
                        if config.retry {
                            let _ = jenkins::retry_job(config, &name).await;
                        }
                    }
                    Ok(status) if status.result.is_none() && status.inProgress => {
                        let current = counter.fetch_add(1, Ordering::Relaxed);
                        println!("{}. job: {} => url: {}", current, name.green(), url.italic().yellow());
                    }
                    Err(e) => eprintln!("⚠️  Error: {}: {}", name, e),
                    _ => {}
                }
            });
        }

        while futures.next().await.is_some() {}
    }
}
