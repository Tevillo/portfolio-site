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

**Names for the home page's sections.** The layout question is settled — the
home page is big photographs at their true aspect ratio, uncropped, one column
on a phone and two on a wide screen. What is left is what sits above them: the
headings are raw digiKam tag names, so the front door announces `misc`,
`pastel`, `2026/CALDWELL-35`. Needs somewhere for a display name to live before
the names themselves can be written.

**Words for the About page.** Its structure is settled too — one column of
paragraphs, an optional portrait beside them from `about.jpg`, a links list at
the bottom — so nothing about the page is waiting on a decision any more. It
still opens "Self hosting enjoyer", which no longer matches the home page
tagline, and there is no `about.jpg` yet.

## Work pages

**Make `/work` more professional.** It is the page a client lands on from a link
I sent them, and it is currently one bordered card carrying a folder slug and a
file count — a directory listing wearing a border. `frontend-audit.md` items 7
and 9 measure what is actually there.

Undecided, and it is really two decisions that keep getting made as one:

- **What a job presents.** Cover photo, client-facing title, date, one card per
  job. The cover can come from the job's `favs/` and needs no new data model.
  The title is copy, so it is mine.
- **What the handover feels like.** The password gate, the download buttons, and
  what a client sees after clicking one. This is the half that really is a
  delivery portal, and it can be plain without being unprofessional.

Presentation is the cheaper one to settle first: it changes what the page is,
and the handover has to sit inside whatever that turns out to be.

## People tab

**Covers on the People tab.** A tile per person — their photograph, their name,
their count — instead of a list of names. The cover should be a photo I tag
deliberately, not whatever sorts first.

**Let someone change their mind.** Saying "also tell me about Judy" or "stop
telling me about Guin" currently means declaring the whole list again from
memory, and getting one name wrong silently drops the rest.

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
