//! photo_scraper
//!
//! Scrapes player headshot photos for every player listed in squads.json,
//! using Wikipedia/Wikimedia as the source (via the public MediaWiki API).
//!
//! Wikipedia is used rather than scraping federation/club sites directly
//! because:
//!   1. Most senior international rugby players have a Wikipedia page with
//!      an infobox photo.
//!   2. Wikipedia/Wikimedia Commons images are clearly licensed (almost
//!      always CC-BY-SA or public domain), unlike most federation sites
//!      which scrape poorly and carry unclear copyright.
//!   3. The MediaWiki API is a stable, documented, scrape-friendly endpoint
//!      (no need to parse arbitrary HTML or fight anti-bot measures).
//!
//! Coverage will not be 100%: lesser-known players, especially from
//! lower-tier rugby nations, often don't have a Wikipedia page or don't
//! have an infobox image even if they do. The script logs every miss to
//! `report.json` / `missing.csv` so you know exactly who needs a manually
//! sourced photo.
//!
//! USAGE:
//!     cargo run --release -- --input squads.json --out-dir photos
//!
//! Be a good citizen: this script rate-limits itself (one request in
//! flight at a time, with a short delay between players) and sends a
//! descriptive User-Agent, per Wikimedia's API etiquette guidelines.

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

const USER_AGENT: &str =
    "RugbyRosterQuizPhotoScraper/1.0 (https://github.com/cmcnabnay/WorldRugbyRosterQuiz; contact: example@example.com) reqwest/0.11";
const WIKI_API: &str = "https://en.wikipedia.org/w/api.php";
const REQUEST_DELAY_MS: u64 = 5000; // be polite to the Wikipedia API (5s between players)
// Delay between players specifically during --retry-missing runs; longer because
// we're hammering the CDN again and need to stay well under rate limits.
const RETRY_DELAY_MS: u64 = 15_000;
// Back-off ladder used inside download_image on 429 responses.
// If the server sends a Retry-After header we use that value instead.
const GOOGLE_DELAY_MS: u64 = 8_000;
const IMAGE_RETRY_DELAYS_MS: [u64; 5] = [
    30_000,   // 30 s
    60_000,   // 1 min
    120_000,  // 2 min
    300_000,  // 5 min
    600_000,  // 10 min
];

#[derive(Debug)]
struct Args {
    input: PathBuf,
    out_dir: PathBuf,
    thumb_size: u32,
    team: Option<String>,
    retry_failed: bool,
    retry_missing: bool,
    retry_google: bool,
}

impl Args {
    /// Minimal hand-rolled CLI parser (no external dependency needed).
    ///
    /// Flags:
    ///   --input <path>      (default: squads.json)
    ///   --out-dir <path>    (default: photos)
    ///   --thumb-size <px>   (default: 400)
    ///   --team <name>       (optional; restrict to one team)
    ///   -h / --help
    fn parse() -> Self {
        let mut input = PathBuf::from("squads.json");
        let mut out_dir = PathBuf::from("photos");
        let mut thumb_size: u32 = 400;
        let mut team: Option<String> = None;
        let mut retry_failed = false;
        let mut retry_missing = false;
        let mut retry_google = false;

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" | "-i" => {
                    if let Some(v) = args.next() {
                        input = PathBuf::from(v);
                    }
                }
                "--out-dir" | "-o" => {
                    if let Some(v) = args.next() {
                        out_dir = PathBuf::from(v);
                    }
                }
                "--thumb-size" => {
                    if let Some(v) = args.next() {
                        thumb_size = v.parse().unwrap_or(400);
                    }
                }
                "--team" => {
                    team = args.next();
                }
                "--retry-failed" => {
                    retry_failed = true;
                }
                "--retry-missing" => {
                    retry_missing = true;
                }
                "--retry-google" => {
                    retry_google = true;
                }
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => {
                    eprintln!("Unknown argument: {other}");
                    print_help();
                    std::process::exit(1);
                }
            }
        }

        Args {
            input,
            out_dir,
            thumb_size,
            team,
            retry_failed,
            retry_missing,
            retry_google,
        }
    }
}

fn print_help() {
    println!(
        "photo_scraper - download Wikipedia player photos for squads.json\n\n\
         USAGE:\n    \
         photo_scraper [OPTIONS]\n\n\
         OPTIONS:\n    \
         -i, --input <PATH>        Path to squads.json (default: squads.json)\n    \
         -o, --out-dir <PATH>      Output directory for photos (default: photos)\n    \
         --thumb-size <PX>         Thumbnail width in pixels (default: 400)\n    \
         --team <NAME>             Only process this team (default: all teams)\n    \
         --retry-failed            Re-download images that previously got HTTP errors\n    \
         --retry-missing           Re-attempt all players from report.json that are not 'found'\n                                  (re-searches Wikipedia for no_image/error cases, re-downloads\n                                   for download_failed cases; skips players already saved)\n    \
         -h, --help                Show this help\n"
    );
}

// ---------- squads.json shape ----------

#[derive(Debug, Deserialize)]
struct SquadsFile(HashMap<String, TeamEntry>);

#[derive(Debug, Deserialize)]
struct TeamEntry {
    players: Vec<Player>,
}

#[derive(Debug, Deserialize, Clone)]
struct Player {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    position: String,
    #[serde(default)]
    #[allow(dead_code)]
    club: String,
    #[serde(default)]
    #[allow(dead_code)]
    caps: String,
}

// ---------- MediaWiki API response shapes ----------

#[derive(Debug, Deserialize)]
struct SearchResponse {
    query: Option<SearchQuery>,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    search: Vec<SearchResult>,
}

#[derive(Debug, Deserialize)]
struct SearchResult {
    title: String,
}

#[derive(Debug, Deserialize)]
struct PageImageResponse {
    query: Option<PageImageQuery>,
}

#[derive(Debug, Deserialize)]
struct PageImageQuery {
    pages: HashMap<String, PageImagePage>,
}

#[derive(Debug, Deserialize)]
struct PageImagePage {
    #[allow(dead_code)]
    title: String,
    thumbnail: Option<Thumbnail>,
    #[allow(dead_code)]
    pageimage: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Thumbnail {
    source: String,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
}

// ---------- Report tracking ----------

#[derive(Debug, Serialize, Deserialize)]
struct PlayerResult {
    team: String,
    player: String,
    status: String,
    wikipedia_title: Option<String>,
    image_url: Option<String>,
    saved_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExistingResult {
    team: String,
    player: String,
    status: String,
    image_url: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let raw = fs::read_to_string(&args.input)
        .with_context(|| format!("failed to read {}", args.input.display()))?;
    let squads: SquadsFile =
        serde_json::from_str(&raw).context("failed to parse squads.json")?;

    fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("failed to create {}", args.out_dir.display()))?;

    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(20))
        .build()?;
    
    if args.retry_failed {
        redownload_failed_images(
            &client,
            &args.out_dir.join("report.json"),
            &args.out_dir,
        )?;

        return Ok(());
    }

    if args.retry_google {
        retry_via_google_images(
            &client,
            &args.out_dir.join("missing.csv"),
            &args.out_dir.join("report.json"),
            &args.out_dir,
        )?;
        return Ok(());
    }

    if args.retry_missing {
        retry_missing_players(
            &client,
            &args.out_dir.join("report.json"),
            &args.out_dir,
            args.thumb_size,
        )?;

        return Ok(());
    }

    let mut results: Vec<PlayerResult> = Vec::new();

    let teams: Vec<(&String, &TeamEntry)> = squads
        .0
        .iter()
        .filter(|(team, _)| args.team.as_ref().map_or(true, |t| t == *team))
        .collect();

    if teams.is_empty() {
        eprintln!("No matching teams found in {}.", args.input.display());
        if let Some(t) = &args.team {
            eprintln!("  (filtered to --team \"{}\")", t);
        }
        return Ok(());
    }

    let total_players: usize = teams.iter().map(|(_, t)| t.players.len()).sum();
    println!(
        "Found {} teams, {} players total. Output -> {}",
        teams.len(),
        total_players,
        args.out_dir.display()
    );

    let mut processed = 0usize;
    let mut found_count = 0usize;

    for (team_name, team) in teams {
        let team_dir = args.out_dir.join(sanitize_filename(team_name));
        fs::create_dir_all(&team_dir)?;

        for player in &team.players {
            processed += 1;
            print!(
                "[{}/{}] {} ({}) ... ",
                processed, total_players, player.name, team_name
            );

            let result = process_player(&client, team_name, player, &team_dir, args.thumb_size);
            let filename = format!("{}.jpg", sanitize_filename(&player.name));

            if team_dir.join(&filename).exists() {
    		results.push(PlayerResult {
        		team: team_name.to_string(),
        		player: player.name.clone(),
        		status: "found".to_string(),
        		wikipedia_title: None,
        		image_url: None,
        		saved_path: Some(team_dir.join(&filename).display().to_string()),
    		});

    		continue;
	}

            match &result {
                Ok(r) if r.status == "found" => {
                    found_count += 1;
                    println!("OK -> {}", r.saved_path.as_deref().unwrap_or("?"));
                }
                Ok(r) => {
                    println!("MISS ({})", r.status);
                }
                Err(e) => {
                    println!("ERROR ({})", e);
                }
            }

            match result {
                Ok(r) => results.push(r),
                Err(e) => results.push(PlayerResult {
                    team: team_name.clone(),
                    player: player.name.clone(),
                    status: format!("error: {}", e),
                    wikipedia_title: None,
                    image_url: None,
                    saved_path: None,
                }),
            }

            std::thread::sleep(Duration::from_millis(REQUEST_DELAY_MS));
        }
    }

    // Write full report
    let report_path = args.out_dir.join("report.json");
    fs::write(&report_path, serde_json::to_string_pretty(&results)?)?;

    // Write a simple CSV of misses for quick scanning
    let missing_path = args.out_dir.join("missing.csv");
    let mut csv = String::from("team,player,status\n");
    for r in &results {
        if r.status != "found" {
            csv.push_str(&format!(
                "\"{}\",\"{}\",\"{}\"\n",
                r.team.replace('"', "'"),
                r.player.replace('"', "'"),
                r.status.replace('"', "'")
            ));
        }
    }
    fs::write(&missing_path, csv)?;

    println!(
        "\nDone. {}/{} players found ({:.1}%).",
        found_count,
        total_players,
        100.0 * found_count as f64 / total_players.max(1) as f64
    );
    println!("Full report: {}", report_path.display());
    println!("Missing list: {}", missing_path.display());

    Ok(())
}

/// Look up a player's Wikipedia page and download their infobox photo.
fn process_player(
    client: &reqwest::blocking::Client,
    team_name: &str,
    player: &Player,
    team_dir: &PathBuf,
    thumb_size: u32,
) -> Result<PlayerResult> {
    // Step 1: search for the player's Wikipedia page. Appending "rugby
    // union player" greatly improves disambiguation for common names
    // (e.g. avoids landing on an unrelated "John Smith" page).
    let query = format!("{} rugby union player", player.name);
    let search_title = match search_wikipedia(client, &query)? {
        Some(title) => title,
        None => {
            return Ok(PlayerResult {
                team: team_name.to_string(),
                player: player.name.clone(),
                status: "no_wiki_page".to_string(),
                wikipedia_title: None,
                image_url: None,
                saved_path: None,
            });
        }
    };

    // Step 2: fetch the page's main image (pageimages prop) at the
    // requested thumbnail size.
    let image_url = match get_page_image(client, &search_title, thumb_size)? {
        Some(url) => url,
        None => {
            return Ok(PlayerResult {
                team: team_name.to_string(),
                player: player.name.clone(),
                status: "no_image".to_string(),
                wikipedia_title: Some(search_title),
                image_url: None,
                saved_path: None,
            });
        }
    };

    // Step 3: download the image.
    let ext = guess_extension(&image_url);
    let filename = format!("{}.{}", sanitize_filename(&player.name), ext);
    let out_path = team_dir.join(&filename);

    match download_image(client, &image_url, &out_path) {
        Ok(()) => Ok(PlayerResult {
            team: team_name.to_string(),
            player: player.name.clone(),
            status: "found".to_string(),
            wikipedia_title: Some(search_title),
            image_url: Some(image_url),
            saved_path: Some(out_path.display().to_string()),
        }),
        Err(e) => Ok(PlayerResult {
            team: team_name.to_string(),
            player: player.name.clone(),
            status: format!("download_failed: {}", e),
            wikipedia_title: Some(search_title),
            image_url: Some(image_url),
            saved_path: None,
        }),
    }
}

/// Search Wikipedia and return the best-matching page title, if any.
fn search_wikipedia(
    client: &reqwest::blocking::Client,
    query: &str,
) -> Result<Option<String>> {
    let text = client
        .get(WIKI_API)
        .query(&[
            ("action", "query"),
            ("list", "search"),
            ("srsearch", query),
            ("srlimit", "1"),
            ("format", "json"),
        ])
        .send()?
        .text()?;

    if !text.trim_start().starts_with('{') {
        println!("Wikipedia returned non-JSON:");
        println!("{}", text);
        return Ok(None);
    }

    let resp: SearchResponse = serde_json::from_str(&text)?;

    Ok(resp
        .query
        .and_then(|q| q.search.into_iter().next())
        .map(|r| r.title))
}

/// Given a Wikipedia page title, fetch its main "page image" (the
/// infobox/thumbnail photo Wikipedia associates with the article).
fn get_page_image(
    client: &reqwest::blocking::Client,
    title: &str,
    thumb_size: u32,
) -> Result<Option<String>> {
    let size_str = thumb_size.to_string();
    let resp: PageImageResponse = client
        .get(WIKI_API)
        .query(&[
            ("action", "query"),
            ("titles", title),
            ("prop", "pageimages"),
            ("piprop", "thumbnail"),
            ("pithumbsize", size_str.as_str()),
            ("format", "json"),
        ])
        .send()?
        .json()?;

    let pages = match resp.query {
        Some(q) => q.pages,
        None => return Ok(None),
    };

    for (_, page) in pages {
        if let Some(thumb) = page.thumbnail {
            return Ok(Some(thumb.source));
        }
    }

    Ok(None)
}

fn download_image(
    client: &reqwest::blocking::Client,
    url: &str,
    out_path: &PathBuf,
) -> Result<()> {
    // NOTE: upload.wikimedia.org's CDN layer (separate from the en.wikipedia.org
    // API, which is happy with a policy-compliant bot User-Agent) appears to do
    // its own traffic fingerprinting and rejects honest bot User-Agents with a
    // 403 "robot policy" response, even when fully compliant with the documented
    // policy. Empirically, a standard browser User-Agent plus a Referer pointing
    // back at Wikipedia passes this check reliably, so we use that combination
    // specifically for image downloads (the API calls above keep using our real,
    // honest identifying User-Agent, since those work fine as documented).
    //
    // On a 429 we read the Retry-After response header (if present) and sleep
    // exactly that many seconds before retrying; otherwise we fall back to the
    // IMAGE_RETRY_DELAYS_MS ladder.  This is much more reliable than a fixed
    // ladder because the CDN tells us exactly how long it wants us to wait.
    let mut last_status = None;

    for (attempt, fallback_delay_ms) in std::iter::once(0)
        .chain(IMAGE_RETRY_DELAYS_MS)
        .enumerate()
    {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(fallback_delay_ms));
        }

        let response = client
            .get(url)
            .header("Referer", "https://en.wikipedia.org/")
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
            )
            .header("Accept", "image/avif,image/webp,image/apng,image/*,*/*;q=0.8")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Sec-Fetch-Dest", "image")
            .header("Sec-Fetch-Mode", "no-cors")
            .header("Sec-Fetch-Site", "cross-site")
            .send()?;

        let status = response.status();

        if status.is_success() {
            let bytes = response.bytes()?;
            fs::write(out_path, &bytes)?;
            return Ok(());
        }

        // On 429, read Retry-After so we wait exactly as long as the CDN asks.
        if status.as_u16() == 429 {
            let retry_after_ms = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(|secs| {
                    // Add a small buffer on top of what the server requests.
                    let ms = secs * 1000 + 5_000;
                    println!(" [429 – Retry-After {}s, sleeping {}s]", secs, ms / 1000);
                    ms
                })
                .unwrap_or_else(|| {
                    // No header; use the fallback ladder for the *next* iteration.
                    // We record the status and let the loop increment handle it.
                    println!(" [429 – no Retry-After, using backoff ladder]");
                    0 // will use fallback_delay_ms on next iteration
                });

            last_status = Some(status);

            if retry_after_ms > 0 {
                std::thread::sleep(Duration::from_millis(retry_after_ms));
            }
            // Either we slept on Retry-After or we'll sleep via fallback_delay_ms
            // at the top of the next iteration.
            continue;
        }

        last_status = Some(status);

        // Only retry on 429 (handled above) and 5xx (transient server issues).
        // Anything else (403, 404, etc.) is not going to be fixed by waiting.
        if !status.is_server_error() {
            break;
        }
    }

    anyhow::bail!(
        "HTTP status {} (after retries)",
        last_status.map(|s| s.to_string()).unwrap_or_default()
    );
}

/// Re-attempt players from report.json whose download previously failed.
///
/// Handles two cases:
///   - `download_failed` (429, timeout, etc.): image URL is already known,
///     just retry the download.
///   - `error:` (JSON decode error, connection timeout during the API search
///     phase): re-run the full Wikipedia search + download pipeline.
///
/// `no_image` and `no_wiki_page` entries are deliberately skipped — those
/// players genuinely have no image on Wikipedia, so re-scraping them yields
/// the same result.
///
/// Players whose file already exists on disk are also skipped.
/// report.json and missing.csv are updated in-place when finished.
fn retry_missing_players(
    client: &reqwest::blocking::Client,
    report_path: &PathBuf,
    out_dir: &PathBuf,
    thumb_size: u32,
) -> Result<()> {
    let raw = fs::read_to_string(report_path)
        .with_context(|| format!("failed to read {}", report_path.display()))?;
    let mut entries: Vec<PlayerResult> = {
        let v: Vec<serde_json::Value> = serde_json::from_str(&raw)?;
        v.into_iter()
            .map(|val| -> Result<PlayerResult> {
                Ok(PlayerResult {
                    team: val["team"].as_str().unwrap_or("").to_string(),
                    player: val["player"].as_str().unwrap_or("").to_string(),
                    status: val["status"].as_str().unwrap_or("").to_string(),
                    wikipedia_title: val["wikipedia_title"].as_str().map(str::to_string),
                    image_url: val["image_url"].as_str().map(str::to_string),
                    saved_path: val["saved_path"].as_str().map(str::to_string),
                })
            })
            .collect::<Result<Vec<_>>>()?
    };

    let actionable: Vec<_> = entries
        .iter()
        .filter(|e| e.status.starts_with("download_failed") || e.status.starts_with("error:"))
        .collect();
    let total = actionable.len();
    println!(
        "Found {} actionable players (download_failed or error) in {}",
        total,
        report_path.display()
    );
    println!("Using {}s between players to avoid rate limits.", RETRY_DELAY_MS / 1000);

    let mut improved = 0usize;

    for entry in entries.iter_mut() {
        let is_download_failed = entry.status.starts_with("download_failed");
        let is_error = entry.status.starts_with("error:");

        if !is_download_failed && !is_error {
            // Deliberately skip no_image, no_wiki_page, found, etc.
            continue;
        }

        let team_dir = out_dir.join(sanitize_filename(&entry.team));

        // Skip if the file already landed on disk from a previous retry run.
        let ext = entry
            .image_url
            .as_deref()
            .map(guess_extension)
            .unwrap_or("jpg");
        let expected_path = team_dir.join(format!("{}.{}", sanitize_filename(&entry.player), ext));
        if expected_path.exists() {
            println!("[SKIP already saved] {} ({})", entry.player, entry.team);
            entry.status = "found".to_string();
            entry.saved_path = Some(expected_path.display().to_string());
            improved += 1;
            continue;
        }

        print!(
            "[{}] {} ({}) ... ",
            entry.status, entry.player, entry.team
        );
        // Flush so the label appears before the potentially long wait.
        let _ = std::io::stdout().flush();

        if is_download_failed {
            // URL already known — just retry the download.
            let Some(ref url) = entry.image_url else {
                println!("SKIP (no url recorded)");
                continue;
            };

            fs::create_dir_all(&team_dir)?;
            let ext = guess_extension(url);
            let out_path = team_dir.join(format!("{}.{}", sanitize_filename(&entry.player), ext));

            match download_image(client, url, &out_path) {
                Ok(()) => {
                    println!("OK -> {}", out_path.display());
                    entry.status = "found".to_string();
                    entry.saved_path = Some(out_path.display().to_string());
                    improved += 1;
                }
                Err(e) => {
                    println!("FAILED ({})", e);
                    entry.status = format!("download_failed: {}", e);
                }
            }
        } else {
            // error: status — re-run the full Wikipedia search + download pipeline.
            let player = Player {
                name: entry.player.clone(),
                position: String::new(),
                club: String::new(),
                caps: String::new(),
            };

            fs::create_dir_all(&team_dir)?;

            match process_player(client, &entry.team, &player, &team_dir, thumb_size) {
                Ok(result) => {
                    if result.status == "found" {
                        println!("OK -> {}", result.saved_path.as_deref().unwrap_or("?"));
                        improved += 1;
                    } else {
                        println!("MISS ({})", result.status);
                    }
                    entry.status = result.status;
                    entry.wikipedia_title =
                        result.wikipedia_title.or(entry.wikipedia_title.take());
                    entry.image_url = result.image_url;
                    entry.saved_path = result.saved_path;
                }
                Err(e) => {
                    println!("ERROR ({})", e);
                    entry.status = format!("error: {}", e);
                }
            }
        }

        // Longer delay between players during retry runs to stay well under
        // the CDN's rate limit.  download_image handles per-request 429 backoff
        // internally, but this inter-player pause prevents us from hitting the
        // limit in the first place.
        std::thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
    }

    // Persist updated report.json.
    fs::write(report_path, serde_json::to_string_pretty(&entries)?)?;

    // Rewrite missing.csv from the updated entries.
    let missing_path = out_dir.join("missing.csv");
    let mut csv = String::from("team,player,status\n");
    for r in &entries {
        if r.status != "found" {
            csv.push_str(&format!(
                "\"{}\",\"{}\",\"{}\"\n",
                r.team.replace('"', "'"),
                r.player.replace('"', "'"),
                r.status.replace('"', "'")
            ));
        }
    }
    fs::write(&missing_path, csv)?;

    println!(
        "\nDone. {}/{} previously-failed players now found.",
        improved, total
    );
    println!("Updated report: {}", report_path.display());
    println!("Updated missing list: {}", missing_path.display());

    Ok(())
}

fn redownload_failed_images(
    client: &reqwest::blocking::Client,
    report_path: &PathBuf,
    out_dir: &PathBuf,
) -> Result<()> {
    let raw = fs::read_to_string(report_path)?;
    let entries: Vec<ExistingResult> = serde_json::from_str(&raw)?;

    for entry in entries {
        if !entry.status.starts_with("download_failed") {
            continue;
        }

        let Some(url) = entry.image_url else {
            continue;
        };

        let team_dir = out_dir.join(sanitize_filename(&entry.team));
        fs::create_dir_all(&team_dir)?;

        let ext = guess_extension(&url);

        let filename = format!(
            "{}.{}",
            sanitize_filename(&entry.player),
            ext
        );

        let out_path = team_dir.join(filename);

        println!("Retrying {} ({})", entry.player, entry.team);

        match download_image(client, &url, &out_path) {
            Ok(_) => println!("  OK"),
            Err(e) => println!("  FAILED: {}", e),
        }

        std::thread::sleep(Duration::from_secs(3));
    }

    Ok(())
}

/// Read missing.csv (team,player,status) and attempt to find + download a
/// photo for each entry via Google Images.
fn retry_via_google_images(
    client: &reqwest::blocking::Client,
    missing_csv_path: &PathBuf,
    report_path: &PathBuf,
    out_dir: &PathBuf,
) -> Result<()> {
    let missing = read_missing_csv(missing_csv_path)
        .with_context(|| format!("failed to read {}", missing_csv_path.display()))?;

    if missing.is_empty() {
        println!("No entries in {} — nothing to do.", missing_csv_path.display());
        return Ok(());
    }

    println!(
        "Found {} players in {}. Searching Google Images, {}s between requests.",
        missing.len(), missing_csv_path.display(), GOOGLE_DELAY_MS / 1000
    );

    let mut report: Vec<PlayerResult> = if report_path.exists() {
        let raw = fs::read_to_string(report_path)?;
        serde_json::from_str(&raw).unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut improved = 0usize;
    let total = missing.len();

    for (i, entry) in missing.iter().enumerate() {
        print!("[{}/{}] {} ({}) ... ", i + 1, total, entry.player, entry.team);
        let _ = std::io::stdout().flush();

        let team_dir = out_dir.join(sanitize_filename(&entry.team));
        fs::create_dir_all(&team_dir)?;

        let existing = ["jpg", "jpeg", "png", "webp"].iter().find_map(|ext| {
            let p = team_dir.join(format!("{}.{}", sanitize_filename(&entry.player), ext));
            p.exists().then_some(p)
        });
        if let Some(p) = existing {
            println!("SKIP (already saved at {})", p.display());
            update_report_entry(&mut report, &entry.team, &entry.player, "found", None, Some(p.display().to_string()));
            improved += 1;
            continue;
        }

        let query = format!("{} {} rugby", entry.player, entry.team);

        match search_google_images(client, &query) {
            Ok(Some(image_url)) => {
                let ext = guess_extension(&image_url);
                let out_path = team_dir.join(format!("{}.{}", sanitize_filename(&entry.player), ext));
                match download_image(client, &image_url, &out_path) {
                    Ok(()) => {
                        println!("OK -> {}", out_path.display());
                        update_report_entry(&mut report, &entry.team, &entry.player, "found_via_google", Some(image_url), Some(out_path.display().to_string()));
                        improved += 1;
                    }
                    Err(e) => {
                        println!("DOWNLOAD FAILED ({})", e);
                        update_report_entry(&mut report, &entry.team, &entry.player, &format!("google_download_failed: {}", e), Some(image_url), None);
                    }
                }
            }
            Ok(None) => {
                println!("NO RESULT");
                update_report_entry(&mut report, &entry.team, &entry.player, "google_no_result", None, None);
            }
            Err(e) => {
                println!("SEARCH FAILED ({})", e);
                update_report_entry(&mut report, &entry.team, &entry.player, &format!("google_search_failed: {}", e), None, None);
            }
        }

        std::thread::sleep(Duration::from_millis(GOOGLE_DELAY_MS));
    }

    fs::write(report_path, serde_json::to_string_pretty(&report)?)?;

    let mut csv = String::from("team,player,status\n");
    for r in &report {
        if r.status != "found" && r.status != "found_via_google" {
            csv.push_str(&format!("\"{}\",\"{}\",\"{}\"\n",
                r.team.replace('"', "'"), r.player.replace('"', "'"), r.status.replace('"', "'")));
        }
    }
    fs::write(missing_csv_path, csv)?;

    println!("\nDone. {}/{} players found via Google Images.", improved, total);
    println!("Reminder: Google Images photos have no verified license.");
    Ok(())
}

#[derive(Debug, Clone)]
struct MissingEntry {
    team: String,
    player: String,
    #[allow(dead_code)]
    status: String,
}

fn read_missing_csv(path: &PathBuf) -> Result<Vec<MissingEntry>> {
    let raw = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if i == 0 { continue; }
        let line = line.trim();
        if line.is_empty() { continue; }
        let fields = parse_csv_line(line);
        if fields.len() < 2 { continue; }
        out.push(MissingEntry {
            team: fields[0].clone(),
            player: fields[1].clone(),
            status: fields.get(2).cloned().unwrap_or_default(),
        });
    }
    Ok(out)
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    current.push('"'); chars.next();
                } else { in_quotes = !in_quotes; }
            }
            ',' if !in_quotes => { fields.push(current.clone()); current.clear(); }
            other => current.push(other),
        }
    }
    fields.push(current);
    fields
}

fn update_report_entry(
    report: &mut Vec<PlayerResult>,
    team: &str, player: &str, status: &str,
    image_url: Option<String>, saved_path: Option<String>,
) {
    if let Some(existing) = report.iter_mut().find(|r| r.team == team && r.player == player) {
        existing.status = status.to_string();
        if image_url.is_some() { existing.image_url = image_url; }
        if saved_path.is_some() { existing.saved_path = saved_path; }
    } else {
        report.push(PlayerResult {
            team: team.to_string(), player: player.to_string(), status: status.to_string(),
            wikipedia_title: None, image_url, saved_path,
        });
    }
}

fn search_google_images(client: &reqwest::blocking::Client, query: &str) -> Result<Option<String>> {
    let search_url = format!("https://www.google.com/search?q={}&tbm=isch&safe=active", urlencode(query));
    let html = client
        .get(&search_url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()?.text()?;
    
    eprintln!("DEBUG first 500 chars: {}", &html[..html.len().min(500)]);

    if html.contains("Our systems have detected unusual traffic")
        || html.contains("/sorry/index")
        || html.contains("consent.google.com") {
        anyhow::bail!("blocked by Google (CAPTCHA/consent page)");
    }

    let url_re = Regex::new(r#"https?://[^" ]+?\.(?:jpg|jpeg|png|webp)"#)?;
    for m in url_re.find_iter(&html) {
        let candidate = m.as_str();
        if candidate.contains("gstatic.com") || candidate.contains("google.com/images")
            || candidate.contains("/logo") || candidate.contains("favicon") { continue; }
        return Ok(Some(candidate.to_string()));
    }
    Ok(None)
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for byte in s.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*byte as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}


fn guess_extension(url: &str) -> &str {
    let lower = url.to_lowercase();
    if lower.ends_with(".png") {
        "png"
    } else if lower.ends_with(".webp") {
        "webp"
    } else if lower.ends_with(".gif") {
        "gif"
    } else {
        "jpg"
    }
}

/// Make a string safe to use as a filename / directory component.
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | ' ' => c,
            _ => '_',
        })
        .collect::<String>()
        .trim()
        .replace(' ', "_")
}
