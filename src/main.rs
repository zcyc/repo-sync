use clap::Parser;
use job_scheduler_ng::{Job, JobScheduler};
use serde::{Deserialize, Serialize};
use std::{env, fs::File, io::BufReader, process::Command, time::Duration};

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(
        short,
        long,
        value_parser,
        help = "source repo, eg: https://github.com/zcyc/repo-sync.git"
    )]
    source: Option<String>,

    #[clap(
        short,
        long,
        value_parser,
        multiple_values = true,
        help = "target repo, eg: https://github.com/zcyc/repo-sync.git"
    )]
    target: Option<Vec<String>>,

    #[clap(
        short,
        long,
        value_parser,
        help = "config file path, eg: ./config.json"
    )]
    file: Option<String>,

    #[clap(
        short,
        long,
        value_parser,
        help = "crontab string, eg: '0 * * * * ? *'"
    )]
    crontab: Option<String>,
}

fn main() {
    // load config
    let args = Args::parse();
    let mut config = Config { jobs: Vec::new() };
    if args.source.is_some() && args.target.is_some() {
        config.jobs.push(Item {
            source: args.source.unwrap(),
            target: args.target.unwrap(),
            crontab: args.crontab,
        })
    } else if args.file.is_some() {
        let file = get_config(args.file.unwrap());
        config.jobs = file.jobs;
    } else {
        panic!("source, target, file 参数不能同时为空!");
    };
    println!("config {:#?}", config);

    // create cron schedule
    let mut schedule = JobScheduler::new();
    for config_item in config.jobs {
        // run one time
        if config_item.crontab.is_none() {
            sync(config_item);
            return;
        }
        // add to cron jobs
        let crontab_str = config_item.clone().crontab.unwrap();
        println!("crontab_str {:#?}", crontab_str);
        schedule.add(Job::new(crontab_str.parse().unwrap(), move || {
            sync(config_item.clone())
        }));
    }

    // start crontab scheduler
    loop {
        schedule.tick();
        std::thread::sleep(Duration::from_millis(500));
    }
}

// core logic
fn sync(config: Item) {
    // 1.git clone
    let clone_output = Command::new("sh")
        .args(["-c", &git_clone_cmd(config.source.clone())])
        .output();
    println!("{:?}", clone_output);

    // 2.cd repo directory
    assert!(env::set_current_dir(&current_dir(config.source.clone())).is_ok());

    // 3.git pull
    let pull_output = Command::new("sh")
        .args(["-c", &git_pull_cmd()])
        .output()
        .expect("failed to execute pull process");
    println!("{:?}", pull_output);

    // 3.git remote add
    config.target.iter().enumerate().for_each(|(i, x)| {
        let remote_add_output = Command::new("sh")
            .args(["-c", &git_remote_add_cmd(i, x.to_string())])
            .output()
            .expect("failed to execute remote add process");
        println!("{:?}", remote_add_output);
    });

    // 4.git push
    config.target.iter().enumerate().for_each(|(i, _x)| {
        let push_output = Command::new("sh")
            .args(["-c", &git_push_cmd(i)])
            .output()
            .expect("failed to execute push process");
        println!("{:?}", push_output);
    });
}

fn git_clone_cmd(repo_url: String) -> String {
    format!("git clone {}", repo_url)
}

fn git_remote_add_cmd(index: usize, repo_url: String) -> String {
    format!("git remote add target{} {}", index, repo_url)
}

fn git_pull_cmd() -> String {
    "git pull".to_string()
}

fn git_push_cmd(index: usize) -> String {
    format!("git push target{}", index)
}

fn current_dir(repo_url: String) -> String {
    let repo_name_git = repo_url.split('/').last().unwrap();
    let repo_name = repo_name_git.split('.').next().unwrap();
    format!("./{}", repo_name)
}

pub fn get_config(path: String) -> Config {
    let file = File::open(path).unwrap();
    let reader = BufReader::new(file);
    serde_json::from_reader(reader).unwrap()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Item {
    pub source: String,
    pub target: Vec<String>,
    pub crontab: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub jobs: Vec<Item>,
}
