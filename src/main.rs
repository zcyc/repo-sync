use clap::Parser;
use job_scheduler_ng::{Job, JobScheduler};
use repo_sync::{get_config_vec, sync, Item};
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
    let config_vec = if args.source.is_some() && args.target.is_some() {
        vec![Item {
            source: args.source.unwrap(),
            target: args.target.unwrap(),
            crontab: args.crontab,
        }]
    } else if args.file.is_some() {
        get_config_vec(&args.file.unwrap())
    } else {
        panic!("command line flags cannot be empty at the same time!");
    };
    println!("config {:#?}", config_vec);

    // create cron schedule
    let mut schedule = JobScheduler::new();
    for config_item in config_vec {
        // run one time
        if config_item.crontab.is_none() {
            sync(&config_item);
            return;
        }
        // add to cron jobs
        let crontab_str = config_item.crontab.clone().unwrap();
        println!("crontab_str {:#?}", crontab_str);
        schedule.add(Job::new(crontab_str.parse().unwrap(), move || {
            sync(&config_item)
        }));
    }

    // start crontab scheduler
    loop {
        schedule.tick();
        std::thread::sleep(Duration::from_millis(500));
    }
}
