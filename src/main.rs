use clap::Parser;
use job_scheduler_ng::{Job, JobScheduler};
use repo_sync::{get_config, sync, Config, Item};
use std::time::Duration;

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
