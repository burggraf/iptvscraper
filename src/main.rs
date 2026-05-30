use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use clap::Parser;
use regex::Regex;
use reqwest::Client;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, fs, path::Path, time::Duration};
use url::Url;

const LABEL_URL: &str = "https://www.iptvregion.eu.org/search/label/XTREAM";
const STATE_FILE: &str = ".iptvscraper-last-run.json";
const DEFAULT_NTFY_TOPIC_URL: &str = "https://ntfy.sh/mb-iptvscraper";

#[derive(Parser, Debug)]
#[command(version, about = "Scrape IPTV Xtream playlists and report priority accounts")]
struct Args {
    /// Process only this URL and exit.
    url: Option<String>,

    /// Override cutoff lookback hours.
    #[arg(long)]
    since_hours: Option<i64>,

    /// ntfy.sh topic URL for priority playlist notifications.
    #[arg(long, default_value = DEFAULT_NTFY_TOPIC_URL)]
    ntfy_topic: String,
}

#[derive(Debug, Clone)]
struct Entry {
    url: String,
    published: Option<DateTime<Local>>,
}

#[derive(Debug, Clone)]
struct PlaylistInput {
    source_entry_title: String,
    source_entry_url: String,
    server: String,
    username: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct ErrorInfo {
    stage: String,
    code: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct PlaylistResult {
    scraped_at_local: String,
    source_entry_title: String,
    source_entry_url: String,
    server: String,
    username: String,
    password: String,
    priority_playlist: bool,
    streams_allowed: Option<u64>,
    streams_in_use: Option<u64>,
    expiration_date: Option<String>,
    live_channels_supported: Option<u64>,
    movies_supported: Option<u64>,
    series_supported: Option<u64>,
    live_channel_categories: Vec<String>,
    errors: Vec<ErrorInfo>,
}

#[derive(Debug, Deserialize)]
struct PlayerApiResponse {
    user_info: Option<UserInfo>,
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    max_connections: Option<serde_json::Value>,
    active_cons: Option<serde_json::Value>,
    exp_date: Option<serde_json::Value>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("iptvscraper/0.1")
        .build()?;

    let cutoff = compute_cutoff(&args)?;

    let single_url_mode = args.url.is_some();
    let queue_inputs = if let Some(url) = args.url.clone() {
        parse_url_as_inputs(&client, &url).await?
    } else {
        let entries = scrape_label_entries(&client, LABEL_URL).await?;
        let mut urls = Vec::new();
        for e in entries {
            if e.published.map(|p| p > cutoff).unwrap_or(true) {
                urls.push(e.url);
            }
        }
        let mut inputs = Vec::new();
        for url in urls {
            inputs.extend(parse_url_as_inputs(&client, &url).await?);
        }
        inputs
    };

    let mut queue: VecDeque<PlaylistInput> = queue_inputs.into();
    let total = queue.len();
    let mut processed = 0usize;
    let mut priority_written = 0usize;
    fs::create_dir_all("playlists").ok();
    fs::create_dir_all("priority-playlists").ok();

    while let Some(item) = queue.pop_front() {
        processed += 1;
        let remaining = queue.len();
        println!("processed {processed}/{total}, remaining {remaining}");
        let result = process_playlist(&client, &item).await;
        let is_priority = result.priority_playlist;
        let folder = if is_priority { "priority-playlists" } else { "playlists" };
        if is_priority {
            priority_written += 1;
        }
        let file_name = make_file_name(&item.server, &item.username, result.streams_allowed, result.expiration_date.as_deref());
        let path = Path::new(folder).join(file_name);
        fs::write(&path, serde_json::to_string_pretty(&result)?)?;
    }

    if total > 0 {
        println!("summary: processed {processed} playlists; wrote {priority_written} priority playlists");
        if priority_written > 0 {
            notify_ntfy(&client, &args.ntfy_topic, processed, priority_written).await;
        }
    }

    if !single_url_mode {
        write_last_run(Local::now())?;
    }
    Ok(())
}

fn compute_cutoff(args: &Args) -> Result<DateTime<Local>> {
    if let Some(h) = args.since_hours {
        return Ok(Local::now() - chrono::Duration::hours(h));
    }
    if let Ok(text) = fs::read_to_string(STATE_FILE) {
        if let Some(ts) = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("last_run_local").and_then(|s| s.as_str()).map(|s| s.to_string()))
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        {
            return Ok(ts.with_timezone(&Local));
        }
    }
    Ok(Local::now() - chrono::Duration::hours(24))
}

fn write_last_run(ts: DateTime<Local>) -> Result<()> {
    fs::write(
        STATE_FILE,
        serde_json::to_string_pretty(&serde_json::json!({"last_run_local": ts.to_rfc3339()}))?,
    )?;
    Ok(())
}

async fn scrape_label_entries(client: &Client, url: &str) -> Result<Vec<Entry>> {
    let html = client.get(url).send().await?.text().await?;
    let doc = Html::parse_document(&html);
    let entry_sel = Selector::parse("a[href]").unwrap();
    let mut entries = Vec::new();
    let re = Regex::new(r"/\d{4}/\d{2}/.+\.html$").unwrap();
    for a in doc.select(&entry_sel) {
        let href = a.value().attr("href").unwrap_or("");
        if !re.is_match(href) { continue; }
        let title = link_title(&a);
        let published = title.as_deref().and_then(parse_entry_date).or_else(|| parse_entry_date_from_url(href));
        entries.push(Entry { url: absolutize(url, href)?, published });
    }
    entries.sort_by_key(|e| e.published);
    entries.dedup_by(|a, b| a.url == b.url);
    Ok(entries)
}

fn link_title(a: &scraper::ElementRef<'_>) -> Option<String> {
    let text = a.text().collect::<Vec<_>>().join(" ").trim().to_string();
    if !text.is_empty() {
        return Some(text);
    }

    let img_sel = Selector::parse("img").ok()?;
    a.select(&img_sel)
        .find_map(|img| img.value().attr("alt").or_else(|| img.value().attr("title")))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn parse_entry_date(title: &str) -> Option<DateTime<Local>> {
    let re = Regex::new(r"\{\s*(\d{1,2})/([A-Za-zÀ-ÿ]+)/([0-9]{4})\s*\}").ok()?;
    let caps = re.captures(title)?;
    entry_date_at_end_of_day(&caps[1], &caps[2], &caps[3])
}

fn parse_entry_date_from_url(url: &str) -> Option<DateTime<Local>> {
    let re = Regex::new(r"iptv-(\d{1,2})([a-zA-ZÀ-ÿ]+)(\d{4})").ok()?;
    let caps = re.captures(url)?;
    entry_date_at_end_of_day(&caps[1], &caps[2], &caps[3])
}

fn entry_date_at_end_of_day(day: &str, month_name: &str, year: &str) -> Option<DateTime<Local>> {
    let day: u32 = day.parse().ok()?;
    let month = parse_month(month_name)?;
    let year: i32 = year.parse().ok()?;
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let dt = date.and_hms_opt(23, 59, 59)?;
    Local.from_local_datetime(&dt).single()
}

fn parse_month(name: &str) -> Option<u32> {
    match name.to_uppercase().as_str() {
        "JAN" | "JANV" | "JANVIER" => Some(1),
        "FEV" | "FÉV" | "FEVR" | "FÉVR" | "FEVRIER" | "FÉVRIER" => Some(2),
        "MAR" | "MARS" => Some(3),
        "AVR" | "AVRIL" => Some(4),
        "MAI" => Some(5),
        "JUN" | "JUIN" => Some(6),
        "JUL" | "JUIL" | "JUILLET" => Some(7),
        "AOU" | "AOÛ" | "AOUT" | "AOÛT" => Some(8),
        "SEP" | "SEPT" | "SEPTEMBRE" => Some(9),
        "OCT" | "OCTOBRE" => Some(10),
        "NOV" | "NOVEMBRE" => Some(11),
        "DEC" | "DÉC" | "DECEMBRE" | "DÉCEMBRE" => Some(12),
        _ => None,
    }
}

async fn parse_url_as_inputs(client: &Client, url: &str) -> Result<Vec<PlaylistInput>> {
    let html = client.get(url).send().await?.text().await?;
    let doc = Html::parse_document(&html);
    let row_sel = Selector::parse("table tr").unwrap();
    let cell_sel = Selector::parse("td").unwrap();
    let mut out = Vec::new();
    for row in doc.select(&row_sel) {
        let cells: Vec<String> = row.select(&cell_sel).map(|c| c.text().collect::<Vec<_>>().join(" ").trim().to_string()).collect();
        if cells.len() < 3 { continue; }
        if cells[0].to_lowercase().contains("server") { continue; }
        if cells[0].is_empty() || cells[1].is_empty() || cells[2].is_empty() { continue; }
        out.push(PlaylistInput {
            source_entry_title: page_title(&doc).unwrap_or_else(|| url.to_string()),
            source_entry_url: url.to_string(),
            server: cells[0].clone(),
            username: cells[1].clone(),
            password: cells[2].clone(),
        });
    }
    Ok(out)
}

fn page_title(doc: &Html) -> Option<String> {
    let sel = Selector::parse("title").ok()?;
    doc.select(&sel).next().map(|n| n.text().collect::<Vec<_>>().join(" ").trim().to_string()).filter(|s| !s.is_empty())
}

async fn process_playlist(client: &Client, item: &PlaylistInput) -> PlaylistResult {
    let mut errors = Vec::new();
    let scraped_at_local = Local::now().to_rfc3339();
    let base = normalize_server(&item.server);
    let api = format!("{base}/player_api.php?username={}&password={}", urlencoding::encode(&item.username), urlencoding::encode(&item.password));

    let mut streams_allowed = None;
    let mut streams_in_use = None;
    let mut expiration_date = None;

    match client.get(&api).send().await {
        Ok(resp) => match resp.text().await {
            Ok(text) => match serde_json::from_str::<PlayerApiResponse>(&text) {
                Ok(resp) => {
                    if let Some(ui) = resp.user_info {
                        streams_allowed = value_to_u64(ui.max_connections);
                        streams_in_use = value_to_u64(ui.active_cons);
                        expiration_date = ui.exp_date.as_ref().and_then(format_expiration_date);
                    }
                }
                Err(e) => errors.push(err("player_api", "parse_error", e.to_string())),
            },
            Err(e) => errors.push(err("player_api", "read_error", e.to_string())),
        },
        Err(e) => errors.push(err("player_api", "request_error", e.to_string())),
    }

    let live_categories = fetch_category_names(client, &base, &item.username, &item.password, "get_live_categories", "live_categories", &mut errors).await;
    let live_channels_supported = fetch_count(client, &base, &item.username, &item.password, "get_live_streams", "live_streams", &mut errors).await;
    let movies_supported = fetch_count(client, &base, &item.username, &item.password, "get_vod_streams", "vod_streams", &mut errors).await;
    let series_supported = fetch_count(client, &base, &item.username, &item.password, "get_series", "series", &mut errors).await;

    let priority_playlist = is_priority_playlist(streams_allowed, expiration_date.as_deref(), &live_categories);

    PlaylistResult {
        scraped_at_local,
        source_entry_title: item.source_entry_title.clone(),
        source_entry_url: item.source_entry_url.clone(),
        server: item.server.clone(),
        username: item.username.clone(),
        password: item.password.clone(),
        priority_playlist,
        streams_allowed,
        streams_in_use,
        expiration_date,
        live_channels_supported,
        movies_supported,
        series_supported,
        live_channel_categories: live_categories,
        errors,
    }
}

async fn fetch_category_names(client: &Client, base: &str, user: &str, pass: &str, action: &str, stage: &str, errors: &mut Vec<ErrorInfo>) -> Vec<String> {
    let url = format!("{base}/player_api.php?username={}&password={}&action={}", urlencoding::encode(user), urlencoding::encode(pass), action);
    match fetch_json_value(client, &url).await {
        Ok(v) => {
            if let Some(arr) = v.as_array() {
                arr.iter().filter_map(|i| i.get("category_name").or_else(|| i.get("name")).or_else(|| i.get("title")).and_then(|x| x.as_str()).map(|s| s.to_string())).collect()
            } else { Vec::new() }
        }
        Err(e) => { errors.push(err(stage, "request_or_parse_error", e.to_string())); Vec::new() }
    }
}

async fn fetch_count(client: &Client, base: &str, user: &str, pass: &str, action: &str, stage: &str, errors: &mut Vec<ErrorInfo>) -> Option<u64> {
    let url = format!("{base}/player_api.php?username={}&password={}&action={}", urlencoding::encode(user), urlencoding::encode(pass), action);
    match fetch_json_value(client, &url).await {
        Ok(v) => v.as_array().map(|a| a.len() as u64),
        Err(e) => { errors.push(err(stage, "request_or_parse_error", e.to_string())); None }
    }
}

async fn fetch_json_value(client: &Client, url: &str) -> Result<serde_json::Value> {
    let text = client.get(url).send().await?.text().await?;
    Ok(serde_json::from_str(&text).with_context(|| format!("bad json: {text}"))?)
}

fn err(stage: &str, code: &str, message: impl Into<String>) -> ErrorInfo {
    ErrorInfo { stage: stage.to_string(), code: code.to_string(), message: message.into() }
}

fn value_to_u64(v: Option<serde_json::Value>) -> Option<u64> {
    v.and_then(|x| match x {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    })
}

fn format_expiration_date(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().and_then(|secs| Local.timestamp_opt(secs, 0).single()).map(|dt| dt.to_rfc3339()),
        serde_json::Value::String(s) => {
            if let Ok(secs) = s.parse::<i64>() {
                Local.timestamp_opt(secs, 0).single().map(|dt| dt.to_rfc3339())
            } else if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                Some(dt.with_timezone(&Local).to_rfc3339())
            } else {
                Some(s.clone())
            }
        }
        _ => None,
    }
}

fn normalize_server(server: &str) -> String {
    if server.starts_with("http://") || server.starts_with("https://") { server.trim_end_matches('/').to_string() } else { format!("http://{}", server.trim_end_matches('/')) }
}

fn absolutize(base: &str, href: &str) -> Result<String> {
    Ok(Url::parse(base)?.join(href)?.to_string())
}

fn make_file_name(server: &str, user: &str, streams_allowed: Option<u64>, expiration_date: Option<&str>) -> String {
    let local = Local::now().format("%Y%m%d-%H%M%S");
    let streams = streams_allowed.unwrap_or(0);
    let days_left = days_left_token(expiration_date);
    format!("{local}-{streams}-{days_left}-{}-{}.json", slug(server), slug(user))
}

fn days_left_token(expiration_date: Option<&str>) -> i64 {
    let Some(expiration_date) = expiration_date else { return 9999; };
    if expiration_date.trim().is_empty() { return 9999; }
    let Ok(exp) = DateTime::parse_from_rfc3339(expiration_date) else { return 9999; };
    let exp = exp.with_timezone(&Local);
    let epoch = Local.timestamp_opt(0, 0).single();
    if epoch == Some(exp) { return 9999; }
    let now = Local::now();
    if exp <= now { return 0; }
    (exp - now).num_days()
}

fn is_priority_playlist(streams_allowed: Option<u64>, expiration_date: Option<&str>, live_categories: &[String]) -> bool {
    matches!(streams_allowed, Some(n) if n >= 2)
        && expiration_is_priority(expiration_date)
        && has_priority_category(live_categories)
}

fn expiration_is_priority(expiration_date: Option<&str>) -> bool {
    match expiration_date {
        None => true,
        Some(s) if s.trim().is_empty() => true,
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|exp| (exp.with_timezone(&Local) - Local::now()).num_days() >= 180)
            .unwrap_or(false),
    }
}

fn has_priority_category(live_categories: &[String]) -> bool {
    live_categories.iter().any(|c| {
        let lc = c.to_lowercase();
        c.contains("US") || c.contains("Usa") || lc.contains("locals")
    })
}

async fn notify_ntfy(client: &Client, topic_url: &str, processed: usize, priority_written: usize) {
    let body = format!("iptvscraper done: processed {processed} playlists; wrote {priority_written} priority playlists");
    let result = client
        .post(topic_url)
        .header("Title", "iptvscraper priority alert")
        .header("Priority", "default")
        .body(body)
        .send()
        .await;

    if let Err(e) = result {
        eprintln!("ntfy notify failed: {e}");
    }
}

fn slug(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>().trim_matches('-').to_string()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entry_date_from_title_as_end_of_day() {
        let dt = parse_entry_date("IPTV { 28/MAI/2026 } ✪~✪ Table").unwrap();
        assert_eq!(dt.format("%Y-%m-%dT%H:%M:%S").to_string(), "2026-05-28T23:59:59");
    }

    #[test]
    fn parses_entry_date_from_url_slug() {
        let dt = parse_entry_date_from_url("https://www.iptvregion.eu.org/2026/05/iptv-28mai2026-table-of-28-account.html").unwrap();
        assert_eq!(dt.format("%Y-%m-%dT%H:%M:%S").to_string(), "2026-05-28T23:59:59");
    }

    #[test]
    fn priority_playlist_needs_streams_expiration_and_us_or_locals_category() {
        let us = vec!["US Entertainment".to_string()];
        let usa = vec!["Usa Sports".to_string()];
        let locals = vec!["LOCALs".to_string()];
        let no_match = vec!["Canada".to_string()];

        assert!(!is_priority_playlist(Some(1), None, &us));
        assert!(is_priority_playlist(Some(2), None, &us));
        assert!(is_priority_playlist(Some(2), Some(""), &us));
        assert!(is_priority_playlist(Some(2), None, &usa));
        assert!(is_priority_playlist(Some(2), None, &locals));
        assert!(!is_priority_playlist(Some(2), None, &no_match));
    }
}
