use clap::Parser;
use std::io::{BufWriter, Write};
use web_fast_crawl::{CrawlOptions, Crawler};

#[derive(Parser)]
#[command(about = "Crawl HTML, browser-rendered pages, and JSON into a bounded semantic graph")]
struct Args {
    #[arg(required = true)]
    seeds: Vec<String>,
    #[arg(long, default_value_t = 100)]
    max_pages: usize,
    #[arg(long, default_value_t = 3)]
    max_depth: usize,
    #[arg(long, default_value_t = 4)]
    concurrency: usize,
    #[arg(long, default_value_t = 200)]
    interval_ms: u64,
    #[arg(long)]
    allow_origin: Vec<String>,
    #[arg(long)]
    allow_private_networks: bool,
    #[arg(long)]
    focus: Vec<String>,
    #[arg(long)]
    retain_documents: bool,
    /// Override the User-Agent consistently for HTTP requests and browser JS.
    #[arg(long)]
    user_agent: Option<String>,
    #[arg(long, value_parser = ["disabled", "auto", "always"], default_value = "disabled")]
    render: String,
    #[arg(long)]
    resource_origin: Vec<String>,
    #[arg(long)]
    browser_executable: Option<String>,
    #[arg(long, default_value = "chrome")]
    browser_channel: String,
    /// Use a visible browser and its native non-headless identity.
    #[arg(long)]
    headed: bool,
    #[arg(long)]
    browser_runner: Option<std::path::PathBuf>,
    #[arg(long, default_value_t = 2)]
    scroll_steps: usize,
    #[arg(long)]
    output: Option<std::path::PathBuf>,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut options = CrawlOptions {
        max_pages: args.max_pages,
        max_depth: args.max_depth,
        concurrency: args.concurrency,
        per_origin_concurrency: args.concurrency.min(2),
        min_origin_interval_ms: args.interval_ms,
        allowed_origins: args.allow_origin,
        allow_private_networks: args.allow_private_networks,
        focus_terms: args.focus,
        retain_documents: args.retain_documents,
        user_agent: args.user_agent,
        ..CrawlOptions::default()
    };
    options.browser.mode = match args.render.as_str() {
        "auto" => web_fast_crawl::browser::RenderMode::Auto,
        "always" => web_fast_crawl::browser::RenderMode::Always,
        _ => web_fast_crawl::browser::RenderMode::Disabled,
    };
    options.browser.resource_origins = args.resource_origin;
    options.browser.executable_path = args.browser_executable;
    options.browser.channel = args.browser_channel;
    options.browser.headless = !args.headed;
    options.browser.runner_path = args.browser_runner;
    options.browser.scroll_steps = args.scroll_steps;
    let report = Crawler::new(options)?.crawl(&args.seeds).await?;
    let output: Box<dyn Write> = match args.output {
        Some(path) => Box::new(std::fs::File::create(path)?),
        None => Box::new(std::io::stdout().lock()),
    };
    let mut output = BufWriter::new(output);
    serde_json::to_writer(&mut output, &report)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}
