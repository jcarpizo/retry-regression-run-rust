use curl::easy::Easy;
use futures::stream::{FuturesUnordered, StreamExt};
use regex::Regex;
use serde::{de::DeserializeOwned, Deserialize};
use std::env;
use tokio::time::{self, Duration};
use urlencoding::encode;
use anyhow::{Result, anyhow};

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

const JENKINS_URLS: [&str; 2] = [
    "http://10.40.1.11:8080/view/Seeking%20API%20Test",
    "http://10.40.1.11:8080/view/Seeking%20Functional",
];

const USERNAME: &str = "jasper";
const PASSWORD: &str = "11bbc6af0dd54621a97ecb0d601b95a8d1";

struct Config<'a> {
    retry: bool,
    base_url: &'a str,
    retry_interval: u64,
}

fn create_easy_handle(url: &str) -> Result<Easy, curl::Error> {
    let mut handle = Easy::new();
    handle.url(url)?;
    handle.username(USERNAME)?;
    handle.password(PASSWORD)?;
    Ok(handle)
}

async fn fetch_json<T: DeserializeOwned>(url: &str) -> Result<T> {
    let mut data = Vec::new();
    let mut handle = create_easy_handle(url)?;
    {
        let mut transfer = handle.transfer();
        transfer.write_function(|new_data| {
            data.extend_from_slice(new_data);
            Ok(new_data.len())
        })?;
        transfer.perform()?;
    }

    let body = std::str::from_utf8(&data).unwrap_or("").to_string();

    serde_json::from_str(&body).or_else(|_| {
        serde_json::from_str(r#"{"result": null, "inProgress": true}"#).map_err(|e| e.into())
    })
}

async fn get_jobs(config: &Config<'_>) -> Result<JobList> {
    let url = format!("{}/api/json?tree=jobs[name,disabled,inQueue,url]", config.base_url);
    fetch_json(&url).await
}

async fn get_job_status(config: &Config<'_>, job_name: &str) -> Result<JobResult> {
    let encoded = encode(job_name);
    let url = format!("{}/job/{}/lastBuild/api/json", config.base_url, encoded);
    fetch_json(&url).await
}

async fn retry_job(config: &Config<'_>, job_name: &str) -> Result<()> {
    let encoded = encode(job_name);
    let url = format!("{}/job/{}/buildWithParameters", config.base_url, encoded);
    let mut handle = create_easy_handle(&url)?;
    handle.post(true)?;
    handle.perform()?;
    println!("🔁 Retried job: {}\n", job_name);
    Ok(())
}

fn parse_arguments() -> Config<'static> {
    let args: Vec<String> = env::args().collect();
    let url_index = args.iter().position(|arg| arg == "--url")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(1);

    let retry_interval = args.iter().position(|arg| arg == "--interval")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(180);

    let retry = args.contains(&"--retry".to_string());

    Config {
        retry,
        base_url: JENKINS_URLS[url_index],
        retry_interval,
    }
}

fn clear_screen() {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "cls"])
            .status();
    }

    #[cfg(unix)]
    {
        // ANSI escape code to clear the screen and move cursor to top-left
        print!("\x1B[2J\x1B[H");
        use std::io::{stdout, Write};
        let _ = stdout().flush(); // Ensure the clear command is immediately reflected
    }
}


#[tokio::main]
async fn main() -> Result<()> {
    let config = parse_arguments();

    let exclude = Regex::new(r"(?i)(_start_test_suite|-test-suite-start)")?;
    let mut interval = time::interval(Duration::from_secs(config.retry_interval));

    let excluded_names = [
        "02-(SEEKING-AUTHENTICATION)-Admin-Login-Google-Account_Test",
        "01-(SEEKING-OTP-AUTHENTICATION)-OTP-Login-Passwordless_Test",
        // ... (shortened for brevity)
        "06-03-(SEEKING-THANOS)-Thanos-FraudML-Auto-Moderation-Disabled_Test",
    ];

    println!("🟢 Monitoring Jenkins at: {}\n⏱  Every {} seconds\n", config.base_url, config.retry_interval);

    loop {
        interval.tick().await;
        clear_screen();

        let job_list = get_jobs(&config).await?;

        let jobs_to_check: Vec<_> = job_list.jobs.into_iter()
            .filter(|job| {
                !job.disabled
                    && !job.inQueue
                    && !exclude.is_match(&job.name)
                    && !excluded_names.contains(&job.name.as_str())
            })
            .collect();

        let mut futures = FuturesUnordered::new();

        for job in jobs_to_check {
            let config = &config;
            let name = job.name.clone();
            let url = job.url.clone();
            futures.push(async move {
                match get_job_status(config, &name).await {
                    Ok(status) => {
                        if status.result.as_deref() == Some("FAILURE") {
                            println!("❌ Job: {}\n   In Progress: {}\n   Url: {}\n", name, status.inProgress, url);
                            if config.retry {
                                let _ = retry_job(config, &name).await;
                            }
                        } else if status.result.is_none() && status.inProgress {
                            println!("⏳ Job: {} is still IN PROGRESS\n", name);
                        }
                    }
                    Err(e) => eprintln!("⚠️  Error fetching job status: {}: {}", name, e),
                }
            });
        }

        while futures.next().await.is_some() {}
    }
}
