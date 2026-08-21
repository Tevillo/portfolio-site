# Plans

What I want to build next, at the level of intent. No line numbers, no function
names — the implementation detail for each of these lives in `.claude/plans/`,
one file per topic.

This is not an assessment of the site as it stands. The measured audit of the
current front end is `.claude/plans/frontend-audit.md`, and shipped work is
`.claude/plans/done.md`.

## First

**Get `data/` off this box.** The record of who agreed to be messaged, and the
record of what has already been sent, exist in one place on one machine and
nothing copies them anywhere. Losing the first destroys consent I cannot
reconstruct; losing the second means the next send mails everyone about rolls
they were already told about. Everything else on this list can wait behind it.

## Photography, presented

**Portfolio formatting on the home page.** The home page stays at `/`. What it
should look like is open — big single-column photographs, justified rows at true
aspect ratios, or a uniform grid — and the layout decision and how much the
section headings should say are really one decision.

**About page structure.** Whether it carries a portrait and where, and whether
it is one column of paragraphs or headed blocks. That choice changes what the
paragraphs have to be, so it comes before rewriting them.

**A real 404 page.** `/browse` URLs are indexed and the archive gets
reorganised, so old links will rot. Right now that is a blank white page with no
way back.

## People

**Covers on the People tab.** A tile per person — their photograph, their name,
their count — instead of a list of names. The cover should be a photo I tag
deliberately, not whatever sorts first.

**Let someone change their mind.** Saying "also tell me about Judy" or "stop
telling me about Guin" currently means declaring the whole list again from
memory, and getting one name wrong silently drops the rest.

## Order

**`/all` by most recently changed**, within each year. The years stay newest
first; the rolls I touched last should lead their year.

**A person's photos newest first.** Their page currently opens on their oldest
photographs, which is the reverse of everywhere else.

Both turn on one unsettled question: whether a photograph's date is when it was
published or when it was taken. Worth answering once rather than per page.

## Knowing it works

**An audit report**, as a CLI command rather than a web route — subscribers,
downloads, and separately per client job, because the question there is not "how
is the site doing" but "did this client get their photos".

**A down detector**, pointed at `/version` so a wrong answer means a stale
deploy and not merely "up".

## New ground

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
