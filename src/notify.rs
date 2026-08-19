//! Subscriptions, and the messages sent to them.
//!
//! People are tagged in digiKam; this module lets them ask to hear when a photo
//! carrying their tag is published, and does the telling. Two channels, because
//! the audience spans a wide age range and neither one covers all of it: email
//! and a Discord DM.
//!
//! # Why the state is a log
//!
//! Everything durable here is an **append-only JSON-lines file**. Two different
//! processes touch this state — the web server appends when someone confirms a
//! subscription, and the `notify` command appends when a message goes out — so a
//! single mutable JSON document would need a lock, and a read-modify-write race
//! between the two would silently drop a subscriber. Appending sidesteps it
//! entirely: writers only ever add a line, and readers fold the log down to
//! current state (last line per handle wins).
//!
//! That also makes "change your preferences" and "unsubscribe" the same
//! operation as "subscribe" — another line — and leaves a history of what
//! happened, which matters for a system that stores other people's contact
//! details.
//!
//! # Why nothing is sent before confirmation
//!
//! `/notify` is a public form. An email address typed into it is not evidence
//! that the person typing owns it, and a Discord user ID is worse: a mistyped
//! 18-digit snowflake is still a *valid* ID belonging to a stranger, who would
//! then receive an unsolicited DM. So a signup writes to `pending.log` and gets
//! exactly one message — the confirmation link — and only clicking it appends to
//! `subscribers.log`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::handlers::encode_path;
use crate::people;
use crate::recent;
use crate::views::abs_url;

/// Confirmation links stop working after this long, so a token leaked from an
/// old mailbox cannot be used to switch a subscription on much later.
const PENDING_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// `data/` splits in two: append-only state under `logs/`, credentials under
/// `token/`. They differ in every way that matters operationally — the logs are
/// written constantly by two processes and are the thing to back up, while the
/// credentials are written once by hand and are the thing to guard — so keeping
/// them apart means a backup, a permissions audit, or a rotation can name one
/// directory and mean exactly what it says.
pub const LOGS_DIR: &str = "logs";
pub const TOKENS_DIR: &str = "token";

pub const SUBSCRIBERS_LOG: &str = "subscribers.log";
pub const PENDING_LOG: &str = "pending.log";
pub const NOTIFIED_LOG: &str = "notified.log";

fn log_path(data_root: &Path, name: &str) -> PathBuf {
    data_root.join(LOGS_DIR).join(name)
}

fn token_path(data_root: &Path, name: &str) -> PathBuf {
    data_root.join(TOKENS_DIR).join(name)
}

/// Where a subscriber wants to hear from the site.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Email,
    Discord,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Email => "email",
            Channel::Discord => "discord",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "email" => Some(Channel::Email),
            "discord" => Some(Channel::Discord),
            _ => None,
        }
    }

    /// Shape check only — the real test of a handle is whether the
    /// confirmation message arrives.
    ///
    /// Deliberately loose for email: the address grammar is far wider than the
    /// regexes people reach for, and rejecting a valid address is worse than
    /// accepting one that bounces. Discord is the opposite, and can be checked
    /// exactly: user IDs are snowflakes, so anything non-numeric is a typo.
    pub fn handle_looks_valid(self, handle: &str) -> bool {
        match self {
            Channel::Email => {
                let bytes = handle.as_bytes();
                handle.len() >= 3
                    && handle.len() <= 254
                    && !handle.contains(char::is_whitespace)
                    && match handle.split_once('@') {
                        Some((local, domain)) => {
                            !local.is_empty()
                                && domain.contains('.')
                                && !domain.starts_with('.')
                                && !domain.ends_with('.')
                        }
                        None => false,
                    }
                    && !bytes.contains(&b'\n')
                    && !bytes.contains(&b'\r')
            }
            Channel::Discord => {
                (17..=20).contains(&handle.len()) && handle.bytes().all(|b| b.is_ascii_digit())
            }
        }
    }
}

/// A confirmed subscription: this handle wants to hear about these people.
///
/// An empty `people` is meaningful, not a bug — it is how unsubscribing is
/// recorded, since the log is append-only and a line can never be removed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subscription {
    pub channel: Channel,
    pub handle: String,
    pub people: Vec<String>,
    /// Hear about every new roll, whoever is in it. Independent of `people`:
    /// someone can follow a few friends, or the whole archive, or both.
    #[serde(default)]
    pub all_rolls: bool,
    pub ts: u64,
    /// The confirmation token this subscription came from. Kept so `confirm`
    /// can recognise a link that has already been clicked and do nothing,
    /// rather than appending a duplicate every time someone re-opens the mail.
    #[serde(default)]
    pub token: String,
}

/// A signup that has not been confirmed yet. Nothing is ever sent to a handle
/// that only appears here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
    pub token: String,
    pub channel: Channel,
    pub handle: String,
    pub people: Vec<String>,
    #[serde(default)]
    pub all_rolls: bool,
    pub ts: u64,
}

/// One message that went out, and every folder it covered.
///
/// A row per message rather than a row per folder: the message is what actually
/// happened, so a row records one send and reading the log back tells you what
/// each recipient was told and when, in one line each.
///
/// The unit of *comparison* is still the folder — `plan` flattens these lists
/// and asks whether this recipient has seen each one — so adding a fourth roll
/// to `.recent` later still announces only the fourth.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Announced {
    pub channel: Channel,
    pub handle: String,
    pub folders: Vec<String>,
    pub ts: u64,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Append-only log storage
// ---------------------------------------------------------------------------

/// Append one JSON line.
///
/// Opened `O_APPEND` and written in a single call: the kernel places the write
/// at the current end of file atomically, so a signup arriving while `notify` is
/// running cannot interleave into the middle of another line. That is the whole
/// reason this is a log and not a document.
async fn append_line<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut line = serde_json::to_string(value).context("serialising log line")?;
    line.push('\n');
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        // The log subdirectory may not exist yet on a first run, and a missing
        // parent would otherwise surface as a confusing "No such file".
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Contact details for other people; not world-readable.
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(line.as_bytes())
            .with_context(|| format!("appending to {}", path.display()))?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("log append task panicked")?
}

/// Read a whole log. A missing file is an empty log, and a line that fails to
/// parse is skipped with a warning rather than failing the read — one corrupt
/// line (a half-written record from a killed process, say) must not take the
/// entire mailing list offline.
async fn read_log<T: DeserializeOwned>(path: &Path) -> Vec<T> {
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "reading log failed");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for (i, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(line) {
            Ok(v) => out.push(v),
            Err(e) => {
                warn!(path = %path.display(), line = i + 1, error = %e, "skipping unparseable log line")
            }
        }
    }
    out
}

/// Fold `subscribers.log` down to who is subscribed *now*: the last line for a
/// given (channel, handle) wins, and a line with no people is an unsubscribe and
/// drops out entirely.
pub async fn current_subscriptions(data_root: &Path) -> Vec<Subscription> {
    let all: Vec<Subscription> = read_log(&log_path(data_root, SUBSCRIBERS_LOG)).await;
    let mut latest: HashMap<(Channel, String), Subscription> = HashMap::new();
    for sub in all {
        latest.insert((sub.channel, sub.handle.clone()), sub);
    }
    let mut out: Vec<Subscription> = latest
        .into_values()
        .filter(|s| !s.people.is_empty() || s.all_rolls)
        .collect();
    // Stable order so a dry run prints the same thing twice.
    out.sort_by(|a, b| {
        a.channel
            .as_str()
            .cmp(b.channel.as_str())
            .then_with(|| a.handle.cmp(&b.handle))
    });
    out
}

// ---------------------------------------------------------------------------
// Signup and confirmation
// ---------------------------------------------------------------------------

/// 128 bits of hex straight from the kernel.
///
/// `/dev/urandom` rather than a crate: this is the only randomness the whole
/// binary needs, and a token is the one thing here that must not be guessable —
/// anything derived from the clock would be.
pub fn new_token() -> Result<String> {
    use std::io::Read as _;
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut buf)
        .context("reading /dev/urandom")?;
    let mut out = String::with_capacity(32);
    for b in buf {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    Ok(out)
}

/// Record a signup and hand back the token its confirmation link needs.
pub async fn stage_pending(
    data_root: &Path,
    channel: Channel,
    handle: &str,
    people: &[String],
    all_rolls: bool,
) -> Result<String> {
    let token = new_token()?;
    let pending = Pending {
        token: token.clone(),
        channel,
        handle: handle.to_string(),
        people: people.to_vec(),
        all_rolls,
        ts: now(),
    };
    append_line(&log_path(data_root, PENDING_LOG), &pending).await?;
    Ok(token)
}

/// The outcome of following a confirmation link.
pub enum Confirmation {
    /// Newly confirmed — this is the subscription that was just switched on.
    Confirmed(Subscription),
    /// The link had already been used. Not an error: mail clients prefetch
    /// links and people click twice, so this has to be a no-op rather than a
    /// failure. Carries the existing subscription so the page can say the same
    /// thing it said the first time.
    AlreadyConfirmed(Subscription),
    /// No such token, or it is older than [`PENDING_TTL_SECS`].
    Unknown,
}

pub async fn confirm(data_root: &Path, token: &str) -> Result<Confirmation> {
    // Held across the whole read-decide-rewrite below, because unlike the other
    // two logs this one is rewritten rather than appended to, and two people
    // confirming at the same moment could otherwise each write back a copy of
    // the file that predates the other's — dropping a stranger's pending signup
    // as a side effect of confirming your own.
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = LOCK.get_or_init(Default::default).lock().await;

    let existing: Vec<Subscription> = read_log(&log_path(data_root, SUBSCRIBERS_LOG)).await;
    // Checked before the pending log, which is what makes a second click a
    // no-op even though the pending row is gone by then.
    if !token.is_empty() && existing.iter().any(|s| s.token == token) {
        let sub = existing
            .into_iter()
            .filter(|s| s.token == token)
            .next_back()
            .expect("just matched");
        return Ok(Confirmation::AlreadyConfirmed(sub));
    }

    let pending: Vec<Pending> = read_log(&log_path(data_root, PENDING_LOG)).await;
    // Last match wins, so re-submitting the form supersedes an older pending
    // row for the same token (which cannot collide in practice, but the rule
    // should be stated rather than assumed).
    let row = pending
        .iter()
        .filter(|p| p.token == token)
        .next_back()
        .cloned();
    let Some(row) = row else {
        return Ok(Confirmation::Unknown);
    };
    if now().saturating_sub(row.ts) > PENDING_TTL_SECS {
        return Ok(Confirmation::Unknown);
    }

    let sub = Subscription {
        channel: row.channel,
        handle: row.handle.clone(),
        people: row.people.clone(),
        all_rolls: row.all_rolls,
        ts: now(),
        token: row.token.clone(),
    };
    // Subscribe first, then forget. In this order a crash between the two
    // leaves a spent pending row, which the token check above already ignores;
    // the other order could lose the subscription entirely.
    append_line(&log_path(data_root, SUBSCRIBERS_LOG), &sub).await?;

    // A pending row exists to be redeemed once. Keeping spent ones would leave
    // every address anyone ever submitted sitting in a second file forever, so
    // this rewrites the log without the redeemed row — and drops rows that have
    // expired while we are here, since they can never be redeemed either.
    let keep: Vec<&Pending> = pending
        .iter()
        .filter(|p| p.token != token && now().saturating_sub(p.ts) <= PENDING_TTL_SECS)
        .collect();
    rewrite_log(&log_path(data_root, PENDING_LOG), &keep).await?;

    Ok(Confirmation::Confirmed(sub))
}

/// Replace a log with exactly these rows, temp-file-then-rename so a reader
/// never sees a partial file. Only `pending.log` is written this way, and only
/// under the lock in [`confirm`].
async fn rewrite_log<T: Serialize>(path: &Path, rows: &[&T]) -> Result<()> {
    let mut body = String::new();
    for row in rows {
        body.push_str(&serde_json::to_string(row).context("serialising log line")?);
        body.push('\n');
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("log.tmp");
    tokio::fs::write(&tmp, body.as_bytes())
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await;
    }
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Message composition
// ---------------------------------------------------------------------------

/// Everything one recipient is about to be told, before it becomes text.
#[derive(Clone, Debug)]
pub struct Digest {
    pub channel: Channel,
    pub handle: String,
    /// The subscriber's people who actually appear in `folders`.
    pub people: Vec<String>,
    /// Folders from the recent set, in `.recent` order, that this recipient has
    /// not been told about yet.
    pub folders: Vec<String>,
}

/// Render a digest into the owner's message.
///
/// The wording is the owner's, from `plans.md`; this only fills the slots and
/// decides where the line breaks go. Returns `(subject, body)` — the subject is
/// the first line, so the two never drift apart, and Discord (which has no
/// subject) simply ignores it.
pub fn compose(digest: &Digest) -> (String, String) {
    // Someone subscribed to every roll may be getting a message about photos
    // with none of their people in them — there is nobody to name, so the
    // people half of the owner's format drops out and the roll links carry the
    // message on their own.
    let subject = if digest.people.is_empty() {
        crate::views::NOTIFY_DIGEST_ROLLS_ONLY.to_string()
    } else {
        // Same natural join as the confirmation sentence: a plain `join(", ")`
        // gives "Photos of Guin, Eliana", which is the one place these two
        // messages used to disagree about how to say the same list.
        crate::views::NOTIFY_DIGEST_PEOPLE.replace("{}", &join_names(&digest.people))
    };
    let mut body = String::new();
    body.push_str(&subject);
    body.push('\n');
    if !digest.people.is_empty() {
        body.push_str(crate::views::NOTIFY_DIGEST_LOOK_AT);
        body.push('\n');
        for person in &digest.people {
            body.push_str("  ");
            body.push_str(&abs_url(&format!("/people/{}", encode_path(person))));
            body.push('\n');
        }
        body.push_str(crate::views::NOTIFY_DIGEST_WHOLE_ROLL);
        body.push('\n');
    }
    for folder in &digest.folders {
        body.push_str("  ");
        body.push_str(&abs_url(&format!("/browse/{}", encode_path(folder))));
        body.push('\n');
    }
    (subject, body)
}

/// The one message an unconfirmed signup receives.
///
/// Structure only: the names and the link are generated, and the sentence that
/// frames them is [`crate::views::NOTIFY_CONFIRM_INTRO`], which is empty until
/// the owner writes it. Empty is survivable — the message still lists who was
/// ticked and carries the link — so the flow works either way.
pub fn compose_confirmation(
    people: &[String],
    all_rolls: bool,
    confirm_url: &str,
) -> (String, String) {
    let subject = crate::views::NOTIFY_CONFIRM_SUBJECT.to_string();
    let mut body = String::new();
    if !crate::views::NOTIFY_CONFIRM_INTRO.is_empty() {
        body.push_str(crate::views::NOTIFY_CONFIRM_INTRO);
        body.push_str("\n\n");
    }
    // The same sentence the confirmation page shows. It used to be a bare
    // indented list of names above a bare URL, which read as machine output at
    // the one moment a stranger is deciding whether this message is legitimate.
    body.push_str(&subscription_sentence(people, all_rolls));
    body.push_str("\n\n");
    body.push_str(confirm_url);
    body.push('\n');
    (subject, body)
}

/// Say what a subscription covers, in a sentence.
///
/// Shared by the confirmation message and the confirmation page so the two can
/// never word it differently — someone reads this twice, minutes apart, and a
/// mismatch between them reads as a mistake.
///
/// Three shapes, because a subscription has three: some people, every roll, or
/// both. Naming the people rather than reporting a bare "subscribed" is what
/// lets someone catch a mistake — a name they did not mean to tick is only
/// visible if it is written out.
pub fn subscription_sentence(people: &[String], all_rolls: bool) -> String {
    if people.is_empty() {
        return crate::views::NOTIFY_CONFIRMED_ROLLS_ONLY.to_string();
    }
    let mut msg = String::from(crate::views::NOTIFY_CONFIRMED_PEOPLE);
    msg.push_str(&join_names(people));
    msg.push('.');
    if all_rolls {
        msg.push(' ');
        msg.push_str(crate::views::NOTIFY_CONFIRMED_ALSO_ROLLS);
    }
    msg
}

/// Join names the way a person would say them: "Guin", "Guin and Eliana",
/// "Guin, Eliana, and Seve".
///
/// A plain `join(", ")` is fine for three or more but wrong below that, and it
/// was what made the sentence ambiguous once a clause followed the list — the
/// reader could not tell where the names stopped.
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

// ---------------------------------------------------------------------------
// Working out who to tell
// ---------------------------------------------------------------------------

/// Build the digest for every subscriber with something new to hear.
///
/// "New" is not inferred from dates: it is the recent set (`photos/.recent`)
/// minus what `notified.log` says this recipient has already been told. Photo
/// timestamps would be the wrong signal anyway — a negative shot in 2019 and
/// scanned today is new to the website and old to EXIF.
pub async fn plan(
    photos_root: &Path,
    db_path: Option<&PathBuf>,
    data_root: &Path,
) -> Result<Vec<Digest>> {
    let folders = recent::load(photos_root).await;
    if folders.is_empty() {
        return Ok(Vec::new());
    }
    let Some(db) = db_path else {
        bail!("no digikam database; cannot tell who is in the recent photos");
    };
    let tagged = people::list_all_tagged_photos(db.clone()).await?;

    // person -> the recent folders they appear in. A photo belongs to a folder
    // when its path sits under it, so a roll's `favs/` subfolder counts toward
    // the roll rather than being missed.
    let mut by_person: HashMap<String, BTreeSet<usize>> = HashMap::new();
    for (person, rel) in tagged {
        for (i, folder) in folders.iter().enumerate() {
            if rel.starts_with(folder) && rel.as_bytes().get(folder.len()) == Some(&b'/') {
                by_person.entry(person.clone()).or_default().insert(i);
            }
        }
    }

    let announced: Vec<Announced> = read_log(&log_path(data_root, NOTIFIED_LOG)).await;
    // Flattened back out to (recipient, folder), because that is the question
    // being asked of it: has this person seen this roll?
    let already: HashSet<(Channel, String, String)> = announced
        .into_iter()
        .flat_map(|a| {
            a.folders
                .into_iter()
                .map(move |folder| (a.channel, a.handle.clone(), folder))
        })
        .collect();

    let mut out = Vec::new();
    for sub in current_subscriptions(data_root).await {
        // Folders this subscriber has not been told about yet: every folder in
        // the drop if they follow all rolls, otherwise only the ones their
        // people appear in.
        let unheard =
            |i: usize| !already.contains(&(sub.channel, sub.handle.clone(), folders[i].clone()));
        let mut folder_idx: BTreeSet<usize> = BTreeSet::new();
        if sub.all_rolls {
            folder_idx.extend((0..folders.len()).filter(|&i| unheard(i)));
        } else {
            for person in &sub.people {
                let Some(idxs) = by_person.get(person) else {
                    continue;
                };
                folder_idx.extend(idxs.iter().copied().filter(|&i| unheard(i)));
            }
        }
        if folder_idx.is_empty() {
            continue;
        }
        // Name only the people who are in the folders actually being announced,
        // so a second run about one new roll does not re-list everyone from the
        // first.
        let mut named: Vec<String> = sub
            .people
            .iter()
            .filter(|p| {
                by_person
                    .get(*p)
                    .is_some_and(|idxs| idxs.iter().any(|i| folder_idx.contains(i)))
            })
            .cloned()
            .collect();
        named.sort();
        named.dedup();
        // No `named.is_empty()` bail: an all-rolls subscriber hears about a roll
        // none of their people are in, and that message is the whole point of
        // the option.
        if named.is_empty() && !sub.all_rolls {
            continue;
        }
        out.push(Digest {
            channel: sub.channel,
            handle: sub.handle.clone(),
            people: named,
            // `.recent` order, which is the owner's chosen order.
            folders: folder_idx.into_iter().map(|i| folders[i].clone()).collect(),
        });
    }
    Ok(out)
}

/// Record that a digest went out, one line per folder it covered.
pub async fn record_sent(data_root: &Path, digest: &Digest) -> Result<()> {
    append_line(
        &log_path(data_root, NOTIFIED_LOG),
        &Announced {
            channel: digest.channel,
            handle: digest.handle.clone(),
            folders: digest.folders.clone(),
            ts: now(),
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

/// Credentials, each in its own file under `data/` at mode 600 — the same
/// one-secret-per-file shape as a work item's `.password`.
///
/// A channel with no credentials is not configured, and sending on it is an
/// error rather than a silent no-op: a missing key should stop a run, not make
/// it look like everything was delivered.
pub struct Sender {
    http: reqwest::Client,
    /// Resend API key, from `data/email.key`.
    email_key: Option<String>,
    /// Envelope sender, from `data/email.from`, e.g. `Name <photos@example.com>`.
    email_from: Option<String>,
    /// Discord bot token, from `data/discord.token`.
    discord_token: Option<String>,
    /// Overrides the email API endpoint, from `data/email.endpoint`.
    ///
    /// Exists so the send path can be pointed at a local mock and exercised
    /// without mailing anyone — otherwise the only way to test it is to send a
    /// real message. Doubles as the hook for a provider with a
    /// Resend-compatible request shape.
    email_endpoint: Option<String>,
}

async fn read_secret(path: PathBuf) -> Option<String> {
    match tokio::fs::read_to_string(&path).await {
        Ok(s) => {
            let t = s.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "reading credential failed");
            None
        }
    }
}

impl Sender {
    pub async fn load(data_root: &Path) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .context("building http client")?,
            email_key: read_secret(token_path(data_root, "email.key")).await,
            email_from: read_secret(token_path(data_root, "email.from")).await,
            discord_token: read_secret(token_path(data_root, "discord.token")).await,
            email_endpoint: read_secret(token_path(data_root, "email.endpoint")).await,
        })
    }

    /// True when this channel has credentials. Lets the form refuse a signup on
    /// a channel that could never deliver its own confirmation.
    pub fn configured(&self, channel: Channel) -> bool {
        match channel {
            Channel::Email => self.email_key.is_some() && self.email_from.is_some(),
            Channel::Discord => self.discord_token.is_some(),
        }
    }

    pub async fn send(
        &self,
        channel: Channel,
        handle: &str,
        subject: &str,
        body: &str,
    ) -> Result<()> {
        match channel {
            Channel::Email => self.send_email(handle, subject, body).await,
            Channel::Discord => self.send_discord(handle, body).await,
        }
    }

    /// Resend rather than SMTP: the hard part of self-hosted mail is
    /// deliverability (SPF/DKIM/DMARC, reverse DNS, warming an IP), not the
    /// sending, and a personal site's notifications landing in spam is the same
    /// as not sending them.
    async fn send_email(&self, to: &str, subject: &str, body: &str) -> Result<()> {
        let (Some(key), Some(from)) = (&self.email_key, &self.email_from) else {
            bail!("email is not configured (need data/email.key and data/email.from)");
        };
        let endpoint = self
            .email_endpoint
            .as_deref()
            .unwrap_or("https://api.resend.com/emails");
        let resp = self
            .http
            .post(endpoint)
            .bearer_auth(key)
            .json(&serde_json::json!({
                "from": from,
                "to": [to],
                "subject": subject,
                "text": body,
            }))
            .send()
            .await
            .context("posting to resend")?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            bail!("resend rejected the message ({status}): {detail}");
        }
        Ok(())
    }

    /// Two calls: open (or reuse) the DM channel with this user, then post into
    /// it. A 403 on either is the expected failure when the recipient shares no
    /// server with the bot or has DMs from server members turned off — the
    /// caller logs it per-recipient and carries on rather than aborting the run.
    async fn send_discord(&self, user_id: &str, body: &str) -> Result<()> {
        let Some(token) = &self.discord_token else {
            bail!("discord is not configured (need data/discord.token)");
        };
        let auth = format!("Bot {token}");
        let resp = self
            .http
            .post("https://discord.com/api/v10/users/@me/channels")
            .header(reqwest::header::AUTHORIZATION, &auth)
            .json(&serde_json::json!({ "recipient_id": user_id }))
            .send()
            .await
            .context("opening discord dm channel")?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            bail!("discord refused to open a DM channel ({status}): {detail}");
        }
        #[derive(Deserialize)]
        struct DmChannel {
            id: String,
        }
        let channel: DmChannel = resp.json().await.context("parsing discord dm channel")?;

        let resp = self
            .http
            .post(format!(
                "https://discord.com/api/v10/channels/{}/messages",
                channel.id
            ))
            .header(reqwest::header::AUTHORIZATION, &auth)
            .json(&serde_json::json!({ "content": body }))
            .send()
            .await
            .context("posting discord message")?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            bail!("discord rejected the message ({status}): {detail}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// A fixed-window counter, keyed by client address.
///
/// `/notify` is the site's only public write, and every accepted submission
/// costs an outbound message. Without a limit one script can burn the sending
/// quota and mail a few thousand strangers on the way. In-memory on purpose:
/// the limit exists to blunt a burst, and a restart clearing it is fine.
pub struct RateLimiter {
    inner: std::sync::Mutex<HashMap<String, (u64, u32)>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Count one hit against `key` and say whether it is allowed.
    pub fn allow(&self, key: &str, max: u32, window_secs: u64) -> bool {
        let now = now();
        let mut map = match self.inner.lock() {
            Ok(m) => m,
            // A poisoned lock means another thread panicked mid-update. Failing
            // open is the right call: the limiter is a nuisance filter, and
            // taking the form down over it would be the bigger outage.
            Err(e) => e.into_inner(),
        };
        // Drop windows that have expired, so a long-running server does not
        // accumulate a row per address that ever submitted.
        map.retain(|_, (start, _)| now.saturating_sub(*start) < window_secs);
        let entry = map.entry(key.to_string()).or_insert((now, 0));
        if now.saturating_sub(entry.0) >= window_secs {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= max
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_handles_are_checked_loosely_discord_exactly() {
        assert!(Channel::Email.handle_looks_valid("a@b.co"));
        assert!(Channel::Email.handle_looks_valid("first+tag@sub.example.com"));
        assert!(!Channel::Email.handle_looks_valid("no-at-sign"));
        assert!(!Channel::Email.handle_looks_valid("a@b"));
        // A newline would let a caller smuggle extra headers into a message.
        assert!(!Channel::Email.handle_looks_valid("a@b.co\nBcc: x@y.co"));

        assert!(Channel::Discord.handle_looks_valid("123456789012345678"));
        assert!(!Channel::Discord.handle_looks_valid("1234"));
        assert!(!Channel::Discord.handle_looks_valid("alice#1234"));
    }

    #[test]
    fn tokens_are_hex_and_do_not_repeat() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    /// The whole point of the confirmation step: a signup is inert until the
    /// link is followed, and following it twice must not double-subscribe —
    /// mail clients prefetch links and people click twice.
    #[tokio::test]
    async fn a_signup_is_inert_until_confirmed_and_replays_are_no_ops() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let people = vec!["Alice".to_string()];

        let token = stage_pending(root, Channel::Email, "a@b.co", &people, false)
            .await
            .unwrap();
        assert!(
            current_subscriptions(root).await.is_empty(),
            "staging alone must not subscribe anyone"
        );

        match confirm(root, &token).await.unwrap() {
            Confirmation::Confirmed(sub) => assert_eq!(sub.handle, "a@b.co"),
            _ => panic!("first click should confirm"),
        }
        assert_eq!(current_subscriptions(root).await.len(), 1);

        assert!(matches!(
            confirm(root, &token).await.unwrap(),
            Confirmation::AlreadyConfirmed(_)
        ));
        assert_eq!(
            current_subscriptions(root).await.len(),
            1,
            "a replayed link must not append a second subscription"
        );
    }

    /// One name needs no comma at all, two need "and" rather than a comma, and
    /// only three or more read as a comma list.
    ///
    /// Expectations are built from the copy constants rather than written out,
    /// because the wording is the owner's and changes without warning — these
    /// tests exist to pin how the *names* are joined, which is this module's
    /// job, not to freeze a sentence that is not.
    #[test]
    fn the_sentence_reads_at_every_length() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let expect = |joined: &str| {
            format!("{}{joined}.", crate::views::NOTIFY_CONFIRMED_PEOPLE)
        };
        assert_eq!(
            subscription_sentence(&names(&["Guin"]), false),
            expect("Guin")
        );
        assert_eq!(
            subscription_sentence(&names(&["Guin", "Eliana"]), false),
            expect("Guin and Eliana")
        );
        assert_eq!(
            subscription_sentence(&names(&["Guin", "Eliana", "Seve"]), false),
            expect("Guin, Eliana, and Seve")
        );
        assert_eq!(
            subscription_sentence(&[], true),
            crate::views::NOTIFY_CONFIRMED_ROLLS_ONLY
        );
    }

    /// The all-rolls clause has to land as its own sentence. Appended to the
    /// list instead, "…: Guin, Eliana, and you will receive…" read as a third
    /// name, so what is pinned here is the boundary: whatever the clause says,
    /// the names before it end in a full stop and never in a comma.
    #[test]
    fn the_all_rolls_clause_never_looks_like_another_name() {
        for people in [
            vec!["Guin"],
            vec!["Guin", "Eliana"],
            vec!["Guin", "Eliana", "Seve"],
        ] {
            let people: Vec<String> = people.iter().map(|s| s.to_string()).collect();
            let msg = subscription_sentence(&people, true);
            let clause = crate::views::NOTIFY_CONFIRMED_ALSO_ROLLS;
            let at = msg
                .find(clause)
                .unwrap_or_else(|| panic!("clause missing from: {msg}"));
            let before = msg[..at].trim_end();
            assert!(
                before.ends_with('.'),
                "clause runs on from the names: {msg}"
            );
            assert!(
                !before.ends_with(','),
                "clause reads as another name: {msg}"
            );
        }
    }

    /// The confirmation message is the first thing a stranger ever receives
    /// from this site, and it used to be an indented list of names above a bare
    /// URL — the exact shape a person and a spam filter both distrust. It now
    /// opens with the same sentence the confirmation page shows.
    #[test]
    fn the_confirmation_message_says_what_it_is_for() {
        let people = vec!["Guin".to_string(), "Eliana".to_string()];
        let (subject, body) = compose_confirmation(&people, false, "https://example.com/c?t=x");
        assert_eq!(subject, crate::views::NOTIFY_CONFIRM_SUBJECT);
        assert!(
            body.starts_with(&subscription_sentence(&people, false)),
            "body does not open with the shared sentence: {body}"
        );
        assert!(body.contains("https://example.com/c?t=x"));
        // The old shape: names alone on indented lines.
        assert!(!body.contains("  Guin\n"), "fell back to a bare list: {body}");
    }

    /// Pins the split. The two kinds of file have different operational
    /// lifetimes — logs are appended to constantly and are what you back up,
    /// credentials are written once and are what you guard — so a change that
    /// quietly merged them back into one directory should fail here.
    #[test]
    fn logs_and_credentials_live_in_separate_directories() {
        let root = Path::new("/srv/data");
        assert_eq!(
            log_path(root, SUBSCRIBERS_LOG),
            Path::new("/srv/data/logs/subscribers.log")
        );
        assert_eq!(
            token_path(root, "discord.token"),
            Path::new("/srv/data/token/discord.token")
        );
        assert_ne!(
            log_path(root, SUBSCRIBERS_LOG).parent(),
            token_path(root, "discord.token").parent()
        );
    }

    /// A pending row is a one-shot ticket. Once redeemed it must leave the
    /// file, or every address anyone ever typed accumulates in a second place
    /// forever — and the replay must still be recognised, which is why the
    /// subscribers log is checked before the pending one.
    #[tokio::test]
    async fn confirming_removes_the_row_from_pending() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let people = vec!["Alice".to_string()];

        let mine = stage_pending(root, Channel::Email, "a@b.co", &people, false)
            .await
            .unwrap();
        // Somebody else's signup, still waiting.
        let theirs = stage_pending(root, Channel::Discord, "123456789012345678", &people, false)
            .await
            .unwrap();
        assert_eq!(
            read_log::<Pending>(&log_path(root, PENDING_LOG))
                .await
                .len(),
            2
        );

        confirm(root, &mine).await.unwrap();
        let left: Vec<Pending> = read_log(&log_path(root, PENDING_LOG)).await;
        assert_eq!(left.len(), 1, "the redeemed row should be gone");
        assert_eq!(left[0].token, theirs, "and only the redeemed one");

        // The spent link still resolves, from the subscribers log.
        assert!(matches!(
            confirm(root, &mine).await.unwrap(),
            Confirmation::AlreadyConfirmed(_)
        ));
    }

    /// Following every roll is its own subscription: it survives the fold with
    /// no people named, where an empty people list would otherwise read as an
    /// unsubscribe.
    #[tokio::test]
    async fn following_every_roll_is_a_subscription_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let token = stage_pending(root, Channel::Email, "a@b.co", &[], true)
            .await
            .unwrap();
        confirm(root, &token).await.unwrap();

        let current = current_subscriptions(root).await;
        assert_eq!(current.len(), 1);
        assert!(current[0].people.is_empty());
        assert!(current[0].all_rolls);
    }

    /// One row per message, listing every folder it covered — not one row per
    /// folder. The log then reads as a history of what was sent, one line per
    /// send, instead of repeating the recipient once per roll.
    #[tokio::test]
    async fn a_send_records_one_row_listing_every_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let digest = Digest {
            channel: Channel::Email,
            handle: "a@b.co".into(),
            people: vec!["Guin".into()],
            folders: vec![
                "2026/caldwell-35".into(),
                "2026/caldwell-utopia-wide".into(),
                "2026/utopia".into(),
            ],
        };
        record_sent(root, &digest).await.unwrap();

        let rows: Vec<Announced> = read_log(&log_path(root, NOTIFIED_LOG)).await;
        assert_eq!(rows.len(), 1, "three folders should be one row, not three");
        assert_eq!(rows[0].handle, "a@b.co");
        assert_eq!(rows[0].folders, digest.folders);
    }

    /// Both messages name the same list, so they have to say it the same way.
    /// The digest used to join with a bare comma while the confirmation used
    /// "and", which meant one person could be told "Guin, Eliana" on Monday and
    /// "Guin and Eliana" on Tuesday.
    #[test]
    fn the_digest_and_the_confirmation_say_a_list_the_same_way() {
        for people in [
            vec!["Guin"],
            vec!["Guin", "Eliana"],
            vec!["Guin", "Eliana", "Seve"],
        ] {
            let people: Vec<String> = people.iter().map(|s| s.to_string()).collect();
            let (subject, _) = compose(&Digest {
                channel: Channel::Email,
                handle: "a@b.co".into(),
                people: people.clone(),
                folders: vec!["2026/utopia".into()],
            });
            let joined = join_names(&people);
            assert!(
                subject.contains(&joined),
                "digest said it differently: {subject}"
            );
            assert!(
                subscription_sentence(&people, false).contains(&joined),
                "confirmation said it differently"
            );
        }
    }

    /// The opener carries a `{}` where the names go. Rewording it is expected —
    /// it is the owner's sentence — but losing the slot is not: the message
    /// would still send, and would name nobody.
    #[test]
    fn the_digest_opener_keeps_its_name_slot() {
        assert!(
            crate::views::NOTIFY_DIGEST_PEOPLE.contains("{}"),
            "NOTIFY_DIGEST_PEOPLE lost its {{}} slot, so names cannot be filled in"
        );
        let filled = compose(&Digest {
            channel: Channel::Email,
            handle: "a@b.co".into(),
            people: vec!["Guin".into()],
            folders: vec!["2026/utopia".into()],
        })
        .0;
        assert!(filled.contains("Guin"), "names did not reach the subject");
    }

    /// With nobody to name, the owner's "Photos of [people]" opener has no
    /// slot to fill, so the roll links carry the message on their own.
    #[test]
    fn a_digest_with_nobody_named_still_reads() {
        let (subject, body) = compose(&Digest {
            channel: Channel::Email,
            handle: "a@b.co".into(),
            people: Vec::new(),
            folders: vec!["2026/utopia".into()],
        });
        assert_eq!(subject, crate::views::NOTIFY_DIGEST_ROLLS_ONLY);
        assert!(
            !body.contains(crate::views::NOTIFY_DIGEST_LOOK_AT),
            "no people means no people links"
        );
        assert!(
            !body.contains(crate::views::NOTIFY_DIGEST_WHOLE_ROLL),
            "the roll links stand alone, with nothing introducing them"
        );
        assert!(body.contains("/browse/2026/utopia"));
    }

    #[tokio::test]
    async fn unknown_and_expired_tokens_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(matches!(
            confirm(root, "nope").await.unwrap(),
            Confirmation::Unknown
        ));

        // A pending row older than the TTL, written directly so the test does
        // not have to wait a week.
        let stale = Pending {
            token: "stale".into(),
            channel: Channel::Email,
            handle: "a@b.co".into(),
            people: vec!["Alice".into()],
            all_rolls: false,
            ts: now() - PENDING_TTL_SECS - 1,
        };
        append_line(&log_path(root, PENDING_LOG), &stale)
            .await
            .unwrap();
        assert!(matches!(
            confirm(root, "stale").await.unwrap(),
            Confirmation::Unknown
        ));
        assert!(current_subscriptions(root).await.is_empty());
    }

    /// The log is append-only, so changing preferences and unsubscribing are
    /// both just another line: the last one for a handle is the truth, and an
    /// empty people list means "stop".
    #[tokio::test]
    async fn the_last_line_for_a_handle_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_path(dir.path(), SUBSCRIBERS_LOG);
        for people in [
            vec!["Alice".to_string(), "Bob".to_string()],
            vec!["Bob".to_string()],
        ] {
            append_line(
                &path,
                &Subscription {
                    channel: Channel::Email,
                    handle: "a@b.co".into(),
                    people,
                    all_rolls: false,
                    ts: now(),
                    token: String::new(),
                },
            )
            .await
            .unwrap();
        }
        let current = current_subscriptions(dir.path()).await;
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].people, vec!["Bob".to_string()]);

        append_line(
            &path,
            &Subscription {
                channel: Channel::Email,
                handle: "a@b.co".into(),
                people: Vec::new(),
                all_rolls: false,
                ts: now(),
                token: String::new(),
            },
        )
        .await
        .unwrap();
        assert!(
            current_subscriptions(dir.path()).await.is_empty(),
            "an empty people list is an unsubscribe"
        );
    }

    /// One unparseable line — a half-written record from a killed process —
    /// must not take the whole mailing list offline.
    #[tokio::test]
    async fn a_corrupt_line_does_not_lose_the_rest_of_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_path(dir.path(), SUBSCRIBERS_LOG);
        // Written directly rather than through `append_line`, so the log
        // directory has to be made by hand here.
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let good_a = r#"{"channel":"email","handle":"a@b.co","people":["Alice"],"ts":1}"#;
        let good_b = r#"{"channel":"email","handle":"c@d.co","people":["Bob"],"ts":2}"#;
        tokio::fs::write(&path, format!("{good_a}\n{{not json\n{good_b}\n"))
            .await
            .unwrap();
        assert_eq!(current_subscriptions(dir.path()).await.len(), 2);
    }

    #[test]
    fn the_rate_limiter_lets_a_burst_through_then_stops_it() {
        let rl = RateLimiter::new();
        for _ in 0..3 {
            assert!(rl.allow("1.2.3.4", 3, 3600));
        }
        assert!(!rl.allow("1.2.3.4", 3, 3600));
        // Keyed per address, so one abuser does not lock everyone out.
        assert!(rl.allow("5.6.7.8", 3, 3600));
    }

    #[test]
    fn compose_uses_the_owners_wording_and_links_every_roll() {
        let (subject, body) = compose(&Digest {
            channel: Channel::Email,
            handle: "a@b.co".into(),
            people: vec!["Alice".into(), "Bob".into()],
            folders: vec!["2026/caldwell-35".into(), "2026/utopia".into()],
        });
        let expected = crate::views::NOTIFY_DIGEST_PEOPLE.replace("{}", "Alice and Bob");
        assert_eq!(subject, expected);
        assert!(body.starts_with(&expected));
        assert!(body.contains("/people/Alice"));
        assert!(body.contains("/people/Bob"));
        assert!(body.contains(crate::views::NOTIFY_DIGEST_LOOK_AT));
        assert!(body.contains(crate::views::NOTIFY_DIGEST_WHOLE_ROLL));
        assert!(body.contains("/browse/2026/caldwell-35"));
        assert!(body.contains("/browse/2026/utopia"));
    }
}
