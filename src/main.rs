use anyhow::{Result, Context};
use curl::easy::Easy;
use regex::Regex;
use serde::{Deserialize, de::DeserializeOwned};
use tokio::time::{self, Duration};
use urlencoding::encode;
use futures::stream::{FuturesUnordered, StreamExt};
use colored::Colorize;
use once_cell::sync::Lazy;
use reqwest::Client;
use std::{
    env,
    io::{stdout, Write},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

const USERNAME: &str = "jasper";
const TOKEN: &str = "1109f682436aa407902fc02e67f97d8e72";

const JENKINS_URLS: [&str; 3] = [
    "http://10.40.1.11:8080/view/Seeking%20API%20Test",
    "http://10.40.1.11:8080/view/Seeking%20Functional",
    "http://10.40.1.11:8080/view/SAMessage",
];

static EXCLUDED_MODULES: Lazy<Vec<&str>> = Lazy::new(|| vec![
    "SEEKING-OTP-AUTHENTICATION",
    "SEEKING-AUTO-MODERATE-PAS",
    "SEEKING-THANOS",
    "SEEKING-DRAGYN",
]);

static EXCLUDED_JOB_NAMES: Lazy<Vec<&str>> = Lazy::new(|| vec![
    "02-(SEEKING-AUTHENTICATION)-Admin-Login-Google-Account_Test",
    "24-(SEEKING-PAS)-PAS-Eye-Photo-Responses_Test",
    "26-(SEEKING-PAS)-Dragyn-Flagged-Terms-Text_Test",
    "03-(SEEKING-VERIFICATIONS)-Social-Facebook-Instagram-LinkedIn_Test",
    "12-(SEEKING-VERIFICATIONS)-Photo-Instagram_Test",
    "09-(SEEKING-PROFILE-WALL)-Profile-Wall-Facebook-Photo-Uploads",
    "20-(SEEKING-MEMBER-PROFILE)-Standard-Attractive-Premium-Label-Visibility_Test",
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

    pub async fn get_jobs(config: &Config) -> Result<JobList> {
        let url = format!(
            "{}/api/json?tree=jobs[name,fullDisplayName,disabled,inQueue,url]",
            config.base_url
        );
        fetch_json(&url).await
    }

    pub async fn get_job_status(config: &Config, job_name: &str) -> Result<JobResult> {
        let url = format!("{}/job/{}/lastBuild/api/json", config.base_url, encode(job_name));
        fetch_json(&url).await
    }

    pub async fn retry_job(config: &Config, job_name: &str) -> Result<()> {
        let url = format!("{}/job/{}/buildWithParameters", config.base_url, encode(job_name));
        let mut handle = create_easy_handle(&url)?;
        handle.post(true)?;
        handle.perform()?;
        println!("🔁 Retried: {}", job_name.black().on_bright_green());
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
            "🟢 Monitoring: {} => Seconds Interval: {}s\n😎 Created by: {} with using {} [ {} ]",
            config.base_url,
            config.retry_interval,
            "Jasper Carpizo",
            "RUST",
            "https://www.rust-lang.org"
        );

        let jobs = jenkins::get_jobs(&config).await?.jobs;

        let filtered_jobs = jobs.into_iter().filter(|j| {
            !j.disabled &&
                !j.inQueue &&
                !EXCLUDED_SUITE_START.is_match(&j.fullDisplayName) &&
                !REMOVED_SUFFIX_SPACE_HASH_NUMBER.is_match(&j.fullDisplayName) &&
                !EXCLUDED_JOB_NAMES.contains(&j.fullDisplayName.as_str()) &&
                !EXCLUDED_MODULES.iter().any(|prefix| j.fullDisplayName.contains(prefix))
        });

        let mut futures = FuturesUnordered::new();

        for job in filtered_jobs {
            let config = Arc::clone(&config);
            let counter = Arc::clone(&counter);

            futures.push(async move {
                match jenkins::get_job_status(&config, &job.fullDisplayName).await {
                    Ok(status) => {
                        let result = status.result.as_deref();
                        if result == Some("FAILURE") {
                            let i = counter.fetch_add(1, Ordering::Relaxed);
                            println!("{}. ❌  {} => {}", i, job.fullDisplayName.red(), job.url.italic().yellow());
                            if config.retry {
                                let _ = jenkins::retry_job(&config, &job.fullDisplayName).await;
                            }
                        } else if result.is_none() && status.inProgress {
                            let i = counter.fetch_add(1, Ordering::Relaxed);
                            println!("{}. 🟡 {} => {}", i, job.fullDisplayName.green(), job.url.italic().yellow());
                        }
                    }
                    Err(e) => eprintln!("⚠️  {} => {}", job.fullDisplayName, e),
                }
            });
        }

        while futures.next().await.is_some() {}
    }
}
