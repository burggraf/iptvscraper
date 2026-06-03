use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use clap::Parser;
use regex::Regex;
use reqwest::{
    Client,
    header::{ACCEPT_ENCODING, CONTENT_TYPE, RANGE},
};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    path::Path,
    time::Duration,
};
use url::Url;

const LABEL_URL: &str = "https://www.iptvregion.eu.org/search/label/XTREAM";
const STATE_FILE: &str = ".iptvscraper-last-run.json";
const DEFAULT_NTFY_TOPIC_URL: &str = "https://ntfy.sh/mb-iptvscraper";

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Scrape IPTV Xtream playlists and report priority accounts"
)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    quality_test: Option<QualityTest>,
    errors: Vec<ErrorInfo>,
}

#[derive(Debug, Clone)]
struct LiveCategory {
    category_id: String,
    category_name: String,
}

#[derive(Debug, Clone)]
struct LiveStream {
    name: String,
    stream_id: String,
    category_id: String,
    container_extension: Option<String>,
}

#[derive(Debug, Serialize)]
struct QualityTest {
    enabled: bool,
    sample_size: usize,
    candidates: usize,
    tested: usize,
    passed: usize,
    failed: usize,
    pass_rate: f64,
    channels: Vec<QualityProbeResult>,
}

#[derive(Debug, Serialize)]
struct QualityProbeResult {
    name: String,
    stream_id: String,
    category_name: String,
    url: String,
    ok: bool,
    status: Option<u16>,
    content_type: Option<String>,
    bytes_read: usize,
    reason: String,
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
    let mut skipped = 0usize;
    let mut written = 0usize;
    let mut priority_written = 0usize;
    fs::create_dir_all("playlists").ok();
    fs::create_dir_all("priority-playlists").ok();
    let seen_inputs = load_seen_inputs(&["playlists", "priority-playlists"]);

    while let Some(item) = queue.pop_front() {
        let remaining = queue.len();
        if seen_inputs.contains(&input_key(&item)) {
            skipped += 1;
            println!(
                "skipped {skipped}/{total}, processed {processed}, remaining {remaining}: already exists"
            );
            continue;
        }
        processed += 1;
        println!("processed {processed}/{total}, skipped {skipped}, remaining {remaining}");
        let result = process_playlist(&client, &item).await;
        let is_priority = result.priority_playlist;
        let folder = if is_priority {
            "priority-playlists"
        } else {
            "playlists"
        };
        let suffix = playlist_file_suffix(
            &item.server,
            &item.username,
            result.streams_allowed,
            result.expiration_date.as_deref(),
        );
        if playlist_file_exists(&suffix, "playlists")
            || playlist_file_exists(&suffix, "priority-playlists")
        {
            skipped += 1;
            println!(
                "skipped {skipped}/{total}, processed {processed}, remaining {remaining}: output file exists"
            );
            continue;
        }
        let file_name = make_file_name(
            &item.server,
            &item.username,
            result.streams_allowed,
            result.expiration_date.as_deref(),
        );
        let path = Path::new(folder).join(file_name);
        fs::write(&path, serde_json::to_string_pretty(&result)?)?;
        written += 1;
        if is_priority {
            priority_written += 1;
        }
    }

    if total > 0 {
        println!(
            "summary: processed {processed} playlists; skipped {skipped}; wrote {written} playlists ({priority_written} priority)"
        );
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
            .and_then(|v| {
                v.get("last_run_local")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
            })
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

#[derive(Debug, Deserialize)]
struct SeenPlaylist {
    source_entry_url: String,
    server: String,
    username: String,
    password: String,
}

fn input_key(item: &PlaylistInput) -> String {
    format!(
        "{}\u{0}{}\u{0}{}\u{0}{}",
        item.source_entry_url, item.server, item.username, item.password
    )
}

fn load_seen_inputs(folders: &[&str]) -> HashSet<String> {
    let mut seen = HashSet::new();
    for folder in folders {
        let Ok(entries) = fs::read_dir(folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(text) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<SeenPlaylist>(&text) else {
                continue;
            };
            seen.insert(format!(
                "{}\u{0}{}\u{0}{}\u{0}{}",
                parsed.source_entry_url, parsed.server, parsed.username, parsed.password
            ));
        }
    }
    seen
}

async fn scrape_label_entries(client: &Client, url: &str) -> Result<Vec<Entry>> {
    let html = client.get(url).send().await?.text().await?;
    let doc = Html::parse_document(&html);
    let entry_sel = Selector::parse("a[href]").unwrap();
    let mut entries = Vec::new();
    let re = Regex::new(r"/\d{4}/\d{2}/.+\.html$").unwrap();
    for a in doc.select(&entry_sel) {
        let href = a.value().attr("href").unwrap_or("");
        if !re.is_match(href) {
            continue;
        }
        let title = link_title(&a);
        let published = title
            .as_deref()
            .and_then(parse_entry_date)
            .or_else(|| parse_entry_date_from_url(href));
        entries.push(Entry {
            url: absolutize(url, href)?,
            published,
        });
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
        .find_map(|img| {
            img.value()
                .attr("alt")
                .or_else(|| img.value().attr("title"))
        })
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
        let cells: Vec<String> = row
            .select(&cell_sel)
            .map(|c| c.text().collect::<Vec<_>>().join(" ").trim().to_string())
            .collect();
        if cells.len() < 3 {
            continue;
        }
        if cells[0].to_lowercase().contains("server") {
            continue;
        }
        if cells[0].is_empty() || cells[1].is_empty() || cells[2].is_empty() {
            continue;
        }
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
    doc.select(&sel)
        .next()
        .map(|n| n.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn process_playlist(client: &Client, item: &PlaylistInput) -> PlaylistResult {
    let mut errors = Vec::new();
    let scraped_at_local = Local::now().to_rfc3339();
    let base = normalize_server(&item.server);
    let api = format!(
        "{base}/player_api.php?username={}&password={}",
        urlencoding::encode(&item.username),
        urlencoding::encode(&item.password)
    );

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

    let live_categories =
        fetch_live_categories(client, &base, &item.username, &item.password, &mut errors).await;
    let live_category_names = live_categories
        .iter()
        .map(|c| c.category_name.clone())
        .collect::<Vec<_>>();
    let live_streams =
        fetch_live_streams(client, &base, &item.username, &item.password, &mut errors).await;
    let live_channels_supported = live_streams.as_ref().map(|streams| streams.len() as u64);
    let movies_supported = fetch_count(
        client,
        &base,
        &item.username,
        &item.password,
        "get_vod_streams",
        "vod_streams",
        &mut errors,
    )
    .await;
    let series_supported = fetch_count(
        client,
        &base,
        &item.username,
        &item.password,
        "get_series",
        "series",
        &mut errors,
    )
    .await;

    let priority_playlist = is_priority_playlist(
        streams_allowed,
        expiration_date.as_deref(),
        &live_category_names,
    );
    let quality_test = if priority_playlist {
        Some(
            quality_test_playlist(
                client,
                &base,
                &item.username,
                &item.password,
                &live_categories,
                live_streams.as_deref().unwrap_or(&[]),
            )
            .await,
        )
    } else {
        None
    };

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
        live_channel_categories: live_category_names,
        quality_test,
        errors,
    }
}

async fn fetch_live_categories(
    client: &Client,
    base: &str,
    user: &str,
    pass: &str,
    errors: &mut Vec<ErrorInfo>,
) -> Vec<LiveCategory> {
    let url = format!(
        "{base}/player_api.php?username={}&password={}&action=get_live_categories",
        urlencoding::encode(user),
        urlencoding::encode(pass)
    );
    match fetch_json_value(client, &url).await {
        Ok(v) => v
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|i| {
                        let category_id = i.get("category_id").and_then(value_to_string_ref)?;
                        let category_name = i
                            .get("category_name")
                            .or_else(|| i.get("name"))
                            .or_else(|| i.get("title"))
                            .and_then(|x| x.as_str())?
                            .to_string();
                        Some(LiveCategory {
                            category_id,
                            category_name,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Err(e) => {
            errors.push(err(
                "live_categories",
                "request_or_parse_error",
                e.to_string(),
            ));
            Vec::new()
        }
    }
}

async fn fetch_live_streams(
    client: &Client,
    base: &str,
    user: &str,
    pass: &str,
    errors: &mut Vec<ErrorInfo>,
) -> Option<Vec<LiveStream>> {
    let url = format!(
        "{base}/player_api.php?username={}&password={}&action=get_live_streams",
        urlencoding::encode(user),
        urlencoding::encode(pass)
    );
    match fetch_json_value(client, &url).await {
        Ok(v) => Some(
            v.as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|i| {
                            let stream_id = i.get("stream_id").and_then(value_to_string_ref)?;
                            let category_id = i.get("category_id").and_then(value_to_string_ref)?;
                            let name = i
                                .get("name")
                                .or_else(|| i.get("title"))
                                .and_then(|x| x.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let container_extension = i
                                .get("container_extension")
                                .and_then(|x| x.as_str())
                                .map(ToString::to_string);
                            Some(LiveStream {
                                name,
                                stream_id,
                                category_id,
                                container_extension,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
        Err(e) => {
            errors.push(err("live_streams", "request_or_parse_error", e.to_string()));
            None
        }
    }
}

async fn quality_test_playlist(
    client: &Client,
    base: &str,
    user: &str,
    pass: &str,
    categories: &[LiveCategory],
    streams: &[LiveStream],
) -> QualityTest {
    const SAMPLE_SIZE: usize = 5;

    let priority_ids = priority_category_ids(categories);
    let category_names = categories
        .iter()
        .map(|category| (category.category_id.clone(), category.category_name.clone()))
        .collect::<HashMap<_, _>>();
    let candidates = streams
        .iter()
        .filter(|stream| priority_ids.contains(&stream.category_id))
        .cloned()
        .collect::<Vec<_>>();
    let seed = format!("{base}|{user}|{pass}");
    let sampled = sample_streams(&candidates, SAMPLE_SIZE, &seed);

    let mut channels = Vec::new();
    for stream in sampled {
        let category_name = category_names
            .get(&stream.category_id)
            .cloned()
            .unwrap_or_default();
        channels.push(probe_stream(client, base, user, pass, stream, category_name).await);
    }

    let tested = channels.len();
    let passed = channels.iter().filter(|channel| channel.ok).count();
    let failed = tested.saturating_sub(passed);
    let pass_rate = if tested == 0 {
        0.0
    } else {
        passed as f64 / tested as f64
    };

    QualityTest {
        enabled: true,
        sample_size: SAMPLE_SIZE,
        candidates: candidates.len(),
        tested,
        passed,
        failed,
        pass_rate,
        channels,
    }
}

async fn probe_stream(
    client: &Client,
    base: &str,
    user: &str,
    pass: &str,
    stream: &LiveStream,
    category_name: String,
) -> QualityProbeResult {
    let url = stream_url(base, user, pass, stream);
    let mut result = QualityProbeResult {
        name: stream.name.clone(),
        stream_id: stream.stream_id.clone(),
        category_name,
        url: url.clone(),
        ok: false,
        status: None,
        content_type: None,
        bytes_read: 0,
        reason: "request_error".to_string(),
    };

    match client
        .get(&url)
        .header(RANGE, "bytes=0-65535")
        .header(ACCEPT_ENCODING, "identity")
        .timeout(Duration::from_secs(8))
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            result.status = Some(status.as_u16());
            result.content_type = resp
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(ToString::to_string);

            if !status.is_success() {
                result.reason = format!("http_status_{}", status.as_u16());
                return result;
            }

            match read_probe_bytes(resp, result.content_type.as_deref()).await {
                Ok(bytes) => {
                    result.bytes_read = bytes.len();
                    if let Some(reason) = sniff_media(result.content_type.as_deref(), &bytes) {
                        result.ok = true;
                        result.reason = reason;
                    } else if bytes.is_empty() {
                        result.reason = "empty_body".to_string();
                    } else {
                        result.reason = "unknown_content".to_string();
                    }
                }
                Err(e) => result.reason = format!("read_error: {e}"),
            }
        }
        Err(e) => result.reason = format!("request_error: {e}"),
    }

    result
}

async fn read_probe_bytes(
    mut resp: reqwest::Response,
    content_type: Option<&str>,
) -> Result<Vec<u8>, reqwest::Error> {
    const MAX_PROBE_BYTES: usize = 64 * 1024;

    let mut body = Vec::new();
    while body.len() < MAX_PROBE_BYTES {
        let Some(chunk) = resp.chunk().await? else {
            break;
        };
        let remaining = MAX_PROBE_BYTES - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);

        if sniff_media(content_type, &body).is_some() {
            break;
        }
    }

    Ok(body)
}

async fn fetch_count(
    client: &Client,
    base: &str,
    user: &str,
    pass: &str,
    action: &str,
    stage: &str,
    errors: &mut Vec<ErrorInfo>,
) -> Option<u64> {
    let url = format!(
        "{base}/player_api.php?username={}&password={}&action={}",
        urlencoding::encode(user),
        urlencoding::encode(pass),
        action
    );
    match fetch_json_value(client, &url).await {
        Ok(v) => v.as_array().map(|a| a.len() as u64),
        Err(e) => {
            errors.push(err(stage, "request_or_parse_error", e.to_string()));
            None
        }
    }
}

async fn fetch_json_value(client: &Client, url: &str) -> Result<serde_json::Value> {
    let text = client.get(url).send().await?.text().await?;
    Ok(serde_json::from_str(&text).with_context(|| format!("bad json: {text}"))?)
}

fn err(stage: &str, code: &str, message: impl Into<String>) -> ErrorInfo {
    ErrorInfo {
        stage: stage.to_string(),
        code: code.to_string(),
        message: message.into(),
    }
}

fn value_to_string_ref(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
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
        serde_json::Value::Number(n) => n
            .as_i64()
            .and_then(|secs| Local.timestamp_opt(secs, 0).single())
            .map(|dt| dt.to_rfc3339()),
        serde_json::Value::String(s) => {
            if let Ok(secs) = s.parse::<i64>() {
                Local
                    .timestamp_opt(secs, 0)
                    .single()
                    .map(|dt| dt.to_rfc3339())
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
    if server.starts_with("http://") || server.starts_with("https://") {
        server.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", server.trim_end_matches('/'))
    }
}

fn absolutize(base: &str, href: &str) -> Result<String> {
    Ok(Url::parse(base)?.join(href)?.to_string())
}

fn make_file_name(
    server: &str,
    user: &str,
    streams_allowed: Option<u64>,
    expiration_date: Option<&str>,
) -> String {
    format!(
        "{}{}",
        Local::now().format("%Y%m%d-%H%M%S"),
        playlist_file_suffix(server, user, streams_allowed, expiration_date)
    )
}

fn playlist_file_suffix(
    server: &str,
    user: &str,
    streams_allowed: Option<u64>,
    expiration_date: Option<&str>,
) -> String {
    let streams = streams_allowed.unwrap_or(0);
    let days_left = days_left_token(expiration_date);
    format!(
        "-{streams}-{days_left}-{}-{}.json",
        slug(server),
        slug(user)
    )
}

fn playlist_file_exists(suffix: &str, folder: &str) -> bool {
    fs::read_dir(folder)
        .ok()
        .into_iter()
        .flat_map(|it| it.flatten())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .any(|name| name.ends_with(suffix))
}

fn days_left_token(expiration_date: Option<&str>) -> i64 {
    let Some(expiration_date) = expiration_date else {
        return 9999;
    };
    if expiration_date.trim().is_empty() {
        return 9999;
    }
    let Ok(exp) = DateTime::parse_from_rfc3339(expiration_date) else {
        return 9999;
    };
    let exp = exp.with_timezone(&Local);
    let epoch = Local.timestamp_opt(0, 0).single();
    if epoch == Some(exp) {
        return 9999;
    }
    let now = Local::now();
    if exp <= now {
        return 0;
    }
    (exp - now).num_days()
}

fn is_priority_playlist(
    streams_allowed: Option<u64>,
    expiration_date: Option<&str>,
    live_categories: &[String],
) -> bool {
    let streams_ok =
        matches!(streams_allowed, Some(0)) || matches!(streams_allowed, Some(n) if n >= 2);
    streams_ok && expiration_is_priority(expiration_date) && has_priority_category(live_categories)
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
    live_categories.iter().any(|c| is_priority_category_name(c))
}

fn is_priority_category_name(category_name: &str) -> bool {
    let lc = category_name.to_lowercase();
    category_name.contains("US") || category_name.contains("Usa") || lc.contains("locals")
}

fn priority_category_ids(categories: &[LiveCategory]) -> HashSet<String> {
    categories
        .iter()
        .filter(|c| is_priority_category_name(&c.category_name))
        .map(|c| c.category_id.clone())
        .collect()
}

fn sample_streams<'a>(
    streams: &'a [LiveStream],
    sample_size: usize,
    seed: &str,
) -> Vec<&'a LiveStream> {
    let mut keyed = streams
        .iter()
        .map(|stream| {
            let mut hasher = DefaultHasher::new();
            seed.hash(&mut hasher);
            stream.stream_id.hash(&mut hasher);
            stream.name.hash(&mut hasher);
            stream.category_id.hash(&mut hasher);
            (hasher.finish(), stream)
        })
        .collect::<Vec<_>>();

    keyed.sort_by_key(|(hash, _)| *hash);
    keyed
        .into_iter()
        .take(sample_size)
        .map(|(_, stream)| stream)
        .collect()
}

fn stream_url(base: &str, user: &str, pass: &str, stream: &LiveStream) -> String {
    let ext = stream
        .container_extension
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("ts");
    format!(
        "{}/live/{}/{}/{}.{}",
        base.trim_end_matches('/'),
        urlencoding::encode(user),
        urlencoding::encode(pass),
        stream.stream_id,
        ext
    )
}

fn sniff_media(content_type: Option<&str>, body: &[u8]) -> Option<String> {
    if body.is_empty() {
        return None;
    }

    let content_type = content_type.unwrap_or("").to_lowercase();
    let start = body.get(..body.len().min(64)).unwrap_or(body);

    if content_type.contains("mpegurl") || start.starts_with(b"#EXTM3U") {
        return Some("hls".to_string());
    }

    if content_type.contains("video/mp2t")
        || (body.len() > 188 && body[0] == 0x47 && body.get(188) == Some(&0x47))
    {
        return Some("mpeg_ts".to_string());
    }

    if content_type.contains("video/mp4") || start.windows(4).any(|w| w == b"ftyp") {
        return Some("mp4".to_string());
    }

    if content_type.contains("application/octet-stream") && body.len() >= 188 {
        return Some("probable_octet_stream".to_string());
    }

    None
}

async fn notify_ntfy(client: &Client, topic_url: &str, processed: usize, priority_written: usize) {
    let body = format!(
        "iptvscraper done: processed {processed} playlists; wrote {priority_written} priority playlists"
    );
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
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entry_date_from_title_as_end_of_day() {
        let dt = parse_entry_date("IPTV { 28/MAI/2026 } ✪~✪ Table").unwrap();
        assert_eq!(
            dt.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "2026-05-28T23:59:59"
        );
    }

    #[test]
    fn parses_entry_date_from_url_slug() {
        let dt = parse_entry_date_from_url(
            "https://www.iptvregion.eu.org/2026/05/iptv-28mai2026-table-of-28-account.html",
        )
        .unwrap();
        assert_eq!(
            dt.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "2026-05-28T23:59:59"
        );
    }

    #[test]
    fn playlist_file_suffix_ignores_timestamp_prefix() {
        let suffix = playlist_file_suffix(
            "http://example.com",
            "user1",
            Some(10),
            Some("2026-06-03T23:59:59+00:00"),
        );

        assert_eq!(suffix, "-10-0-http---example-com-user1.json");
        assert!("20260603-020405-10-0-http---example-com-user1.json".ends_with(&suffix));
    }

    #[test]
    fn load_seen_inputs_matches_exact_playlist_row_identity() {
        let dir = std::env::temp_dir().join(format!(
            "iptvscraper-test-{}",
            Local::now().timestamp_nanos_opt().unwrap()
        ));
        let folder = dir.join("playlists");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("one.json"),
            serde_json::json!({
                "source_entry_url": "https://site/a.html",
                "server": "http://example.com",
                "username": "user1",
                "password": "pass1"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(folder.join("bad.json"), "not json").unwrap();

        let seen = load_seen_inputs(&[folder.to_str().unwrap()]);
        let item = PlaylistInput {
            source_entry_title: "x".to_string(),
            source_entry_url: "https://site/a.html".to_string(),
            server: "http://example.com".to_string(),
            username: "user1".to_string(),
            password: "pass1".to_string(),
        };
        let other = PlaylistInput {
            password: "pass2".to_string(),
            ..item.clone()
        };

        assert!(seen.contains(&input_key(&item)));
        assert!(!seen.contains(&input_key(&other)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn priority_playlist_allows_zero_or_at_least_two_streams() {
        let us = vec!["US Entertainment".to_string()];

        assert!(is_priority_playlist(Some(0), None, &us));
        assert!(!is_priority_playlist(Some(1), None, &us));
        assert!(is_priority_playlist(Some(2), None, &us));
    }

    #[test]
    fn priority_playlist_keeps_other_rules_intact() {
        let us = vec!["US Entertainment".to_string()];
        let usa = vec!["Usa Sports".to_string()];
        let locals = vec!["LOCALs".to_string()];
        let no_match = vec!["Canada".to_string()];

        assert!(is_priority_playlist(Some(2), Some(""), &us));
        assert!(is_priority_playlist(Some(2), None, &usa));
        assert!(is_priority_playlist(Some(2), None, &locals));
        assert!(!is_priority_playlist(Some(2), None, &no_match));
    }

    #[test]
    fn priority_category_ids_match_existing_category_rules() {
        let categories = vec![
            LiveCategory {
                category_id: "1".to_string(),
                category_name: "US Entertainment".to_string(),
            },
            LiveCategory {
                category_id: "2".to_string(),
                category_name: "Canada".to_string(),
            },
            LiveCategory {
                category_id: "3".to_string(),
                category_name: "LOCALs".to_string(),
            },
        ];

        let ids = priority_category_ids(&categories);

        assert!(ids.contains("1"));
        assert!(ids.contains("3"));
        assert!(!ids.contains("2"));
    }

    #[test]
    fn deterministic_sampler_returns_stable_max_five_streams() {
        let streams = (1..=8)
            .map(|id| LiveStream {
                name: format!("Channel {id}"),
                stream_id: id.to_string(),
                category_id: "1".to_string(),
                container_extension: Some("ts".to_string()),
            })
            .collect::<Vec<_>>();

        let first = sample_streams(&streams, 5, "playlist-a");
        let second = sample_streams(&streams, 5, "playlist-a");

        assert_eq!(first.len(), 5);
        assert_eq!(
            first.iter().map(|s| &s.stream_id).collect::<Vec<_>>(),
            second.iter().map(|s| &s.stream_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn stream_url_uses_extension_and_ts_fallback() {
        let with_ext = LiveStream {
            name: "One".to_string(),
            stream_id: "123".to_string(),
            category_id: "1".to_string(),
            container_extension: Some("m3u8".to_string()),
        };
        let no_ext = LiveStream {
            name: "Two".to_string(),
            stream_id: "456".to_string(),
            category_id: "1".to_string(),
            container_extension: None,
        };

        assert_eq!(
            stream_url("http://example.com", "user", "pass", &with_ext),
            "http://example.com/live/user/pass/123.m3u8"
        );
        assert_eq!(
            stream_url("http://example.com/", "user", "pass", &no_ext),
            "http://example.com/live/user/pass/456.ts"
        );
    }

    #[test]
    fn sniff_media_accepts_hls_ts_mp4_and_octet_stream() {
        let mut ts_packet = vec![0_u8; 189];
        ts_packet[0] = 0x47;
        ts_packet[188] = 0x47;

        assert_eq!(
            sniff_media(Some("application/vnd.apple.mpegurl"), b"#EXTM3U\n#EXTINF"),
            Some("hls".to_string())
        );
        assert_eq!(sniff_media(None, &ts_packet), Some("mpeg_ts".to_string()));
        assert_eq!(
            sniff_media(None, b"\0\0\0\x18ftypmp42"),
            Some("mp4".to_string())
        );
        assert_eq!(
            sniff_media(Some("application/octet-stream"), &[1_u8; 256]),
            Some("probable_octet_stream".to_string())
        );
    }

    #[tokio::test]
    async fn probe_stream_accepts_live_stream_that_stays_open() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request);

            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\n\r\n")
                .unwrap();
            let mut ts = vec![0_u8; 376];
            ts[0] = 0x47;
            ts[188] = 0x47;
            socket.write_all(&ts).unwrap();
            socket.flush().unwrap();
            thread::sleep(Duration::from_millis(500));
        });

        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .user_agent("iptvscraper-test")
            .build()
            .unwrap();
        let stream = LiveStream {
            name: "Live".to_string(),
            stream_id: "1".to_string(),
            category_id: "1".to_string(),
            container_extension: Some("ts".to_string()),
        };

        let result = probe_stream(
            &client,
            &format!("http://{addr}"),
            "user",
            "pass",
            &stream,
            "Test".to_string(),
        )
        .await;

        assert!(result.ok, "{result:?}");
        assert_eq!(result.reason, "mpeg_ts");
        assert!(result.bytes_read > 0, "{result:?}");
    }

    #[test]
    fn sniff_media_rejects_empty_short_ts_and_html() {
        assert_eq!(sniff_media(None, b""), None);
        assert_eq!(sniff_media(None, &[0x47, 0, 0, 0, 0]), None);
        assert_eq!(sniff_media(Some("text/html"), b"<html>nope</html>"), None);
    }
}
