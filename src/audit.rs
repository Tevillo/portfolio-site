//! The download log, and the `portfolio-site audit` report that reads it.
//!
//! Two halves that live together because one exists only to feed the other.
//!
//! The **write** half is two append-only logs under `data/logs/`, written by
//! the server in the same style as the subscriber logs — one JSON object per
//! line, `O_APPEND`, 0600 — so the report can read them while the server is
//! running and rotation stays a `wc -l` question rather than a schema change.
//!
//! The **read** half is a terminal report. Deliberately not a web route: a page
//! showing who subscribed and what a client downloaded would need a password to
//! hold and a session to get wrong, and the question it answers is one the
//! owner asks from a shell on the box anyway.
//!
//! Nothing here identifies who pulled a file. An IP address would turn a usage
//! log into a visitor log with a retention question attached, and the counts —
//! which is the part that answers "what gets downloaded" and "is something
//! being scraped" — do not need one. A per-hour bucket catches a bulk pull
//! without keeping an address.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::notify::{self, Announced, Channel, Pending, Subscription};

/// Every deliberate save: the public per-photo route and the two work routes.
///
/// `/image`, `/thumb`, `/preview` and `/medium` are page renders, not
/// downloads, and are deliberately absent — logging them would bury the signal
/// under every tile `/all` loads and leave a log that answers nothing.
pub const DOWNLOADS_LOG: &str = "downloads.log";

/// Wrong passwords on `/work/:name/auth`, one line per attempt.
///
/// Separate from the downloads log because it is a different question — not
/// "did the client get their photos" but "is someone guessing at them" — and
/// because a per-job counter is what the throttling work will need as its
/// source. Carries a job name and a timestamp and nothing else, so it counts
/// attempts without recording who made them.
pub const AUTH_FAILURES_LOG: &str = "work-auth-failures.log";

/// Which of the three download routes served a response.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// `GET /download/*path` — a public JPEG or its sibling raw.
    Public,
    /// `POST /work/:name/download` — the bulk zip for one job.
    WorkZip,
    /// `POST /work/:name/file/*filename` — one file out of a delivery.
    WorkFile,
}

impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Route::Public => "public",
            Route::WorkZip => "work zip",
            Route::WorkFile => "work file",
        }
    }
}

/// One served download.
///
/// `path` is relative to the photos root for [`Route::Public`] and relative to
/// the job folder for the work routes, matching what each handler already has
/// in hand. `job`, `scope` and `kind` are absent on public downloads, which is
/// why they are `Option` rather than empty strings — a reader can tell "not a
/// work download" from "a work download with no scope recorded".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Download {
    pub route: Route,
    pub path: String,
    pub ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

impl Download {
    pub fn public(path: String) -> Self {
        Self { route: Route::Public, path, ts: notify::now(), job: None, scope: None, kind: None }
    }

    pub fn work_zip(job: String, scope: &str, kind: &str, path: String) -> Self {
        Self {
            route: Route::WorkZip,
            path,
            ts: notify::now(),
            job: Some(job),
            scope: Some(scope.to_string()),
            kind: Some(kind.to_string()),
        }
    }

    pub fn work_file(job: String, path: String) -> Self {
        Self {
            route: Route::WorkFile,
            path,
            ts: notify::now(),
            job: Some(job),
            scope: None,
            kind: None,
        }
    }
}

/// One wrong password against one job.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthFailure {
    pub job: String,
    pub ts: u64,
}

/// Append one download, best-effort.
///
/// A failure here is warned about and swallowed: the visitor asked for a file
/// and the file is going out, and losing a line of usage statistics is not a
/// reason to turn that into a 500.
pub async fn record_download(data_root: &Path, rec: Download) {
    let path = notify::log_path(data_root, DOWNLOADS_LOG);
    if let Err(e) = notify::append_line(&path, &rec).await {
        warn!(error = ?e, "recording download failed");
    }
}

/// Append one failed work password attempt, best-effort. Same reasoning as
/// [`record_download`]: the response is already decided.
pub async fn record_auth_failure(data_root: &Path, job: &str) {
    let path = notify::log_path(data_root, AUTH_FAILURES_LOG);
    let rec = AuthFailure { job: job.to_string(), ts: notify::now() };
    if let Err(e) = notify::append_line(&path, &rec).await {
        warn!(error = ?e, "recording work auth failure failed");
    }
}

pub async fn read_downloads(data_root: &Path) -> Vec<Download> {
    notify::read_log(&notify::log_path(data_root, DOWNLOADS_LOG)).await
}

pub async fn read_auth_failures(data_root: &Path) -> Vec<AuthFailure> {
    notify::read_log(&notify::log_path(data_root, AUTH_FAILURES_LOG)).await
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// Rows in `notified.log` this close to the newest one are treated as the same
/// run. A `notify` run writes one row per recipient as each send returns, so
/// its rows are spread over however long the sends took; anything inside half
/// an hour of the last one went out together.
const LAST_RUN_WINDOW_SECS: u64 = 30 * 60;

/// How many rows each "top N" list prints before it stops.
const TOP_N: usize = 10;

/// `portfolio-site audit`
///
/// Three sections, printed in the order the questions get asked: who is
/// subscribed, what is being downloaded, and — separately — whether each
/// client actually collected their photos.
pub async fn report(photos_root: &Path, data_root: &Path) -> anyhow::Result<()> {
    let now = notify::now();
    println!("audit — {}", fmt_datetime(now));
    println!("data:   {}", data_root.join(notify::LOGS_DIR).display());
    println!("photos: {}", photos_root.display());

    subscribers_section(data_root, now).await;
    let downloads = read_downloads(data_root).await;
    downloads_section(photos_root, &downloads, now).await;
    work_section(photos_root, data_root, &downloads, now).await;
    Ok(())
}

// --- subscribers -----------------------------------------------------------

async fn subscribers_section(data_root: &Path, now: u64) {
    heading("SUBSCRIBERS");

    let subs = notify::current_subscriptions(data_root).await;
    if subs.is_empty() {
        println!("  nobody is subscribed");
    } else {
        println!("  confirmed: {}", subs.len());
        for channel in [Channel::Email, Channel::Discord] {
            let of_channel: Vec<&Subscription> =
                subs.iter().filter(|s| s.channel == channel).collect();
            if of_channel.is_empty() {
                continue;
            }
            let all_rolls = of_channel.iter().filter(|s| s.all_rolls).count();
            let by_person = of_channel.iter().filter(|s| !s.people.is_empty()).count();
            println!(
                "    {:<8} {:>4}   any new set: {all_rolls}, named people: {by_person}",
                channel.as_str(),
                of_channel.len(),
            );
        }
        // Who is actually being followed, so a person with subscribers is
        // visibly different from a person with none.
        let mut followers: HashMap<&str, usize> = HashMap::new();
        for sub in &subs {
            for person in &sub.people {
                *followers.entry(person.as_str()).or_default() += 1;
            }
        }
        if !followers.is_empty() {
            println!("  people followed:");
            for (name, count) in top_n(followers, TOP_N) {
                println!("    {count:>4}  {name}");
            }
        }
    }

    // Pending rows are removed when redeemed, so everything still here is an
    // unconfirmed signup. A pile of expired ones is the shape a confirmation
    // mail landing in spam makes.
    let pending: Vec<Pending> =
        notify::read_log(&notify::log_path(data_root, notify::PENDING_LOG)).await;
    if pending.is_empty() {
        println!("  pending: none");
    } else {
        let expired = pending
            .iter()
            .filter(|p| now.saturating_sub(p.ts) > notify::PENDING_TTL_SECS)
            .count();
        println!(
            "  pending, never confirmed: {}  ({} still inside the {}-day window, {expired} expired)",
            pending.len(),
            pending.len() - expired,
            notify::PENDING_TTL_SECS / 86_400,
        );
        let mut oldest: Vec<&Pending> = pending.iter().collect();
        oldest.sort_by_key(|p| p.ts);
        for p in oldest.iter().take(TOP_N) {
            let stale = if now.saturating_sub(p.ts) > notify::PENDING_TTL_SECS {
                " (link expired)"
            } else {
                ""
            };
            println!(
                "    {:>8} old  {:<8} {}{stale}",
                fmt_age(now.saturating_sub(p.ts)),
                p.channel.as_str(),
                p.handle,
            );
        }
        if pending.len() > TOP_N {
            println!("    ... and {} more", pending.len() - TOP_N);
        }
    }

    // `notified.log` carries one row per recipient holding the last time they
    // were told anything, so "the last drop" is the cluster of rows sharing the
    // newest timestamp. Which folders were in it is not recoverable from here —
    // the row merges every folder that recipient has ever been sent.
    let announced: Vec<Announced> =
        notify::read_log(&notify::log_path(data_root, notify::NOTIFIED_LOG)).await;
    match announced.iter().map(|a| a.ts).max() {
        None => println!("  last drop: nothing has ever been sent"),
        Some(newest) => {
            let recipients: Vec<&Announced> = announced
                .iter()
                .filter(|a| newest.saturating_sub(a.ts) <= LAST_RUN_WINDOW_SECS)
                .collect();
            println!(
                "  last drop: {} — {} recipient(s), {} ago",
                fmt_datetime(newest),
                recipients.len(),
                fmt_age(now.saturating_sub(newest)),
            );
            for a in recipients.iter().take(TOP_N) {
                println!("    {:<8} {}", a.channel.as_str(), a.handle);
            }
            if recipients.len() > TOP_N {
                println!("    ... and {} more", recipients.len() - TOP_N);
            }
            // A confirmed subscriber with no row at all has never been sent
            // anything — either they signed up after the last drop, or a send
            // to them failed every time it was tried.
            let told: HashSet<(Channel, &str)> = announced
                .iter()
                .map(|a| (a.channel, a.handle.as_str()))
                .collect();
            let never: Vec<&Subscription> = subs
                .iter()
                .filter(|s| !told.contains(&(s.channel, s.handle.as_str())))
                .collect();
            if never.is_empty() {
                println!("  every confirmed subscriber has been sent something");
            } else {
                println!("  confirmed but never sent anything: {}", never.len());
                for s in never.iter().take(TOP_N) {
                    println!(
                        "    {:<8} {}  (subscribed {})",
                        s.channel.as_str(),
                        s.handle,
                        fmt_date(s.ts),
                    );
                }
                if never.len() > TOP_N {
                    println!("    ... and {} more", never.len() - TOP_N);
                }
            }
        }
    }
}

// --- downloads -------------------------------------------------------------

async fn downloads_section(photos_root: &Path, downloads: &[Download], now: u64) {
    heading("DOWNLOADS");

    if downloads.is_empty() {
        println!("  nothing logged yet — {DOWNLOADS_LOG} is empty or absent");
        println!("  (the log starts at the first download served by a build that writes it)");
        return;
    }

    let first = downloads.iter().map(|d| d.ts).min().unwrap_or(now);
    println!(
        "  {} download(s) since {} ({} of history)",
        downloads.len(),
        fmt_date(first),
        fmt_age(now.saturating_sub(first)),
    );
    let mut per_route: BTreeMap<&str, usize> = BTreeMap::new();
    for d in downloads {
        *per_route.entry(d.route.label()).or_default() += 1;
    }
    for (label, count) in per_route {
        println!("    {label:<10} {count:>6}");
    }

    // Public rows only, here and below. A client pulling their own delivery
    // twenty times is not a fact about the archive, and averaging it into these
    // figures would drown the public numbers in one job's activity — the work
    // section reports those per job instead.
    let pulled: HashSet<&str> = downloads
        .iter()
        .filter(|d| d.route == Route::Public)
        .map(|d| d.path.as_str())
        .collect();
    let mut per_path: HashMap<&str, usize> = HashMap::new();
    for d in downloads.iter().filter(|d| d.route == Route::Public) {
        *per_path.entry(d.path.as_str()).or_default() += 1;
    }
    if per_path.is_empty() {
        println!("  no public downloads yet");
    } else {
        println!("  most downloaded:");
        for (path, count) in top_n(per_path, TOP_N) {
            println!("    {count:>4}  {path}");
        }
    }

    // What never goes out is the other half of the question, and it can only be
    // answered against the files that exist — the log alone cannot name a photo
    // nobody has ever asked for. Files under `work/` are served by the work
    // routes and belong in that section.
    let servable: Vec<String> = crate::collect_jpegs(photos_root)
        .await
        .iter()
        .filter_map(|p| p.strip_prefix(photos_root).ok())
        .filter_map(|p| p.to_str())
        .filter(|rel| !rel.starts_with("work/"))
        .map(str::to_string)
        .collect();
    let untouched = servable
        .iter()
        .filter(|rel| !pulled.contains(rel.as_str()))
        .count();
    println!(
        "  never downloaded: {} of {} public photos",
        untouched,
        servable.len(),
    );

    // A coarse bucket rather than an address: a scraper shows up as one hour
    // holding a large share of the log, and that is all the counts need to say.
    let mut per_hour: HashMap<u64, usize> = HashMap::new();
    for d in downloads.iter().filter(|d| d.route == Route::Public) {
        *per_hour.entry(d.ts / 3600).or_default() += 1;
    }
    if let Some((hour, count)) = per_hour.iter().max_by_key(|(h, c)| (**c, **h)) {
        println!(
            "  busiest hour: {} — {count} download(s)",
            fmt_datetime(hour * 3600),
        );
    }
}

// --- work ------------------------------------------------------------------

async fn work_section(photos_root: &Path, data_root: &Path, downloads: &[Download], now: u64) {
    heading("WORK");
    println!("  (deliveries are noindex and unlisted, so a download nobody asked for is worth seeing)");

    let jobs = match crate::work::list_work(photos_root.to_path_buf()).await {
        Ok(j) => j,
        Err(e) => {
            println!("  listing work items failed: {e:#}");
            return;
        }
    };
    if jobs.is_empty() {
        println!("  no work items");
        return;
    }

    let failures = read_auth_failures(data_root).await;

    for job in &jobs {
        let name = job.name.as_str();
        println!();
        println!("  {name}  ({} jpeg, {} raw)", job.jpeg_count, job.raw_count);

        let has_password =
            match crate::work::read_password(photos_root.to_path_buf(), name.to_string()).await {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    println!("    password: unreadable ({e:#})");
                    continue;
                }
            };
        // No password means the download endpoints refuse everyone, so an
        // unset password is a job that cannot be collected, not an open one.
        println!(
            "    password: {}",
            if has_password { "set" } else { "NOT SET — downloads are locked" },
        );

        let mine: Vec<&Download> = downloads
            .iter()
            .filter(|d| d.job.as_deref() == Some(name))
            .collect();
        match mine.iter().map(|d| d.ts).max() {
            None => println!("    delivery: never downloaded"),
            Some(last) => println!(
                "    delivery: last downloaded {} ({} ago), {} download(s) total",
                fmt_datetime(last),
                fmt_age(now.saturating_sub(last)),
                mine.len(),
            ),
        }

        // "They took the JPEGs but never the RAWs" is a real answer to "did
        // they get what they needed", so the zips are reported by what came
        // out rather than as one number.
        let mut combos: BTreeMap<String, usize> = BTreeMap::new();
        let mut files = 0usize;
        for d in &mine {
            match d.route {
                Route::WorkZip => {
                    let scope = d.scope.as_deref().unwrap_or("?");
                    let kind = d.kind.as_deref().unwrap_or("?");
                    *combos.entry(format!("{scope}/{kind}")).or_default() += 1;
                }
                Route::WorkFile => files += 1,
                Route::Public => {}
            }
        }
        if combos.is_empty() {
            println!("    zips: none taken");
        } else {
            let summary: Vec<String> =
                combos.iter().map(|(c, n)| format!("{c} ×{n}")).collect();
            println!("    zips: {}", summary.join(", "));
        }
        println!("    single files: {files}");

        let wrong = failures.iter().filter(|f| f.job == name).count();
        if wrong == 0 {
            println!("    failed password attempts: 0");
        } else {
            let last = failures
                .iter()
                .filter(|f| f.job == name)
                .map(|f| f.ts)
                .max()
                .unwrap_or(0);
            println!(
                "    failed password attempts: {wrong}  (last {} ago)",
                fmt_age(now.saturating_sub(last)),
            );
        }
    }

    // Attempts against a job name that no longer exists — a deleted job, or
    // someone guessing at names as well as passwords.
    let known: HashSet<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
    let orphan = failures.iter().filter(|f| !known.contains(f.job.as_str())).count();
    if orphan > 0 {
        println!();
        println!("  {orphan} failed attempt(s) against job names that do not exist");
    }
}

// --- formatting ------------------------------------------------------------

fn heading(title: &str) {
    println!();
    println!("{title}");
    println!("{}", "-".repeat(title.len()));
}

/// The `n` largest counts, ties broken by key so two runs over an unchanged log
/// print the same thing.
fn top_n<'a>(counts: HashMap<&'a str, usize>, n: usize) -> Vec<(&'a str, usize)> {
    let mut rows: Vec<(&str, usize)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    rows.truncate(n);
    rows
}

/// Unix seconds to a UTC `YYYY-MM-DD` date.
///
/// Hand-rolled rather than pulled in: a date crate would be the heaviest
/// dependency in the tree and this is the only place in the binary that needs
/// one. Civil-from-days after Howard Hinnant's `chrono`-compatible algorithm,
/// with the era shifted so the arithmetic stays in unsigned range.
fn civil_from_unix(ts: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (ts / 86_400) as i64;
    let secs_of_day = ts % 86_400;
    // Shift the epoch to 0000-03-01 so leap days land at the end of the cycle.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (secs_of_day / 3600) as u32,
        (secs_of_day % 3600 / 60) as u32,
        (secs_of_day % 60) as u32,
    )
}

fn fmt_date(ts: u64) -> String {
    let (y, m, d, _, _, _) = civil_from_unix(ts);
    format!("{y:04}-{m:02}-{d:02}")
}

fn fmt_datetime(ts: u64) -> String {
    let (y, m, d, hh, mm, _) = civil_from_unix(ts);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

/// A rough age, one unit wide. "9d" and "3mo" are both more readable than a
/// second count and precise enough for every question this report asks.
fn fmt_age(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    match secs {
        s if s < MINUTE => format!("{s}s"),
        s if s < HOUR => format!("{}m", s / MINUTE),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < 60 * DAY => format!("{}d", s / DAY),
        s if s < 730 * DAY => format!("{}mo", s / (30 * DAY)),
        s => format!("{}y", s / (365 * DAY)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The date arithmetic is the one thing here that is easy to get subtly
    /// wrong and impossible to notice: a report that prints the wrong day for
    /// the last drop still looks like a working report.
    #[test]
    fn dates_round_trip_against_known_instants() {
        assert_eq!(fmt_date(0), "1970-01-01");
        assert_eq!(fmt_datetime(0), "1970-01-01 00:00 UTC");
        // 2000-02-29, the leap day the century rule makes a leap day.
        assert_eq!(fmt_date(951_782_400), "2000-02-29");
        // 2024-02-29T12:34:56Z
        assert_eq!(fmt_datetime(1_709_210_096), "2024-02-29 12:34 UTC");
        // 2038-01-19T03:14:07Z, past the signed 32-bit wrap.
        assert_eq!(fmt_datetime(2_147_483_647), "2038-01-19 03:14 UTC");
        assert_eq!(fmt_date(1_735_689_600), "2025-01-01");
        assert_eq!(fmt_date(1_735_603_200), "2024-12-31");
    }

    #[test]
    fn ages_pick_one_unit() {
        assert_eq!(fmt_age(30), "30s");
        assert_eq!(fmt_age(90), "1m");
        assert_eq!(fmt_age(3 * 3600 + 10), "3h");
        assert_eq!(fmt_age(9 * 86_400), "9d");
        assert_eq!(fmt_age(90 * 86_400), "3mo");
        assert_eq!(fmt_age(800 * 86_400), "2y");
    }

    /// Ties must not depend on hash order, or two runs over the same log print
    /// the "most downloaded" list in different orders.
    #[test]
    fn top_n_breaks_ties_by_name() {
        let counts = HashMap::from([("b", 2), ("a", 2), ("c", 9)]);
        assert_eq!(top_n(counts, 3), vec![("c", 9), ("a", 2), ("b", 2)]);
    }

    /// A public download must not be mistaken for a work one: the work section
    /// filters on `job`, and a `Some("")` would put every public row in it.
    #[test]
    fn public_downloads_carry_no_job() {
        let d = Download::public("2024/roll/IMG_1.jpg".into());
        assert!(d.job.is_none() && d.scope.is_none() && d.kind.is_none());
        let z = Download::work_zip("smith".into(), "edited", "jpeg", "smith-edited-jpeg.zip".into());
        assert_eq!(z.job.as_deref(), Some("smith"));
        assert_eq!(z.scope.as_deref(), Some("edited"));
    }

    /// Old lines must keep parsing after a field is added, and a public row
    /// must not grow empty `job`/`scope`/`kind` keys it never had.
    #[test]
    fn download_lines_are_forward_and_backward_compatible() {
        let old = r#"{"route":"public","path":"a/b.jpg","ts":100}"#;
        let parsed: Download = serde_json::from_str(old).expect("old line still parses");
        assert_eq!(parsed.route, Route::Public);
        assert!(parsed.job.is_none());

        let line = serde_json::to_string(&Download::public("a/b.jpg".into())).unwrap();
        assert!(!line.contains("job"), "public rows stay three fields wide: {line}");
    }
}
