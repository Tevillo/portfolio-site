# Plans

What I want to build next, at the level of intent. No line numbers, no function
names — the implementation detail for each of these lives in `.claude/plans/`,
one file per topic.

This is not an assessment of the site as it stands. The measured audit of the
current front end is `.claude/plans/frontend-audit.md`, and shipped work is
`.claude/plans/done.md`.

## Safe copies of data

**Get `data/` off this box.** The record of who agreed to be messaged, and the
record of what has already been sent, exist in one place on one machine and
nothing copies them anywhere. Losing the first destroys consent I cannot
reconstruct; losing the second means the next send mails everyone about rolls
they were already told about. Everything else on this list can wait behind it.

## Page layout: home and About

**Names for the portfolio's sections.** The layout question is settled and
built: the portfolio is one section per page, photographs at their true aspect
ratio in three wide equal-width columns, ranked by star level, with any
panorama promoted to the full width, reached from a sub-tab strip under the
header. `/` is whichever section leads `SECTION_ORDER`,
and every other lives at `/portfolio/<slug>`. The section order is now mine to
set — `SECTION_ORDER` in `views.rs` takes slugs, and the first entry is the
front door. It is empty, so the front page currently opens on whatever tag sorts
first alphabetically, which is `misc`.

What is still open is the *names*. The headings are raw digiKam tag names, so
the page announces `misc` and `pastel`, and there is nowhere for a display name
to live yet. Adding that mapping is small; deciding what the names should be is
mine.

Two other empty slots the layout left behind, both needing words rather than
code:

- `SECTION_DESCRIPTIONS` in `views.rs` — the `<meta name="description">` for
  each section page. Empty entries fall back to `SITE_DESCRIPTION`, so all three
  sections currently share one description, which tells a search engine nothing
  about what separates them.
- Every `alt` attribute on the site is the photo's filename (`aruba-4.jpg`),
  which is useless to a crawler and unhelpful to a screen reader. It is also,
  besides `HOME_INTRO`, the only text in a photo page's body.

**Words for the About page.** Its structure is settled too — a photo, name,
facts and links column beside the prose, a /notify callout, and extra sections
(`ABOUT_SECTIONS`: Photography Work, with a button to /work and the contact
email, and Programming Projects) — so nothing about the page is waiting on a
decision any more.
Phrases in the prose that name a tab or section are linked through
`ABOUT_INLINE_LINKS`. The photo beside it is a random portfolio photograph, picked fresh on
every visit.

**Elsewhere links on About.** The About page has an "Elsewhere" block under the
name (`ABOUT_LINKS`) that is empty and hidden for now. Instagram, and any other
accounts I want people to find, go there when I decide to add them.

## Notifications

**Let someone change their mind.** Saying "also tell me about Judy" or "stop
telling me about Guin" currently means declaring the whole list again from
memory, and getting one name wrong silently drops the rest. This sat under
"People tab" while the tab was still being built; it is a /notify job and
nothing on /people is waiting on it.

## Archive

**A private archive for a few friends.** A page of photographs that are not
on the public site, open only to a handful of people I hand a key to myself.
There is no sign-up and no login page. I make a key for each person and send
them a link with it. Opening the link once unlocks the archive in that browser
for a year, and every visit renews it.
Anyone without a valid key gets the site's ordinary 404, so the page cannot be
told apart from one that does not exist.

**Hiding moves to tags.** Today a photograph is hidden by putting `hidden` in
its filename. That convention goes away entirely; hiding becomes two digiKam
tags:

- `hidden`: the photo is never shown anywhere on the site, archive included.
- `archive`: the photo is off every public page but shows in the archive.
  Sub-tags under `archive` become the archive's sections, the way
  `portfolio/<x>` does for the portfolio. A photo tagged with plain `archive`
  and no sub-tag goes in an unsorted section. If a photo somehow carries both
  `archive` and a `portfolio` tag, the archive wins.

Before the filename rule is removed, every file currently named `hidden` (13
of them, one a work job's card cover) has to be tagged, or it goes public on
deploy.

**Per-person keys I control.** Every key has a name attached, so I can revoke
one person without touching anyone else's, and the change takes effect on
their next request. The site records whose key opened the archive and when,
so I can tell if a link has been passed around. Key holders can download
originals.

**Invisible to search.** The archive is never in the sitemap and never linked
for anyone without a key, and every page and image in it tells crawlers not to
index it. Once someone has unlocked it, an Archive tab appears in their top nav
and in no one else's.

Words this needs from me: the tab's label, the display names of the sections,
and any heading on the page.

## Programming projects page

**A programming projects page.** Undecided, and the content source decides the
shape: notes in the vault, a constant in the code, a data file, or the GitHub
API. Pick that before anything else.

## Standing constraints

- The photographs are the only heavy thing on this site. Markup, styling and
  scripts are a few tens of kilobytes and stay that way. No feature is worth a
  framework.
- Every word a visitor reads is mine. Anything above that needs prose stops at
  the point where prose is needed.
- One box, self-hosted, deploy is a pull and a restart.
