use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::work::WorkCounts;

pub struct DirEntry {
    pub name: String,
    pub url: String,
}

pub struct ImageEntry {
    pub name: String,
    pub thumb_url: String,
    pub image_url: String,
    /// Force-download URL for the JPEG itself (served with an attachment
    /// Content-Disposition, unlike `image_url` which displays inline).
    pub jpg_download_url: String,
    /// Download URL for a sibling raw/edit-master file (same basename, raw
    /// extension, same folder), when one exists alongside the JPEG. `None`
    /// hides the RAW choice in the per-photo download menu.
    pub raw_download_url: Option<String>,
    /// Rendered pixel dimensions of `thumb_url`, when known. Only the
    /// natural-ratio masonry grids need this: their tiles have no CSS
    /// `aspect-ratio`, so without `<img width height>` every image lays out
    /// at zero height, the whole grid collapses into the viewport, and
    /// `loading="lazy"` defers nothing. Square grids reserve their space via
    /// `aspect-ratio: 1` in CSS and leave this `None`.
    pub dims: Option<(u32, u32)>,
    /// One larger `srcset` candidate beyond `thumb_url`, as (url, intrinsic
    /// width in px). `None` emits a plain `src` and nothing else.
    ///
    /// Only the second candidate is stored because the first *is* `thumb_url`,
    /// whose width comes from `dims`. Both are required for a `srcset`: a `w`
    /// descriptor is a claim about the file's real width, and a wrong one makes
    /// the browser choose against the wrong number rather than merely
    /// inefficiently.
    pub srcset: Option<(String, u32)>,
}

pub struct Crumb {
    pub label: String,
    pub url: Option<String>,
}

pub struct FolderGroup {
    pub label: String,
    pub path: String,
    pub browse_url: String,
    pub images: Vec<ImageEntry>,
    /// How many of the *leading* images in `images` came from a `favs/`
    /// subfolder. `/recent` folds favorites into the front of the roll they
    /// belong to rather than showing them as a separate section, and this is
    /// where the rule ends and the rest of the roll begins — the grid draws a
    /// line there. Zero means no favorites were folded in and no line is drawn.
    pub favs_count: usize,
}

pub struct PersonEntry {
    pub name: String,
    pub url: String,
    pub photo_count: u32,
}

pub struct WorkIndexEntry {
    pub name: String,
    pub url: String,
    pub jpeg_count: u32,
    pub raw_count: u32,
}

pub struct WorkFeedPhoto {
    pub name: String,
    pub preview_url: String,
    pub image_url: String,
    pub download_action: String,
    /// Final preview pixel dimensions (post-orientation, post-downscale).
    /// Rendered as `<img width=… height=…>` so the browser reserves the
    /// right space pre-load and `loading="lazy"` can actually defer tiles
    /// that aren't near the viewport.
    pub preview_dims: Option<(u32, u32)>,
}

pub struct WorkFeedSection {
    /// Empty string for files at the job root; otherwise the full subfolder
    /// path (e.g. "digital", "digital/edited", "film/medium-format/positive").
    pub label: String,
    pub photos: Vec<WorkFeedPhoto>,
    /// Unique-per-page identifier used by `collapse.js` to keep the user's
    /// toggle choice in localStorage, e.g. `job:marisol-sam-wedding:digital/edited`.
    pub data_path: String,
    /// True when this section should be open on first load. Sections whose
    /// path contains an `edited` segment (and the job-root section) are
    /// promoted; everything else collapses by default until the client
    /// expands it.
    pub default_open: bool,
}

/// One node in the Obsidian vault sidebar tree.
pub enum NavNode {
    Folder {
        name: String,
        children: Vec<NavNode>,
    },
    Note {
        name: String,
        url: String,
        active: bool,
    },
}

/// Which top-nav entry should render as the current section, so the header can
/// mark it with `aria-current` and an underline. `None` for pages that hang off
/// no nav entry (e.g. a job detail reached from Work still marks Work).
#[derive(Clone, Copy, PartialEq)]
pub enum Nav {
    Home,
    Recent,
    Browse,
    All,
    People,
    Work,
    About,
    /// Pages that belong to no nav entry — the Nether vault, which is reachable
    /// by URL and from the About page but has no top-nav tab of its own.
    None,
}

// ---------------------------------------------------------------------------
// Site copy. Everything a human would want to reword lives here — the About
// page prose, the contact links, and the name in the footer. Edit these
// constants rather than hunting through markup.
// ---------------------------------------------------------------------------

pub const OWNER_NAME: &str = "Paul Borrego";
pub const OWNER_EMAIL: &str = "borregopaulj@gmail.com";

/// Absolute origin this site is served from, no trailing slash. Every
/// `<link rel="canonical">`, `og:url` and `sitemap.xml` entry is built from it.
///
/// It must match the spelling visitors actually reach, scheme and host exactly.
/// A search engine treats `http://x`, `https://x`, `https://www.x` and
/// `https://x/` as four candidate addresses for the same bytes; naming one here
/// is what tells it which is real. Change this and the canonical URLs move with
/// it — there is no second place to edit.
pub const SITE_ORIGIN: &str = "https://paulborrego.com";

/// One-liner under the name on the About page and in the home page hero. Kept
/// short because it is display copy; the longer sentence search results show
/// is [`SITE_DESCRIPTION`]. Empty renders nothing in either spot.
pub const OWNER_TAGLINE: &str = "Full stack film photographer";

/// The home page's opening paragraph, rendered under the name and tagline.
///
/// The owner's own words. This is the prose a search engine reaches for when it
/// builds the snippet: before it existed the first prose-shaped text on the page
/// was the gallery headings, so the result for this site read "Misc (7) Portrait
/// (12) Pastel (4)". Keep it in the owner's voice — a generated paragraph here
/// is both worse to read and worse at the job. While it is empty the paragraph
/// is simply not rendered, so the page is correct either way.
///
/// Renders as a single `<p>`; there is no markup and no paragraph break here.
pub const HOME_INTRO: &str = "I do all my own shooting, developing, and \
scanning. This site is my own repository of my photos, sorted by year and roll. \
Have a look through the portfolio below, or if you are a friend, look for photos \
of you and others in the People tab.";

/// Extra home page links out to the rest of the site, as (href, label) pairs.
///
/// Empty: the header already links every section, so this only earns its space
/// with labels written in your own words. An empty list hides the row entirely.
pub const HOME_LINKS: &[(&str, &str)] = &[];

/// The `<meta name="description">` and `og:description` for the home and About
/// pages — the sentence that appears under the link in search results.
///
/// The owner's own words, trimmed to survive truncation: Google cuts this around
/// 155 characters, so the first sentence has to stand on its own. Shorter than
/// [`HOME_INTRO`] for that reason, and it drops the closing invitation, which
/// asks for a click that a search result has already offered.
///
/// Must contain no `"` or `\`: it is interpolated into the JSON-LD in
/// [`person_jsonld`] unescaped, where a quote would silently invalidate the
/// whole block. Apostrophes are fine.
const SITE_DESCRIPTION: &str = "Full stack film photographer — I do all my own \
shooting, developing, and scanning. My film photos, sorted by year and roll.";

/// Subjects claimed in the JSON-LD `knowsAbout` field — the terms a search
/// engine associates with the person beyond the job title.
///
/// Photography only, and drawn from words the site itself uses. The site is a
/// photography portfolio; software and self-hosting are deliberately not
/// claimed here, and belong in whatever section eventually covers that work.
///
/// The three entries are the three stages the home page claims: shooting,
/// developing, scanning.
const KNOWS_ABOUT: &[&str] = &["Film photography", "Film developing", "Film scanning"];

/// About page body. Each entry is one paragraph, rendered in order. Empty
/// means the About page shows just the name, portrait and links — add your own
/// paragraphs here, e.g.
///
/// ```ignore
/// pub const ABOUT_PARAGRAPHS: &[&str] = &[
///     "First paragraph.",
///     "Second paragraph.",
/// ];
/// ```
pub const ABOUT_PARAGRAPHS: &[&str] = &[
    "Self hosting enjoyer and lover of film photography. 
    This website contains all of my photos that I have taken and scanned.",
    "If you want to find yourself or a friend check out the \"People\" tab, 
    and you can sign up there to get a message when new photos of you go up.
    Any professional work I have done is under the \"Work\" Tab.
    And if you just want to look around \"Recent\" is the latest rolls I have
    scanned and \"All\" is all of my folders that can scroll",
];

// Status and error messages on `/notify`.
//
// Functional UI text rather than prose — a form that reports nothing back is
// broken, so unlike [`NOTIFY_INTRO`] these cannot be empty — but they are still
// words a visitor reads, so they live here with the rest of the copy and are the
// owner's to reword.
pub const NOTIFY_SENT_MSG: &str = "Check your messages for a link to confirm.";
pub const NOTIFY_CONFIRM_SUBJECT: &str = "Confirm your photo notifications";
/// Label on the "every new roll" toggle at the top of the people list.
pub const NOTIFY_ALL_ROLLS_LABEL: &str = "Any new set of photos";

// The notification itself, in the owner's format:
//
//     Photos of Guin and Eliana have been uploaded!
//     Look at
//       https://paulborrego.com/people/Guin
//       https://paulborrego.com/people/Eliana
//     or check out the whole roll at
//       https://paulborrego.com/browse/2026/utopia
//
// The URLs are generated; these three lines are the words around them.

/// Opening line, where `{}` is replaced by the names. Move the `{}` and the
/// names move with it; a rewrite that drops it would silently send a message
/// naming nobody, so a test asserts it is still there.
pub const NOTIFY_DIGEST_PEOPLE: &str = "Photos of {} have been uploaded!";

/// Introduces the per-person links.
pub const NOTIFY_DIGEST_LOOK_AT: &str = "Look at";

/// Introduces the folder links, after the per-person ones.
pub const NOTIFY_DIGEST_WHOLE_ROLL: &str = "or check out the whole roll at";

/// Opening line of a digest sent to an all-rolls subscriber when none of their
/// people are in the new photos.
///
/// The owner's format ("Photos of [people] have been uploaded!") has nobody to
/// name in that case, so this stands in for it. Mine, not the owner's — worth
/// rewording.
pub const NOTIFY_DIGEST_ROLLS_ONLY: &str = "New photos have been uploaded!";

/// Text of the link to `/notify` from the People pages.
pub const NOTIFY_LINK_LABEL: &str = "Get notified about new photos";
// What someone reads once the confirmation link has been followed. Assembled
// from these three pieces, because which of them applies depends on what they
// ticked: the people half, the all-rolls half, or both.
//
// The wording is the owner's, from the request that added the all-rolls option.
pub const NOTIFY_CONFIRMED_PEOPLE: &str =
    "You have subscribed to receive notifications for photos of these people: ";
/// Follows the people sentence as a sentence of its own. Appending it to the
/// list instead produced "Guin, Eliana, and you will receive notifications…",
/// which reads as a third item in the list rather than a new clause.
pub const NOTIFY_CONFIRMED_ALSO_ROLLS: &str =
    "You will also receive notifications on any new sets of photos I post.";
/// Stands alone when someone subscribed to every roll and no one person, so it
/// opens the sentence instead of continuing one.
pub const NOTIFY_CONFIRMED_ROLLS_ONLY: &str =
    "You will receive notifications on any new sets of photos I post.";
pub const NOTIFY_BAD_LINK_MSG: &str = "That confirmation link is not valid, or has expired.";
pub const NOTIFY_UNAVAILABLE_MSG: &str = "Notifications are not available right now.";
pub const NOTIFY_UNDELIVERABLE_MSG: &str = "Could not send to that address.";
pub const NOTIFY_ERR_CHANNEL: &str = "Choose email or Discord.";
pub const NOTIFY_ERR_EMAIL: &str = "That does not look like an email address.";
pub const NOTIFY_ERR_DISCORD: &str = "A Discord user ID is 17 to 20 digits.";
pub const NOTIFY_ERR_NO_PEOPLE: &str = "Pick at least one person, or choose every new roll.";
pub const NOTIFY_ERR_UNKNOWN_PERSON: &str = "Unknown person.";
pub const NOTIFY_ERR_RATE: &str = "Too many attempts. Try again later.";

/// Paragraph at the top of `/notify`, explaining what subscribing does.
///
/// **Empty, and waiting for the owner's words.** The page works without it —
/// the guard below renders nothing — but a stranger arriving at a form that
/// asks for their email address deserves a sentence about who is asking and
/// what they will get. It wants to say: whose site this is, that a message
/// arrives only when a new photo carrying one of the ticked names is posted,
/// and that they can stop at any time.
pub const NOTIFY_INTRO: &str = "";

/// Note under the Discord field on `/notify`.
///
/// Written by the assistant on request, unlike the rest of this block — it is a
/// procedure rather than prose, and the form is unusable without it. Reword it
/// freely; it is the one hint standing between a non-technical visitor and an
/// 18-digit number they have never had to look for.
///
/// Names the bot, so it needs an edit if the bot is ever renamed.
pub const NOTIFY_DISCORD_HINT: &str = "Not the @name — Discord hides this one. \
Open Discord settings and search \"developer\", turn on Developer Mode, then \
right click your own name and pick \"Copy User ID\". You also need to share a \
server with Photo-Bot so it can message you.";

/// Opening line of the confirmation message, above the list of names and the
/// confirmation link.
///
/// **Empty, and waiting for the owner's words.** The message is still correct
/// while it is empty — the names and the link are generated — but this is the
/// first thing a subscriber ever receives from the site, and the one that has
/// to make an unexpected message look legitimate rather than like spam.
pub const NOTIFY_CONFIRM_INTRO: &str = "";

/// Links rendered as a list at the bottom of the About page. Add or remove
/// rows freely; an empty list simply hides the section.
pub const ABOUT_LINKS: &[(&str, &str)] = &[
    ("Email", "mailto:borregopaulj@gmail.com"),
    // Shares its label with the People pages' link, so rewording
    // `NOTIFY_LINK_LABEL` moves all three at once.
    (NOTIFY_LINK_LABEL, "/notify"),
    ("Notes", "/nether"),
];

/// `<title>` for the home and About pages: the owner's name, followed by the
/// owner's own tagline when there is one.
///
/// The tagline rather than a separate roles string, so the title says exactly
/// what the page says and stays in the owner's words — one place to edit, and
/// no second description of who he is to drift out of sync. `prefix` is the
/// page's own word ("About"); empty for the home page, which is the name.
fn owner_title(prefix: &str) -> String {
    let stem = if prefix.is_empty() {
        OWNER_NAME.to_string()
    } else {
        format!("{prefix} {OWNER_NAME}")
    };
    if OWNER_TAGLINE.is_empty() {
        stem
    } else {
        format!("{stem} \u{2014} {OWNER_TAGLINE}")
    }
}

/// `<meta name="description">` for the home and About pages.
fn site_description() -> &'static str {
    SITE_DESCRIPTION
}

/// Absolute URL for a site-root-relative path, e.g. `/about` ->
/// `https://paulborrego.com/about`. Canonical links, `og:url` and the sitemap
/// all need the full origin; relative paths are legal in a canonical tag but
/// ambiguous in a sitemap and useless in an Open Graph card.
pub fn abs_url(path: &str) -> String {
    format!("{SITE_ORIGIN}{path}")
}

/// Absolute `og:image` URL for a photo, given the `/image/...` URL of the same
/// photo — i.e. the 1600px preview rendition rather than the original.
///
/// The original is the wrong file for a share card. The home page's was 3.4 MB,
/// and a scraper that caps or times out below that renders no card at all —
/// which matters here because notification messages are where this site's links
/// actually get previewed. The preview rendition of the same photo is ~290 KB
/// and larger than any card displays.
///
/// The swap is a prefix rewrite because every `ImageEntry::image_url` is built
/// as `/image/{encoded rel}`, so the tail is already the encoded path the
/// `/preview/` route wants. Falls back to the input untouched if that ever
/// stops being true, which yields the old behaviour rather than a broken URL.
fn share_image(image_url: &str) -> String {
    match image_url.strip_prefix("/image/") {
        Some(rel) => abs_url(&format!("/preview/{rel}")),
        None => abs_url(image_url),
    }
}

/// `application/ld+json` describing the site owner, emitted on the home and
/// About pages.
///
/// Prose leaves a search engine to infer that "Paul Borrego" names a person and
/// to guess what he does. This states both in the vocabulary Google parses
/// directly, which is what lets a query for the bare name resolve to a person
/// rather than to whichever page happened to rank.
///
/// Hand-rolled rather than serialised: every interpolated value is a `const` in
/// this file containing no `"` and no `\`, so there is nothing to escape and no
/// dependency to add. Keep it that way — one stray quote makes the whole block
/// invalid JSON-LD and it is silently ignored, and a literal `</script>` inside
/// it would end the element early.
fn person_jsonld() -> String {
    let home = abs_url("/");
    let about = abs_url("/about");
    let knows = KNOWS_ABOUT
        .iter()
        .map(|k| format!("\"{k}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        concat!(
            r#"{{"@context":"https://schema.org","@type":"Person","#,
            r#""@id":"{home}#person","name":"{name}","jobTitle":"Photographer","#,
            r#""description":"{desc}","url":"{home}","mainEntityOfPage":"{about}","#,
            r#""email":"mailto:{email}","image":"{image}","knowsAbout":[{knows}]}}"#,
        ),
        home = home,
        about = about,
        name = OWNER_NAME,
        desc = SITE_DESCRIPTION,
        email = OWNER_EMAIL,
        image = abs_url("/static/icon.png"),
        knows = knows,
    )
}

/// Optional portrait for the About page: drop a JPEG at `<photos>/about.jpg`
/// and it appears beside the prose. Absent, the page renders text-only.
pub const ABOUT_PORTRAIT_REL: &str = "about.jpg";

fn mtime_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Newest mtime among the files in `static/`, so a CSS/JS-only deploy still
/// mints a fresh build id even though the binary did not relink.
fn newest_static_mtime() -> Option<u64> {
    let dir = std::env::current_dir().ok()?.join("static");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| mtime_secs(&e.path()))
        .max()
}

/// Identifier for the running build, in hex. Serves double duty: it is the
/// `?v=` cache-busting stamp on every `/static/*` URL, and the value
/// `static/version.js` compares against `GET /version` to decide whether an
/// already-open page is running an outdated build.
///
/// Executable mtime rather than process start time: `reset.sh` has no
/// `set -e`, so a failed `cargo build` still reaches `systemctl restart` and
/// relaunches the *old* binary. A start-time version would force-reload every
/// visitor for a deploy that never happened, and would do the same on every
/// crash-loop restart and host reboot. Cargo only relinks when something
/// actually changed, so exe mtime tracks real deploys.
pub fn build_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let exe = std::env::current_exe()
            .ok()
            .as_deref()
            .and_then(mtime_secs)
            .unwrap_or(0);
        let newest = exe.max(newest_static_mtime().unwrap_or(0));
        // Never fall back to a constant. Assets are served `immutable`, so a
        // stamp that can never change would freeze the CSS/JS in every browser
        // with no URL left to bust — and the reload feature could not rescue
        // it, because the asset URL would be identical. Start time at least
        // moves on the next restart.
        let id = if newest == 0 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(1)
        } else {
            newest
        };
        format!("{id:x}")
    })
}

/// A `/static/...` URL with the cache-busting stamp attached.
fn asset(path: &str) -> String {
    format!("{path}?v={}", build_id())
}

/// Everything the shared `<head>` needs, grouped into a struct because the list
/// outgrew a readable argument list once canonical URLs, robots policy and
/// structured data joined the title, description and scripts. Build one with
/// [`Head::new`] and add the optional parts with the builder methods.
struct Head<'a> {
    title: &'a str,
    description: &'a str,
    /// Root-relative path this page should be indexed under, e.g.
    /// `/people/sarah`. Rendered as an absolute `<link rel="canonical">`.
    /// Required rather than optional: a page with no canonical is exactly the
    /// state Search Console flags, so there is no sensible default to omit.
    canonical: &'a str,
    /// Extra `/static/*.js` to defer-load beyond the theme handler.
    scripts: &'a [&'a str],
    /// Starts the fetch of the page's Largest Contentful Paint image during
    /// HTML parse, rather than waiting for the parser to reach the `<img>`.
    preload_image: Option<&'a str>,
    /// Absolute URL of the image link previews should show. Defaults to the
    /// site icon; pages with a real photograph on them pass one.
    og_image: Option<String>,
    /// Keeps this page out of search results while still letting the crawler
    /// follow its links.
    noindex: bool,
    /// Serialised `application/ld+json` for the page's subject.
    jsonld: Option<String>,
}

impl<'a> Head<'a> {
    fn new(title: &'a str, description: &'a str, canonical: &'a str) -> Self {
        Self {
            title,
            description,
            canonical,
            scripts: &[],
            preload_image: None,
            og_image: None,
            noindex: false,
            jsonld: None,
        }
    }

    fn scripts(mut self, scripts: &'a [&'a str]) -> Self {
        self.scripts = scripts;
        self
    }

    fn preload(mut self, image: Option<&'a str>) -> Self {
        self.preload_image = image;
        self
    }

    fn og_image(mut self, url: String) -> Self {
        self.og_image = Some(url);
        self
    }

    fn noindex(mut self) -> Self {
        self.noindex = true;
        self
    }

    fn jsonld(mut self, json: String) -> Self {
        self.jsonld = Some(json);
        self
    }
}

/// The one `<head>` every page shares. Previously each of the seven page
/// functions carried its own copy of this block, so every meta/script change
/// had to be made seven times.
fn head_block(h: Head) -> Markup {
    let canonical = abs_url(h.canonical);
    let og_image = h.og_image.unwrap_or_else(|| abs_url("/static/icon.png"));
    html! {
        head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title { (h.title) }
            meta name="description" content=(h.description);
            // The single URL this content should be indexed under.
            //
            // Without it, one page reachable at more than one address — /about
            // and /about/, or any path with a tracking query stuck on the end —
            // is filed as a set of duplicates with no stated original, and the
            // crawler either picks a winner itself or indexes none of them. The
            // latter is what Search Console reports as "Duplicate without
            // user-selected canonical". Absolute, because a relative canonical
            // resolves against the current URL and so cannot correct the
            // host-level duplicates (www vs bare, http vs https).
            link rel="canonical" href=(canonical);
            @if h.noindex {
                meta name="robots" content="noindex, follow";
            }
            @if let Some(href) = h.preload_image {
                link rel="preload" as="image" href=(href) fetchpriority="high";
            }
            // Matches --surface in each theme so mobile browser chrome blends
            // with the site header instead of flashing white/black.
            meta name="theme-color" content="#f6f7f1" media="(prefers-color-scheme: light)";
            meta name="theme-color" content="#181a17" media="(prefers-color-scheme: dark)";
            meta property="og:title" content=(h.title);
            meta property="og:description" content=(h.description);
            meta property="og:type" content="website";
            // Open Graph resolves nothing relatively, so both of these are
            // absolute. og:url doubles as a canonical hint for the crawlers
            // that read it and not the link element.
            meta property="og:url" content=(canonical);
            meta property="og:site_name" content=(OWNER_NAME);
            meta property="og:image" content=(og_image);
            meta name="twitter:card" content="summary_large_image";
            meta name="twitter:title" content=(h.title);
            meta name="twitter:description" content=(h.description);
            meta name="twitter:image" content=(og_image);
            // Carries the `?v=` stamp like the other static assets, so the
            // `immutable` cache entry is replaced when the icon is swapped.
            link rel="icon" type="image/png" href=(asset("/static/icon.png"));
            // The build this page was rendered by. `version.js` compares it
            // against `GET /version` on tab refocus and reloads the page when
            // a newer build is live, so a tab left open across a deploy does
            // not keep showing stale markup.
            meta name="build-version" content=(build_id());
            (theme_head())
            link rel="stylesheet" href=(asset("/static/style.css"));
            script src=(asset("/static/version.js")) defer {}
            @for src in h.scripts {
                script src=(asset(src)) defer {}
            }
            // PreEscaped because JSON-LD is not HTML: escaping its quotes into
            // &quot; would make the block unparseable. Safe only because every
            // value in it comes from a const in this file — see person_jsonld.
            @if let Some(json) = h.jsonld {
                script type="application/ld+json" { (PreEscaped(json)) }
            }
        }
    }
}

fn site_header(active: Nav) -> Markup {
    html! {
        header.site {
            div.site-left {
                // The owner's name, not the word "Portfolio". This is the most
                // prominent link on every page and the one a crawler weighs
                // most heavily as the site's name; spending it on a generic
                // noun told a search engine nothing and told a visitor who had
                // arrived from a search result even less.
                a.brand href="/" aria-current=[(active == Nav::Home).then_some("page")] { (OWNER_NAME) }
                button.theme-toggle type="button" aria-label="Toggle dark mode" {
                    svg.theme-icon.theme-icon-sun viewBox="0 0 24 24" aria-hidden="true" focusable="false" {
                        circle cx="12" cy="12" r="4" fill="currentColor" {}
                        g stroke="currentColor" stroke-width="2" stroke-linecap="round" {
                            line x1="12" y1="2" x2="12" y2="5" {}
                            line x1="12" y1="19" x2="12" y2="22" {}
                            line x1="2" y1="12" x2="5" y2="12" {}
                            line x1="19" y1="12" x2="22" y2="12" {}
                            line x1="4.5" y1="4.5" x2="6.5" y2="6.5" {}
                            line x1="17.5" y1="17.5" x2="19.5" y2="19.5" {}
                            line x1="4.5" y1="19.5" x2="6.5" y2="17.5" {}
                            line x1="17.5" y1="6.5" x2="19.5" y2="4.5" {}
                        }
                    }
                    svg.theme-icon.theme-icon-moon viewBox="0 0 24 24" aria-hidden="true" focusable="false" {
                        path d="M20 14.5A8 8 0 0 1 9.5 4a7 7 0 1 0 10.5 10.5z" fill="currentColor" {}
                    }
                }
            }
            // Alphabetical by label, not by importance: with five one-word
            // tabs there is no reading order to communicate, and a rule a
            // visitor can infer beats one only the author knows. Adding a tab
            // later means slotting it in by name rather than re-deciding the
            // whole order.
            nav.topnav {
                a href="/about" aria-current=[(active == Nav::About).then_some("page")] { "About" }
                a href="/all" aria-current=[(active == Nav::All).then_some("page")] { "All" }
                a href="/people" aria-current=[(active == Nav::People).then_some("page")] { "People" }
                a href="/recent" aria-current=[(active == Nav::Recent).then_some("page")] { "Recent" }
                a href="/work" aria-current=[(active == Nav::Work).then_some("page")] { "Work" }
            }
        }
    }
}

fn site_footer() -> Markup {
    html! {
        footer.site-footer {
            span.footer-name { (OWNER_NAME) }
            nav.footer-links {
                a href="/about" { "About" }
                a href=(format!("mailto:{OWNER_EMAIL}")) { "Email" }
            }
        }
    }
}

/// Inline `<head>` snippet that sets `<html data-theme="...">` before paint
/// to avoid a flash of the wrong theme, plus the deferred toggle handler.
/// Read order: localStorage override -> OS `prefers-color-scheme` -> light.
fn theme_head() -> Markup {
    html! {
        script {
            (PreEscaped(
                "(function(){var k='portfolio-theme',t;\
                 try{t=localStorage.getItem(k);}catch(e){}\
                 if(t!=='dark'&&t!=='light'){t=matchMedia('(prefers-color-scheme: dark)').matches?'dark':'light';}\
                 document.documentElement.dataset.theme=t;})();"
            ))
        }
        script src=(asset("/static/theme.js")) defer {}
    }
}

/// The single `<h1>` naming what a listing page contains.
///
/// These pages opened straight into a breadcrumb and a grid, with no heading at
/// all: a crawler (and a screen reader tabbing through landmarks) got no
/// statement of what the page was. Rendered visually hidden because the grids
/// were laid out without a heading and adding a visible one would move
/// everything down — the text is the same one a sighted visitor infers from the
/// breadcrumb, so it is describing the page, not hiding keywords in it.
///
/// Not for the vault notes: their markdown already supplies its own headings.
fn page_heading(text: &str) -> Markup {
    html! { h1.page-heading { (text) } }
}

fn crumbs_nav(crumbs: &[Crumb]) -> Markup {
    html! {
        nav.crumbs aria-label="breadcrumb" {
            @for (i, c) in crumbs.iter().enumerate() {
                @if i > 0 { span.sep { "/" } }
                @match &c.url {
                    Some(u) => a href=(u) { (c.label) },
                    None => span.current { (c.label) },
                }
            }
        }
    }
}

/// How many leading tiles load eagerly at high priority. These are the ones
/// above the fold on essentially every viewport, so deferring them only delays
/// the Largest Contentful Paint; everything after is lazy.
///
/// This budget is for the *page*, not for each grid — a page with several
/// folder sections must only spend it on the first, since every later section
/// starts well below the fold.
const EAGER_TILES: usize = 2;

/// `masonry` adds the `work-grid` class so the grid renders as a natural-ratio
/// CSS-columns masonry (used by the work and portfolio pages) instead of the
/// default square-cropped grid. `eager` is how many leading tiles skip lazy
/// loading; pass 0 for any grid that is not the first on the page.
/// `favs_count` splits the grid: that many tiles lead, then a hairline, then
/// the rest. Zero (or a count covering every image) draws no line — there is
/// nothing to separate.
/// `sizes` for the natural-ratio masonry, and the reason it is a constant
/// rather than something derived.
///
/// The grid is CSS multi-column (`columns: 360px`), so the tile width is chosen
/// by the browser and cannot be expressed exactly here. Measured, it barely
/// moves: 252 CSS px at a 1920 viewport, 283 at 945, and 327 at 390 where the
/// single column is at its widest. So two cases cover it — the single column
/// below 700px, and a flat 260px above.
///
/// 260 rather than the 283 actually measured, deliberately. The Grid rendition
/// puts a portrait scan at ~272px wide, so declaring 283 would put every
/// portrait tile just *over* the threshold and send a 1x desktop to the 800px
/// candidate — four times the bytes to cover a 4% difference nobody can see.
/// Declaring 260 keeps 1x on the small file and still sends 2x to the large one.
const GRID_SIZES: &str = "(max-width: 700px) calc(100vw - 3rem), 260px";

fn image_grid(images: &[ImageEntry], masonry: bool, eager: usize, favs_count: usize) -> Markup {
    let divider_at = (favs_count > 0 && favs_count < images.len()).then_some(favs_count);
    html! {
        ul.grid.work-grid[masonry] {
            @for (i, img) in images.iter().enumerate() {
                @if divider_at == Some(i) {
                    // Presentational only: the tiles either side are already in
                    // document order, so announcing a rule would add noise
                    // without adding information.
                    li.fav-divider aria-hidden="true" {}
                }
                li.tile {
                    // Per-photo download URLs ride on the anchor as data-* so
                    // the lightbox can surface JPG / RAW choices to the side of
                    // the selected photo (see lightbox.js). `data-raw` is
                    // omitted when no sibling raw exists, hiding the RAW choice.
                    a href=(img.image_url)
                      data-name=(img.name)
                      data-jpg=(img.jpg_download_url)
                      data-raw=[img.raw_download_url.as_deref()] {
                        (grid_img(img, i < eager))
                    }
                }
            }
        }
    }
}

/// One tile's `<img>`. Intrinsic `width`/`height` are emitted whenever known so
/// the browser reserves the tile's real height before the bytes arrive — that
/// is what makes `loading="lazy"` able to skip off-screen photos at all, and it
/// removes the layout shift as each image lands.
fn grid_img(img: &ImageEntry, eager: bool) -> Markup {
    // A `srcset` needs the width of *every* candidate, and the first candidate's
    // width is `dims.0` — so no dims means no srcset, and the tile falls back to
    // the plain `src` it had before.
    let srcset = match (img.dims, img.srcset.as_ref()) {
        (Some((w, _)), Some((url, big_w))) => {
            Some(format!("{} {w}w, {url} {big_w}w", img.thumb_url))
        }
        _ => None,
    };
    html! {
        @match img.dims {
            Some((w, h)) => img
                src=(img.thumb_url) alt=(img.name)
                srcset=[srcset.as_deref()]
                sizes=[srcset.as_ref().map(|_| GRID_SIZES)]
                width=(w) height=(h)
                decoding="async"
                loading=(if eager { "eager" } else { "lazy" })
                fetchpriority=[eager.then_some("high")];,
            None => img
                src=(img.thumb_url) alt=(img.name)
                decoding="async"
                loading=(if eager { "eager" } else { "lazy" })
                fetchpriority=[eager.then_some("high")];,
        }
    }
}

/// Shared flat gallery page used by the per-folder browse views and per-person
/// photo lists: a folder list plus a single square-cropped photo grid. When
/// `show_favs` is set, a "Favorites only" toggle is shown that filters the grid
/// down to photos living inside a `favs` folder (used by the per-person view).
/// `canonical` is the request path this listing lives at — `/browse/2024/rolls`
/// or `/people/sarah`. It cannot be derived from `title` (two folders in
/// different years share a name) so the handler passes it in.
pub fn page(
    title: &str,
    canonical: &str,
    crumbs: &[Crumb],
    subdirs: &[DirEntry],
    images: &[ImageEntry],
    show_favs: bool,
    active: Nav,
) -> Markup {
    let scripts: &[&str] = if show_favs {
        &["/static/lightbox.js", "/static/favs.js"]
    } else {
        &["/static/lightbox.js"]
    };
    let page_title = format!("{title} — Photographs by {OWNER_NAME}");
    let description = format!("Film photographs in {title}, shot and scanned by {OWNER_NAME}.");
    // A listing's first tile is its largest image; showing it in a link preview
    // beats showing the site icon.
    let og = images.first().map(|img| share_image(&img.image_url));
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block({
                let h = Head::new(&page_title, &description, canonical).scripts(scripts);
                match og {
                    Some(url) => h.og_image(url),
                    None => h,
                }
            }))
            body {
                (site_header(active))
                main {
                    (crumbs_nav(crumbs))
                    (page_heading(&match active {
                        Nav::People => format!("Photographs of {title}"),
                        _ => format!("Photographs in {title}"),
                    }))
                    // On a person's own page the link pre-ticks them, so the
                    // form arrives half filled in.
                    @if active == Nav::People {
                        p.notify-link {
                            a href=(format!("/notify?person={}", crate::handlers::encode_path(title))) { (NOTIFY_LINK_LABEL) }
                        }
                    }
                    @if show_favs && !images.is_empty() {
                        section.all-controls {
                            button.favs-toggle type="button" aria-pressed="false" {
                                span.favs-toggle-track { span.favs-toggle-thumb {} }
                                span.favs-toggle-label { "Favorites only" }
                            }
                        }
                    }
                    @if !subdirs.is_empty() {
                        section.dirs {
                            h2 { "Folders" }
                            ul.dirlist {
                                @for d in subdirs {
                                    li { a href=(d.url) { (d.name) "/" } }
                                }
                            }
                        }
                    }
                    @if !images.is_empty() {
                        section.gallery {
                            @if !subdirs.is_empty() { h2 { "Photos" } }
                            (image_grid(images, false, EAGER_TILES, 0))
                        }
                    }
                    @if subdirs.is_empty() && images.is_empty() {
                        p.empty { "Nothing here yet." }
                    }
                }
                (site_footer())
            }
        }
    }
}

pub fn people_index_page(title: &str, crumbs: &[Crumb], people: &[PersonEntry]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(Head::new(
                &format!("{title} — Photographs by {OWNER_NAME}"),
                "Every photograph on this site, indexed by the people in it.",
                "/people",
            )))
            body {
                (site_header(Nav::People))
                main {
                    (crumbs_nav(crumbs))
                    (page_heading("People in Paul Borrego's photographs"))
                    @if people.is_empty() {
                        p.empty { "No people tagged yet." }
                    } @else {
                        section.dirs {
                            ul.dirlist {
                                @for p in people {
                                    li {
                                        a href=(p.url) {
                                            (p.name)
                                            span.count { " (" (p.photo_count) ")" }
                                        }
                                    }
                                }
                            }
                        }
                        // The form is reachable from nowhere else — it is
                        // deliberately absent from the top nav, since it is a
                        // thing a handful of people do once, not a section of
                        // the site.
                        p.notify-link { a href="/notify" { (NOTIFY_LINK_LABEL) } }
                    }
                }
                (site_footer())
            }
        }
    }
}

fn nav_tree(nodes: &[NavNode]) -> Markup {
    html! {
        ul.nav-tree {
            @for node in nodes {
                @match node {
                    NavNode::Folder { name, children } => {
                        li.nav-folder {
                            span.folder-name { (name) }
                            (nav_tree(children))
                        }
                    }
                    NavNode::Note { name, url, active } => {
                        li.nav-note {
                            a href=(url) class=(if *active { "active" } else { "" }) { (name) }
                        }
                    }
                }
            }
        }
    }
}

/// Which vault view is active, so the sidebar toggle can mark itself.
#[derive(Clone, Copy, PartialEq)]
pub enum NetherView {
    Notes,
    Graph,
}

fn nether_sidebar(nav: &[NavNode], view: NetherView) -> Markup {
    html! {
        aside.nether-sidebar {
            a.nether-home href="/nether" { "Nether" }
            nav.nether-views {
                a href="/nether" class=(if view == NetherView::Notes { "active" } else { "" }) { "Notes" }
                a href="/nether/graph" class=(if view == NetherView::Graph { "active" } else { "" }) { "Graph" }
            }
            (nav_tree(nav))
        }
    }
}

/// Render a single vault note: portfolio chrome, a folder-tree sidebar, and the
/// already-rendered note HTML as the main column. `content` is trusted markup.
pub fn nether_page(
    title: &str,
    canonical: &str,
    crumbs: &[Crumb],
    nav: &[NavNode],
    content: Markup,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(Head::new(
                &format!("{title} — Notes by {OWNER_NAME}"),
                &format!("{title} — a note from {OWNER_NAME}'s vault on software and self-hosting."),
                canonical,
            )))
            body {
                (site_header(Nav::None))
                main.nether {
                    div.nether-layout {
                        (nether_sidebar(nav, NetherView::Notes))
                        article.nether-content {
                            (crumbs_nav(crumbs))
                            // Most notes open with a `# Heading`, which becomes
                            // this page's h1; plenty do not, and those went out
                            // with no heading at all. Supply the note's name only
                            // in that case — emitting it unconditionally would
                            // give the ones that do have a heading two competing
                            // h1s.
                            @if !content.0.contains("<h1") {
                                (page_heading(title))
                            }
                            div.note-body { (content) }
                        }
                    }
                }
            }
        }
    }
}

/// Render the Obsidian-style graph view. `graph_json` is a trusted JSON string
/// describing nodes and edges, consumed by `graph.js` to lay out the canvas.
pub fn nether_graph_page(crumbs: &[Crumb], nav: &[NavNode], graph_json: &str) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(Head::new(
                &format!("Note Graph — {OWNER_NAME}"),
                "Link graph of the note vault.",
                "/nether/graph",
            )))
            body {
                (site_header(Nav::None))
                main.nether {
                    (page_heading("Note graph"))
                    div.nether-layout {
                        (nether_sidebar(nav, NetherView::Graph))
                        article.nether-content.graph-content {
                            div.graph-stage {
                                (crumbs_nav(crumbs))
                                canvas #graph-canvas {}
                                div.graph-empty hidden { "This vault has no notes to graph yet." }
                            }
                        }
                    }
                }
                script #graph-data type="application/json" { (PreEscaped(graph_json)) }
                script src=(asset("/static/graph.js")) defer {}
            }
        }
    }
}

/// Portfolio home page: one collapsible section per `portfolio/*` tag, photos at
/// their natural aspect ratio in the work page's masonry chrome. `/all` renders
/// the same [`FolderGroup`] shape through [`all_photos_page`] instead, which adds
/// the folder tree.
/// Collapsible, labelled folder sections — the shared body of the home page and
/// `/recent`. Both render the same thing (a list of folders, each with its own
/// grid); only the header above them differs, so the markup lives here once and
/// the two page functions supply their own surroundings.
fn gallery_sections(groups: &[FolderGroup]) -> Markup {
    html! {
        @if groups.is_empty() {
            p.empty { "Nothing here yet." }
        } @else {
            @for (gi, g) in groups.iter().enumerate() {
                section.gallery.work-gallery data-path=(g.path) {
                    h2 {
                        button.collapse-toggle type="button" aria-label="Collapse folder" aria-expanded="true" {
                            (chevron())
                        }
                        // A tag-driven section's photos are spread across the
                        // tree, so it has no folder to link to and renders as
                        // plain text — the shared `.section-label` styling is
                        // element-agnostic, so the heading looks the same either
                        // way.
                        @if g.browse_url.is_empty() {
                            span.section-label { (g.label) }
                        } @else {
                            a.section-label href=(g.browse_url) { (g.label) }
                        }
                        // No count here, unlike /all's heading. How many files
                        // sit in a folder is archive information: useful when
                        // you are navigating the archive, and on the home page
                        // it is the first thing a visitor reads, ahead of any
                        // photograph. /all and the work delivery pages keep
                        // theirs — there a file count is the point.
                    }
                    // Only the first section is above the fold, so it is the
                    // only one that gets the eager budget.
                    (image_grid(&g.images, true, if gi == 0 { EAGER_TILES } else { 0 }, g.favs_count))
                }
            }
        }
    }
}

pub fn grouped_gallery_page(groups: &[FolderGroup]) -> Markup {
    // The home page's first tile is the LCP element on every viewport. Telling
    // the browser about it in <head> starts the fetch during HTML parse instead
    // of waiting for the image to be discovered in the body.
    let lcp = groups
        .first()
        .and_then(|g| g.images.first())
        .map(|img| img.thumb_url.as_str());
    // The preview rendition, not the tile and not the original: a 400px tile
    // renders as a blurry card, and the original is megabytes of file a scraper
    // may refuse. See [`share_image`].
    let og = groups
        .first()
        .and_then(|g| g.images.first())
        .map(|img| share_image(&img.image_url));
    // Bound outside the `html!` block: `Head` borrows its title, so a `format!`
    // temporary built inline would be dropped at the end of the builder chain
    // while the borrow is still live.
    let page_title = owner_title("");
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block({
                let h = Head::new(
                    &page_title,
                    site_description(),
                    "/",
                )
                .scripts(&["/static/lightbox.js", "/static/collapse.js"])
                .preload(lcp)
                .jsonld(person_jsonld());
                match og {
                    Some(url) => h.og_image(url),
                    None => h,
                }
            }))
            body {
                (site_header(Nav::Home))
                main.portfolio {
                    // The home page's own breadcrumb only ever read "Portfolio",
                    // duplicating the brand; a name and a line of context earn
                    // that space better.
                    section.hero {
                        h1.hero-name { (OWNER_NAME) }
                        @if !OWNER_TAGLINE.is_empty() {
                            p.hero-tagline { (OWNER_TAGLINE) }
                        }
                        @if !HOME_INTRO.is_empty() {
                            p.hero-intro { (HOME_INTRO) }
                        }
                        @if !HOME_LINKS.is_empty() {
                            nav.hero-links aria-label="Sections of this site" {
                                @for (href, label) in HOME_LINKS {
                                    a href=(href) { (label) }
                                }
                            }
                        }
                    }
                    (gallery_sections(groups))
                }
                (site_footer())
            }
        }
    }
}

/// `/recent` — the folders named in `photos/.recent`, newest drop first.
///
/// Same body as the home page (see [`gallery_sections`]) without the hero: this
/// page is a listing, so it opens on a breadcrumb like every other listing
/// rather than reintroducing the site. The section order is the order of the
/// lines in `.recent`, which is deliberate — that file is the owner's statement
/// of what the current drop is, ordering included.
pub fn recent_page(groups: &[FolderGroup]) -> Markup {
    let lcp = groups
        .first()
        .and_then(|g| g.images.first())
        .map(|img| img.thumb_url.as_str());
    let og = groups
        .first()
        .and_then(|g| g.images.first())
        .map(|img| share_image(&img.image_url));
    // Bound outside `html!` for the same reason as in `grouped_gallery_page`:
    // `Head` borrows its title, so an inline `format!` temporary would be
    // dropped while the borrow is still live.
    let page_title = format!("Recent — Photographs by {OWNER_NAME}");
    let crumbs = [
        Crumb {
            label: "Home".into(),
            url: Some("/".into()),
        },
        Crumb {
            label: "Recent".into(),
            url: None,
        },
    ];
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block({
                let h = Head::new(&page_title, site_description(), "/recent")
                    .scripts(&["/static/lightbox.js", "/static/collapse.js"])
                    .preload(lcp);
                match og {
                    Some(url) => h.og_image(url),
                    None => h,
                }
            }))
            body {
                (site_header(Nav::Recent))
                // `recent` rather than `portfolio`: this page lays its photos
                // out at Browse's density and full width, not the home page's
                // two wide columns with deep side margins.
                main.recent {
                    (crumbs_nav(&crumbs))
                    (page_heading("Recent photographs"))
                    (gallery_sections(groups))
                }
                (site_footer())
            }
        }
    }
}

/// `/notify` — pick the people you want to hear about, and where.
///
/// Checkboxes rather than the `aria-pressed` button the `/all` favorites control
/// uses: this is a real form that has to submit, and a native checkbox does that
/// with JavaScript disabled and lands in the tab order for free. The switch
/// *look* is shared with that control through `.switch-track`/`.switch-thumb`.
///
/// Both handle fields are rendered. `notify.js` hides whichever one the selected
/// channel does not need; with no JavaScript both stay visible and the server
/// reads the one matching the chosen radio, so the page never becomes unusable.
///
/// `message` is `(text, is_error)` — the outcome of a submission or a
/// confirmation, shown above the form. The form is rendered either way so a
/// subscriber can immediately correct a mistake.
pub fn notify_page(
    people: &[PersonEntry],
    selected: &[String],
    all_rolls: bool,
    message: Option<(&str, bool)>,
) -> Markup {
    let page_title = format!("Notifications — Photographs by {OWNER_NAME}");
    let crumbs = [
        Crumb {
            label: "Home".into(),
            url: Some("/".into()),
        },
        Crumb {
            label: "People".into(),
            url: Some("/people".into()),
        },
        Crumb {
            label: "Notifications".into(),
            url: None,
        },
    ];
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(
                // Kept out of search results: it is a form, not content, and an
                // indexed signup page is a magnet for form spam.
                Head::new(&page_title, site_description(), "/notify")
                    .scripts(&["/static/notify.js"])
                    .noindex(),
            ))
            body {
                (site_header(Nav::People))
                main {
                    (crumbs_nav(&crumbs))
                    (page_heading("Photo notifications"))
                    @if let Some((text, is_error)) = message {
                        p.notify-message.is-error[is_error] { (text) }
                    }
                    form.notify-form method="post" action="/notify" {
                        @if !NOTIFY_INTRO.is_empty() {
                            p.notify-intro { (NOTIFY_INTRO) }
                        }
                        // Honeypot: invisible to people, irresistible to the
                        // bots that fill every field they can see. A non-empty
                        // value is the cheapest possible spam signal.
                        label.notify-hp aria-hidden="true" {
                            "Website"
                            // `tabindex="-1"` as well as the aria-hidden on the
                            // wrapper: hiding a focusable element from the
                            // accessibility tree while leaving it in the tab
                            // order strands a keyboard user on a field their
                            // screen reader never announced.
                            input type="text" name="website" tabindex="-1"
                                  autocomplete="off" aria-hidden="true";
                        }
                        fieldset.notify-fieldset {
                            legend { "People" }
                            // Above the list and set apart, because it is not a
                            // person: ticking it means every new roll whoever is
                            // in it, and it composes with any names below rather
                            // than replacing them.
                            label.person-toggle.all-rolls-toggle {
                                input type="checkbox" name="all_rolls" value="on" checked[all_rolls];
                                span.switch-track { span.switch-thumb {} }
                                span.person-name { (NOTIFY_ALL_ROLLS_LABEL) }
                            }
                            @if people.is_empty() {
                                p.empty { "Nothing here yet." }
                            } @else {
                                ul.person-list {
                                    @for person in people {
                                        li {
                                            label.person-toggle {
                                                input type="checkbox"
                                                      name="person"
                                                      value=(person.name)
                                                      checked[selected.iter().any(|s| s == &person.name)];
                                                span.switch-track { span.switch-thumb {} }
                                                span.person-name { (person.name) }
                                                span.person-count { (person.photo_count) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        fieldset.notify-fieldset {
                            legend { "Where" }
                            div.channel-choice {
                                label.channel-option {
                                    input type="radio" name="channel" value="email" checked;
                                    span { "Email" }
                                }
                                label.channel-option {
                                    input type="radio" name="channel" value="discord";
                                    span { "Discord" }
                                }
                            }
                            label.handle-field data-channel="email" {
                                span.handle-label { "Email address" }
                                input.handle-input type="email" name="handle_email"
                                      autocomplete="email" maxlength="254";
                            }
                            label.handle-field data-channel="discord" {
                                span.handle-label { "Discord user ID" }
                                input.handle-input type="text" name="handle_discord"
                                      inputmode="numeric" autocomplete="off" maxlength="20";
                                @if !NOTIFY_DISCORD_HINT.is_empty() {
                                    span.handle-hint { (NOTIFY_DISCORD_HINT) }
                                }
                            }
                        }
                        button.notify-submit type="submit" { "Subscribe" }
                    }
                }
                (site_footer())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// /all — folder tree sidebar
// ---------------------------------------------------------------------------

/// One directory in the `/all` sidebar tree.
///
/// `walk_groups` only emits a [`FolderGroup`] for directories that *directly*
/// hold JPEGs, so a year folder whose photos all live in subfolders never got a
/// section — and therefore had nothing to collapse. The tree is rebuilt here
/// from the group paths, so every level becomes a node the reader can fold,
/// image-bearing or not.
struct TreeNode {
    /// Last path segment. The root carries a display name instead.
    name: String,
    /// Folder path relative to the photos root; `""` is the root.
    path: String,
    /// DOM id of this folder's section, so a tree row can link to it.
    id: String,
    browse_url: String,
    /// Images directly in this folder.
    direct: usize,
    /// Images in this folder and every descendant.
    total: usize,
    /// Distance from the root, used to indent the inline section headings that
    /// stand in for the tree on narrow screens.
    depth: usize,
    children: Vec<TreeNode>,
}

impl TreeNode {
    fn new(name: &str, path: &str) -> Self {
        let browse_url = if path.is_empty() {
            "/browse".to_string()
        } else {
            format!("/browse/{}", crate::handlers::encode_path(path))
        };
        Self {
            name: name.to_string(),
            path: path.to_string(),
            id: String::new(),
            browse_url,
            direct: 0,
            total: 0,
            depth: 0,
            children: Vec::new(),
        }
    }

    /// Pre-order pass that mints DOM ids, records depth, and rolls the image
    /// counts up the tree. Ids are positional rather than derived from the
    /// path: folder names may contain anything, and two distinct paths must
    /// never collide on one id.
    fn finish(&mut self, next_id: &mut usize, depth: usize) -> usize {
        self.id = format!("folder-{}", *next_id);
        *next_id += 1;
        self.depth = depth;
        self.total = self.direct;
        for child in &mut self.children {
            self.total += child.finish(next_id, depth + 1);
        }
        self.total
    }
}

/// Sort key for a top-level `/all` folder: the leading run of digits in its
/// name, when it parses as a number. Top-level folders are years, so "2024"
/// keys on 2024 and "2024-summer" keys on 2024 too; a folder with no leading
/// digits (or a digit run too long to be a year) has no key. Returning `None`
/// rather than a fallback number keeps such folders out of the numeric order
/// entirely instead of pretending they are year 0.
fn leading_year(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Rebuild the directory hierarchy from the flat group list. Below the top
/// level, children keep the order in which `walk_groups` first mentioned them,
/// which is alphabetical pre-order, so the tree and the page scroll in the same
/// sequence. The top level is re-sorted newest year first — see below.
fn build_tree(groups: &[FolderGroup]) -> TreeNode {
    let mut root = TreeNode::new("All photos", "");
    for g in groups {
        let node = if g.path.is_empty() {
            &mut root
        } else {
            let mut cur = &mut root;
            let mut acc = String::new();
            for seg in g.path.split('/') {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(seg);
                let idx = match cur.children.iter().position(|c| c.name == seg) {
                    Some(i) => i,
                    None => {
                        cur.children.push(TreeNode::new(seg, &acc));
                        cur.children.len() - 1
                    }
                };
                cur = &mut cur.children[idx];
            }
            cur
        };
        node.direct = g.images.len();
    }
    // Years read newest-first: the library is browsed from the most recent work
    // backwards, so 2026 belongs above 2025. Compare the parsed number rather
    // than the string — string order only happens to work while every year has
    // the same digit count, and it would put "999" above "2026". Folders whose
    // name has no year in it (one-off buckets like "misc") sink below every year
    // instead of interleaving with the timeline; `sort_by_key` is stable, so
    // among themselves they keep the alphabetical order `walk_groups` produced.
    // Only the top level is touched: the plan item is about years, and a year's
    // subfolders are names, not dates, so alphabetical still reads best there.
    // This runs before `finish` so the positional DOM ids, the sidebar rows and
    // the page sections are all minted in this one order.
    root.children.sort_by_key(|c| match leading_year(&c.name) {
        Some(year) => (0, std::cmp::Reverse(year)),
        None => (1, std::cmp::Reverse(0)),
    });
    root.finish(&mut 0, 0);
    root
}

/// Chevron shared by the tree twisties and the inline section headers, so both
/// rotate identically when their folder closes.
fn chevron() -> Markup {
    html! {
        svg viewBox="0 0 24 24" aria-hidden="true" focusable="false" {
            polyline points="6,9 12,15 18,9" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" {}
        }
    }
}

fn folder_icon() -> Markup {
    html! {
        svg.tree-icon viewBox="0 0 24 24" aria-hidden="true" focusable="false" {
            path d="M3.5 6.5a1.5 1.5 0 0 1 1.5-1.5h3.6l1.8 2h8.1a1.5 1.5 0 0 1 1.5 1.5v9a1.5 1.5 0 0 1-1.5 1.5H5a1.5 1.5 0 0 1-1.5-1.5z"
                fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round" {}
        }
    }
}

/// The sticky left-hand tree. Hidden below the desktop breakpoint, where the
/// inline section headers carry the same collapse state instead.
fn folder_tree_sidebar(root: &TreeNode) -> Markup {
    html! {
        aside.tree-sidebar aria-label="Folder tree" {
            div.tree-head {
                span.tree-title { "Albums" }
                div.tree-head-actions {
                    button.tree-action type="button" data-tree-all="open" title="Expand all" { "Expand" }
                    button.tree-action type="button" data-tree-all="close" title="Collapse all" { "Collapse" }
                }
            }
            div.tree-scroll {
                ul.tree-list.tree-root {
                    (tree_row(root))
                }
            }
            div.tree-search {
                input.tree-search-input type="search" placeholder="Search folders…" aria-label="Search folders" autocomplete="off";
            }
        }
    }
}

fn tree_row(node: &TreeNode) -> Markup {
    let has_children = !node.children.is_empty();
    html! {
        li.tree-node data-path=(node.path) data-name=(node.name.to_lowercase()) data-target=(node.id) {
            div.tree-row {
                @if has_children {
                    button.tree-twisty type="button" aria-expanded="true"
                        aria-label=(format!("Collapse {}", node.name)) {
                        (chevron())
                    }
                } @else {
                    span.tree-twisty.tree-twisty-leaf aria-hidden="true" {}
                }
                a.tree-link href=(format!("#{}", node.id)) {
                    (folder_icon())
                    span.tree-name { (node.name) }
                    span.tree-count { (node.total) }
                }
            }
            @if has_children {
                ul.tree-list {
                    @for child in &node.children {
                        (tree_row(child))
                    }
                }
            }
        }
    }
}

/// Heading for one `/all` section. Folders with no photos of their own still
/// get one: on narrow screens, where the sidebar is hidden, it is the only
/// handle for collapsing a whole year or trip.
fn tree_section_heading(node: &TreeNode, group: Option<&FolderGroup>) -> Markup {
    let label = if node.path.is_empty() {
        "Photos (root)"
    } else {
        node.path.as_str()
    };
    let count = match group {
        Some(g) => g.images.len(),
        None => node.total,
    };
    html! {
        h2 {
            button.collapse-toggle type="button" aria-label="Collapse folder" aria-expanded="true" {
                (chevron())
            }
            a.section-label href=(group.map_or(node.browse_url.as_str(), |g| g.browse_url.as_str())) { (label) }
            span.section-count { "(" (count) ")" }
        }
    }
}

/// Emit one section per tree node, in the same pre-order the sidebar lists, so
/// clicking a row always scrolls to a target that exists. `eager` is the
/// remaining above-the-fold image budget: the first section that actually has
/// photos consumes it, everything below stays lazy.
fn tree_sections(node: &TreeNode, groups: &[FolderGroup], eager: &mut usize) -> Markup {
    let group = groups.iter().find(|g| g.path == node.path);
    html! {
        @match group {
            Some(g) => {
                section.gallery data-path=(node.path) id=(node.id) style=(format!("--depth:{}", node.depth)) {
                    (tree_section_heading(node, Some(g)))
                    (image_grid(&g.images, false, std::mem::take(eager), g.favs_count))
                }
            }
            None => {
                section.gallery.folder-node data-path=(node.path) id=(node.id) style=(format!("--depth:{}", node.depth)) {
                    (tree_section_heading(node, None))
                }
            }
        }
        @for child in &node.children {
            (tree_sections(child, groups, eager))
        }
    }
}

/// `/all`: the whole library on one page, with a collapsible folder tree down
/// the left on desktop and inline collapsible headers on narrow screens. Both
/// drive the same per-path open/closed state in `collapse.js`.
pub fn all_photos_page(crumbs: &[Crumb], groups: &[FolderGroup]) -> Markup {
    let root = build_tree(groups);
    let mut eager = EAGER_TILES;
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(
                Head::new(
                    &format!("All Photographs — {OWNER_NAME}"),
                    &format!("Every photograph by {OWNER_NAME}, grouped by folder — the complete scanned film archive on one page."),
                    "/all",
                )
                .scripts(&["/static/lightbox.js", "/static/collapse.js", "/static/favs.js"]),
            ))
            body {
                (site_header(Nav::All))
                main.all-layout {
                    (page_heading("Every photograph by Paul Borrego"))
                    @if groups.is_empty() {
                        div.all-column {
                            (crumbs_nav(crumbs))
                            p.empty { "Nothing here yet." }
                        }
                    } @else {
                        (folder_tree_sidebar(&root))
                        div.all-column {
                            (crumbs_nav(crumbs))
                            section.all-controls {
                                button.favs-toggle type="button" aria-pressed="false" {
                                    span.favs-toggle-track { span.favs-toggle-thumb {} }
                                    span.favs-toggle-label { "Favorites only" }
                                }
                                // Duplicated from the sidebar head so the same
                                // reach exists on phones, where the tree is not
                                // rendered at all.
                                div.all-controls-actions {
                                    button.tree-action type="button" data-tree-all="open" { "Expand all" }
                                    button.tree-action type="button" data-tree-all="close" { "Collapse all" }
                                }
                            }
                            (tree_sections(&root, groups, &mut eager))
                        }
                    }
                }
                (site_footer())
            }
        }
    }
}

pub fn work_index_page(title: &str, crumbs: &[Crumb], items: &[WorkIndexEntry]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(Head::new(
                &format!("Photography {title} — {OWNER_NAME}"),
                &format!("Professional photography by {OWNER_NAME} — client galleries and photo deliveries."),
                "/work",
            )))
            body {
                (site_header(Nav::Work))
                main {
                    (crumbs_nav(crumbs))
                    (page_heading("Professional photography work by Paul Borrego"))
                    @if items.is_empty() {
                        p.empty { "No work yet." }
                    } @else {
                        section.work-index {
                            ul.work-cards {
                                @for j in items {
                                    li.work-card {
                                        a href=(j.url) {
                                            span.work-name { (j.name) }
                                            span.work-counts {
                                                (j.jpeg_count) " JPEG"
                                                @if j.raw_count > 0 { " · " (j.raw_count) " RAW" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                (site_footer())
            }
        }
    }
}

/// One preview tile's `<img>`, with width/height attributes when we have
/// them so the browser reserves the right space pre-load and `loading=lazy`
/// only fetches tiles near the viewport.
fn work_preview_img(p: &WorkFeedPhoto) -> Markup {
    html! {
        @match p.preview_dims {
            Some((w, h)) => img src=(p.preview_url) alt=(p.name) loading="lazy" decoding="async" width=(w) height=(h);,
            None => img src=(p.preview_url) alt=(p.name) loading="lazy" decoding="async";,
        }
    }
}

/// One row in the bulk-download bar — a label plus JPEG/RAW/Both submit
/// buttons. The row is its own `<form>` so the `<input name="scope">` hidden
/// field rides along regardless of which kind button is pressed; no
/// JavaScript needed.
fn download_scope_row(label: &str, scope: &str, counts: &WorkCounts, action: &str) -> Markup {
    use crate::work::{DownloadKind, Scope};
    let parsed = Scope::parse(scope).expect("download_scope_row called with unknown scope");
    let jpeg = counts.count(parsed, DownloadKind::Jpeg);
    let raw = counts.count(parsed, DownloadKind::Raw);
    let both = counts.count(parsed, DownloadKind::Both);
    html! {
        form.dl-row method="post" action=(action) {
            input type="hidden" name="scope" value=(scope);
            span.dl-row-label { (label) }
            div.dl-buttons {
                button type="submit" name="kind" value="jpeg" disabled[jpeg == 0] {
                    "JPEG (" (jpeg) ")"
                }
                button type="submit" name="kind" value="raw" disabled[raw == 0] {
                    "RAW (" (raw) ")"
                }
                button type="submit" name="kind" value="both" disabled[both == 0] {
                    "Both (" (both) ")"
                }
            }
        }
    }
}

/// Job detail view: grid sections like /all but with bigger tiles. The page
/// has two visual states gated by an HttpOnly cookie:
///   - **Pre-auth**: only the password form is interactive; tile links omit
///     `data-download` so the lightbox download button stays hidden.
///   - **Post-auth**: password form disappears, bulk JPEG/RAW buttons appear,
///     and tile links carry `data-download` so the lightbox button renders.
pub fn work_page(
    name: &str,
    crumbs: &[Crumb],
    sections: &[WorkFeedSection],
    total_jpeg_count: u32,
    counts: WorkCounts,
    has_password: bool,
    authorized: bool,
    bulk_action: &str,
    auth_action: &str,
    error: Option<&str>,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            // noindex: this is a password-gated client delivery. To a crawler
            // every one of these is the same login stub with a different name —
            // near-duplicates that dilute the site and offer a searcher nothing.
            (head_block(
                Head::new(
                    &format!("{name} — Photo Delivery — {OWNER_NAME}"),
                    &format!("Private photo delivery for {name}."),
                    &format!("/work/{name}"),
                )
                .scripts(&["/static/lightbox.js", "/static/collapse.js"])
                .noindex(),
            ))
            body {
                (site_header(Nav::Work))
                main.work {
                    (crumbs_nav(crumbs))
                    h1.work-title { (name) }

                    @if let Some(msg) = error {
                        div.banner.banner-error role="alert" { (msg) }
                    }

                    @if !has_password {
                        div.work-downloads.dl-locked-banner {
                            p.dl-locked {
                                "Downloads are locked — no password has been set for this job yet."
                            }
                        }
                    } @else if authorized {
                        div.work-downloads {
                            (download_scope_row("Download all", "all", &counts, bulk_action))
                            (download_scope_row("Download edited", "edited", &counts, bulk_action))
                            (download_scope_row("Download original", "original", &counts, bulk_action))
                        }
                    } @else {
                        form.work-downloads.work-auth method="post" action=(auth_action) {
                            label.dl-label for="work-password" { "Password" }
                            input #work-password
                                type="password"
                                name="password"
                                autocomplete="off"
                                placeholder="Enter password to unlock downloads"
                                required;
                            div.dl-buttons {
                                button type="submit" { "Unlock downloads" }
                            }
                        }
                    }

                    @if total_jpeg_count == 0 {
                        p.empty { "No JPEG photos in this job yet." }
                    } @else {
                        @for section in sections {
                            section
                                class=(if section.default_open { "gallery work-gallery" } else { "gallery work-gallery collapsed" })
                                data-path=(section.data_path)
                                data-default-collapsed=(if section.default_open { "false" } else { "true" })
                            {
                                h2 {
                                    button.collapse-toggle type="button"
                                        aria-label=(if section.default_open { "Collapse folder" } else { "Expand folder" })
                                        aria-expanded=(if section.default_open { "true" } else { "false" }) {
                                        svg viewBox="0 0 24 24" aria-hidden="true" focusable="false" {
                                            polyline points="6,9 12,15 18,9" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" {}
                                        }
                                    }
                                    @if section.label.is_empty() {
                                        span.section-label { "(root)" }
                                    } @else {
                                        span.section-label { (section.label) }
                                    }
                                    span.section-count { "(" (section.photos.len()) ")" }
                                }
                                ul.grid.work-grid {
                                    @for p in &section.photos {
                                        li.tile {
                                            @if p.download_action.is_empty() {
                                                a href=(p.image_url) data-name=(p.name) {
                                                    (work_preview_img(p))
                                                }
                                            } @else {
                                                a href=(p.image_url)
                                                  data-name=(p.name)
                                                  data-download=(p.download_action) {
                                                    (work_preview_img(p))
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                (site_footer())
            }
        }
    }
}

/// About page: the prose from `ABOUT_PARAGRAPHS` in a single readable column,
/// with an optional portrait alongside. `portrait` is the preview URL for
/// `ABOUT_PORTRAIT_REL` plus its rendered dimensions, or `None` when that file
/// does not exist under the photos root.
pub fn about_page(portrait: Option<(&str, (u32, u32))>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            (head_block(
                Head::new(
                    &owner_title("About"),
                    site_description(),
                    "/about",
                )
                .jsonld(person_jsonld()),
            ))
            body {
                (site_header(Nav::About))
                main.about {
                    div.about-layout {
                        @if let Some((src, (w, h))) = portrait {
                            img.about-portrait
                                src=(src)
                                alt=(format!("Portrait of {OWNER_NAME}"))
                                width=(w) height=(h)
                                decoding="async"
                                fetchpriority="high";
                        }
                        div.about-body {
                            h1.about-name { (OWNER_NAME) }
                            @if !OWNER_TAGLINE.is_empty() {
                                p.about-tagline { (OWNER_TAGLINE) }
                            }
                            @for para in ABOUT_PARAGRAPHS {
                                p { (para) }
                            }
                            @if !ABOUT_LINKS.is_empty() {
                                h2.about-links-heading { "Elsewhere" }
                                ul.about-links {
                                    @for (label, href) in ABOUT_LINKS {
                                        li { a href=(href) { (label) } }
                                    }
                                }
                            }
                        }
                    }
                }
                (site_footer())
            }
        }
    }
}
