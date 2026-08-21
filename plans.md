# Plans

What this site is trying to be, and the directions that are open. Deliberately
at the level of ideas: no line numbers, no function names, no hex values. The
implementation-level detail — decisions already made, questions still open, the
code they touch — lives in `.claude/plans/`, indexed in the README there.

## What the site is

A film photographer's own archive, self-hosted on one box, published as a
website. Every photograph on it was shot, developed and scanned by the owner,
and the site is the repository of record rather than a selection from one.

That is the unusual thing about it, and most of the tension below comes from it.

## The archive and the portfolio are two different sites

Right now they share one. `/all` and `/browse` are a file browser: a folder
tree, nearly nine hundred photographs, counts on every heading. That is
genuinely the right tool for finding one negative among years of them, and it
should stay. `/` and `/recent` are presentation — the pages a stranger lands on.

The two keep bleeding into each other, and it always runs the same direction:
the archive's habits show up on the front door. Folder names stand in for
titles. File counts sit above the first photograph. A roll is identified by its
path.

The direction is not to hide the archive. It is to stop the front door being
built out of it — to let the presented surface have its own names, its own
order, and its own idea of what a section is, while the archive underneath goes
on being a file browser and gets better at that job.

## Presentation

The ambition is that someone who does not know the owner can arrive, understand
within a screen what they are looking at, and want to keep scrolling.

What that implies, roughly in order of how much it would change:

- **Named bodies of work.** A portfolio's unit is a series with a title. A
  file browser's is a directory. The site currently shows the second.
- **One measure.** Page width, and where the content's left edge sits, should
  not change as you move between pages.
- **`/work` is the page a client lands on** and is the thinnest page on the
  site. It is also half a delivery portal, which is why it looks like one.
  Those two jobs may not belong on one page.
- **A face on the About page.** The single highest-value thing that page is
  missing, and it is a file, not a feature.
- **The dark theme is the better presentation of the photographs.** Worth
  deciding whether it should be the default rather than the opt-in.

Two constraints sit over all of it. The photographs are the only thing on this
site allowed to be heavy — the markup, styling and scripts are a few tens of
kilobytes and should stay that way, and no feature here is worth a framework.
And every word a visitor reads is the owner's, so any of this that needs prose
stops at the point where prose is needed.

## The site is about people, not files

This is the part that makes it not a generic portfolio. There is a person index,
photographs are tagged by who is in them, and someone can ask to hear about a
named person and be messaged when a new roll carrying them goes up. The home
page invites exactly that.

It is also the part that touches other people's data, so it moves carefully:
nothing is sent to an unconfirmed address, and a page should not reveal whether
a given address is subscribed. Where it is still awkward is changing your mind
— saying "also tell me about her" currently means declaring your whole list
again from memory.

The open question underneath is how much of a person the site should show. A
face per person on the index is presentation. A page that answers "who is this
and where do they appear" is closer to a profile, and that is a decision about
other people, not about layout.

## What "recent" means

A question that keeps coming back in different clothes: is a photograph's date
when it was published or when it was taken? The archive's answer is
publication — a negative scanned today is new today, whatever year it was shot.
A person's page reads more naturally by capture date. Both are defensible and
they sort differently, so the site currently answers inconsistently by accident.

Worth settling once, deliberately, rather than per page.

## Durability

The photographs can be re-derived and the renditions can be rebuilt. Two things
cannot: the record of who consented to be messaged, and the record of what has
already been sent. Both exist in exactly one place, on one machine, and nothing
copies them anywhere.

Everything else in this area is smaller and in the same spirit — knowing when
the site is down rather than hearing about it, and not being able to deploy a
build whose tests fail.

## Beyond photographs

There is a private notes vault the site can already render, and an unbuilt idea
of a page for programming work. Whether this stays a photography site with a
side room, or becomes two things sharing a domain, is undecided — and the
content source for it is the decision that settles the shape, not the routing.

## Where the detail lives

`.claude/plans/` — one file per topic, plus the measured front-end audit and a
record of shipped work kept for its reasoning. Note that directory is
gitignored, which is discussed in its README and is worth a decision.
