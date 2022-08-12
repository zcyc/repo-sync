use std::{env, process::Command};

use clap::Parser;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(
        short,
        long,
        value_parser,
        help = "source repo url, like: https://github.com/zcyc/repo-sync.git"
    )]
    src: String,

    #[clap(
        short,
        long,
        value_parser,
        help = "target repo url, like: https://github.com/zcyc/repo-sync.git"
    )]
    target: String,
}

fn main() {
    let args = Args::parse();

    // 1.git clone
    let clone_output = Command::new("sh")
        .args(["-c", &git_clone_cmd(args.src.clone())])
        .output();
    println!("{:?}", clone_output);
    // 2.cd repo directory
    assert!(env::set_current_dir(&current_dir(args.src.clone())).is_ok());
    // 3.git pull
    let pull_output = Command::new("sh")
        .args(["-c", &git_pull_cmd()])
        .output()
        .expect("failed to execute pull process");
    println!("{:?}", pull_output);
    // 3.git remote add
    let remote_add_output = Command::new("sh")
        .args(["-c", &git_remote_add_cmd(args.target.clone())])
        .output()
        .expect("failed to execute remote add process");
    println!("{:?}", remote_add_output);
    // 4.git push
    let push_output = Command::new("sh")
        .args(["-c", &git_push_cmd()])
        .output()
        .expect("failed to execute push process");
    println!("{:?}", push_output);
}

fn git_clone_cmd(repo_url: String) -> String {
    format!("git clone {}", repo_url)
}

fn git_remote_add_cmd(repo_url: String) -> String {
    format!("git remote add target {}", repo_url)
}

fn git_pull_cmd() -> String {
    "git pull".to_string()
}

fn git_push_cmd() -> String {
    "git push target".to_string()
}

fn current_dir(repo_url: String) -> String {
    let repo_name_git = repo_url.split('/').last().unwrap();
    let repo_name = repo_name_git.split('.').next().unwrap();
    format!("./{}", repo_name)
}
