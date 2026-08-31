# portfolio-site

The server behind the photography site: a Rust/axum app that renders the
archive under `../photos` and serves password-gated client deliveries under
`/work`.

Everything below is about **client work** — adding a job, giving it a name and a
cover, setting its password, and prebuilding its download archives.

---

## Adding a client job

A job is a folder. There is no database entry to create and no restart to do:
the server reads the tree on every request, so a job exists the moment its
folder does.

### 1. Create the folder

```
photos/work/<job-name>/
```

`<job-name>` is the URL — `photos/work/marisol-sam-wedding/` is served at
`/work/marisol-sam-wedding`. Pick it once and leave it alone: it is what goes
into the link you send the client. It may not start with `.` and may not contain
`/` or `\`.

Use lowercase and hyphens. The site spells hyphens and underscores out as spaces
wherever a client reads a name, so `marisol-sam-wedding` displays as "marisol sam
wedding" without anything further. Step 2 is how you get better than that.

### 2. Arrange the photographs

Group them however the shoot was actually organised. The folder names become the
gallery's tabs:

```
photos/work/marisol-sam-wedding/
├── digital/
│   ├── edited/          ← browsable, tab "digital"
│   └── original/        ← download-only
├── medium-format/
│   └── positive/
│       ├── edited/      ← browsable, tab "medium format"
│       └── original/    ← download-only
└── large-format/
    ├── negative/        ← download-only
    └── positive/
        └── edited/      ← browsable, tab "large format"
```

Two segment names are load-bearing, anywhere in the path and case-insensitively:

- **`edited`** — the delivery. These folders become the browsable tabs, and they
  are what the "Download edited" row collects.
- **`original`** — the same frames unedited. Collected by "Download original",
  and deliberately **not** browsable: they usually outnumber the delivery several
  times over, and a client choosing favourites should be choosing from the
  edit.

Everything else in the path is just a label. Tab names drop the `edited` segment
and the word `positive` (which names which side of the film a scan came from, not
a set the client is choosing between), then spell out the separators — so
`medium-format/positive/edited` becomes the tab **medium format**. `negative` is
kept, so a hypothetical `medium-format/negative/edited` would read "medium format
negative" rather than colliding with its sibling. To change what is dropped, edit
`WORK_LABEL_DROP` in `src/views.rs`.

JPEGs at the job root are browsable too, under a tab named after the job.

**What counts as a file.** JPEGs are `.jpg`/`.jpeg`. RAWs are `cr2 cr3 nef nrw
arw raf dng orf rw2 pef srw x3f raw rwl iiq 3fr`, plus `psd`/`psb` — a layered
master is the source-of-truth file a client wants next to a retouched export, so
it ships in the RAW archive. Anything else (sidecars, `.DS_Store`) is ignored
everywhere. Dotfiles are skipped, which is why `.password` never lands in an
archive.

### 3. Name it — `title.txt`

The folder name is a path segment that has to stay put once links have gone out.
The title is what the client reads. Put it in a file at the job root:

```
photos/work/marisol-sam-wedding/title.txt
```

```
Marisol and Sam's Wedding
```

Taken **verbatim** — capitals, apostrophes, ampersands, and any hyphens you
actually meant. It feeds the `/work` card, the page's `<h1>` in the header, the
browser tab title and the link-preview title. The URL never changes with it.

- First non-empty line only. A trailing newline is fine; a second line is
  ignored, so you can keep notes below it.
- Trimmed, and capped at 120 characters.
- Missing or empty falls back to the folder name with its separators spelled out.
- Read on every request — no rebuild, no restart, no re-scan.

### 4. Give it a cover — the `thumbnail` tag

The card on `/work` shows one photograph per job. Which one is a judgement, so it
lives where the portfolio's judgements live: **digiKam**.

1. In digiKam, find the frame you want.
2. Tag it `thumbnail`. Tag the JPEG *and* its RAW if that is how you work — only
   the JPEG can be rendered, and only it is picked.
3. Copy the database over: `./update_db.sh`

The tag is matched by name at any depth, so typing "thumbnail" into digiKam's tag
box (which lands it at the root, beside `portfolio` and `People`) is enough.

Constraints worth knowing:

- Only photographs inside `photos/work/<job>/…` are considered. The tag does
  nothing anywhere else; a person's tile on /people is chosen by digiKam's own
  tag thumbnail instead — see "Faces on the People tab" below.
- Only `.jpg`/`.jpeg`. A filename containing `hidden`, or anything under a
  `negative/` folder, is excluded — the same visibility filter the portfolio and
  People pages use. So a negative scan cannot be a cover even though it can be in
  the job.
- Photographs digiKam has lost track of (status 3 — the row it keeps so tags
  survive a move) are ignored, so a card never points at a path that is gone.
- Tagging several in one job picks the first by album then filename. Tag one.
- **No tag means no cover**, and the card renders as a bare name and counts. There
  is no automatic fallback: if you add jobs faster than you tag them, that is what
  the page will show.

Unlike `title.txt`, this one needs the database copied before the site sees it.

### 5. Set the password

```
./set_password.sh marisol-sam-wedding
```

Prompts twice without echoing, trims, and writes `photos/work/<job>/.password`
at mode 600. The server reads it per request — no restart. Re-run it to change
a password.

The file is plaintext by design. It gates a gallery of photographs the client
already owns, not an account, and keeping it readable is what makes "what is the
password again" a one-line answer. Comparison is constant-time.

**No `.password` file means downloads are locked**, and the page says so. The
gallery is still browsable — the password gates downloads, not viewing.

The script offers to prebuild the archives when it finishes. Say yes; that is
step 6.

### 6. Prebuild the archives

```
./portfolio-site prebuild marisol-sam-wedding
# or, for every job:
./portfolio-site prebuild --all
```

Builds all nine archives (3 scopes × 3 kinds) so the client's first download
click streams a finished file rather than waiting on a zip of several gigabytes.
Prints size and time per combination; combinations with no files (a job with no
RAWs, say) are skipped rather than failed.

Safe to run against a live server: archives are written to a temp file and
renamed, so the server only ever sees a complete one.

---

## How the pieces work

### Download scopes and kinds

The panel offers three rows × three buttons:

| | JPEG | RAW | Both |
|---|---|---|---|
| **Download all** | every JPEG | every RAW | one merged archive |
| **Download edited** | JPEGs under an `edited/` segment | RAWs under one | both |
| **Download original** | JPEGs under an `original/` segment | RAWs under one | both |

"Both" is a single merged archive rather than two downloads, because a browser
firing two at once is unreliable on mobile.

Each archive preserves the job's folder structure, so the client unzips into the
same `digital/edited/…` layout they saw on the page.

### The zip cache

```
cache/work/<job>-<scope>-<kind>.zip
```

Freshness is by mtime: an archive older than the newest file it contains is
rebuilt on request, otherwise it is streamed as-is. So **adding or replacing a
photograph invalidates the archives automatically** — but the next client to
click pays the rebuild. Re-run `prebuild` after changing a job's contents.

The cache is disposable; deleting it costs only the rebuild time.

### Renditions

Photographs are never served at full size on a page. Four downscales are built
on demand and cached under `cache/`:

| kind | max dimension | used by |
|---|---|---|
| `thumbs` | 400px | `/browse`, `/people`, `/all` grids |
| `medium` | 800px | second `srcset` candidate on natural-ratio grids |
| `preview` | 1600px | delivery-page tiles, `/work` card covers, link previews |
| `faces` | 320px square | `/people` tiles — a crop, not a downscale; keyed by person |
| `wide` | 3200px | full-width portfolio panoramas only |

`./portfolio-site warm` builds every missing rendition so the first visitor after
a deploy pays nothing; `warm --prune` also deletes renditions whose source is
gone. `reset.sh` runs `warm` as part of every deploy.

### Failed passwords

Five wrong passwords for one job and it stops checking for 15 minutes — a rolling
window, so a client who fumbles gets their gallery back without asking. The sixth
attempt is not checked at all, including a correct one, and is not itself
recorded (counting it would push the window forward on every retry and the pause
would never end).

The limit is **per job, not per visitor**: the site keeps no visitor log by
design, so a stranger guessing five times locks the real client out for the
quarter hour. `WORK_AUTH_MAX_TRIES` and `WORK_AUTH_WINDOW_SECS` in
`src/handlers.rs` are the two numbers.

Failures are counted in `data/logs/work-auth-failures.log` — job and timestamp,
nothing identifying — which is both what the limit reads and what the owner's
report means by "is someone guessing at this job".

### Faces on the People tab

`/people` is a tile per person, and the face on it comes from digiKam's own face
rectangles — the boxes it drew and you confirmed a name for.

Which of a person's faces gets used:

1. **Their tag thumbnail.** In digiKam, right-click the face you want and
   *Set as Tag Thumbnail*. That is the deliberate pick, it is one per person,
   and it is set in the same window where the faces are confirmed.
2. Otherwise the **biggest** face rectangle they have. The crop is enlarged from
   it, so the face with the most pixels is the one that survives being a tile.

So there is nothing to set up: confirming faces is enough to fill the page, and
setting a tag thumbnail is how you overrule the automatic pick for one person.
Either way it needs `./update_db.sh` before the site sees it.

**Set the tag thumbnail on a JPEG.** Only `.jpg`/`.jpeg` can be rendered, so a
tag thumbnail set on a TIFF or a RAW is an error and shows as a broken tile — it
is not quietly swapped for the JPEG beside it, and it does not quietly fall back
to another face. Both would hide a five-second fix behind a page that looks
right. `./portfolio-site warm` names every person in that state:

```
  FAILED face for Guin: …/zoo-4thJuly-13.tif is not a .jpg/.jpeg, and only
  those can be rendered — set the tag thumbnail on the JPEG instead
```

It happens easily: a face gets confirmed on whichever copy was open at the time,
and for a scan that is often the TIFF. Re-setting it on the JPEG beside it is
the whole fix.

Same visibility rules as everywhere else — nothing under a `negative/` folder,
nothing with `hidden` in the name — so a person's tile can never show a frame
the site would not publish.

A person with no confirmed face anywhere, and no tag thumbnail, gets their
initial in a plain disc rather than a photograph they merely appear in.

Crops are cached under `cache/faces/` and served from `/face/<person>`. They are
built by `./portfolio-site warm` along with everything else, and `warm --prune`
deletes the ones the database has stopped asking for — which includes the old
crop of a face you have since replaced. A visitor whose browser already has the
old crop keeps it for up to an hour; the URL is per person and does not change
when the pick does.

### Indexing

Delivery pages are `noindex`. To a crawler every one of them is the same login
stub with a different name. `/work` itself is indexed; the jobs it links to are
not.

---

## Deploying

```
./reset.sh
```

Pulls, builds release, relinks `./portfolio-site`, warms renditions, restarts
the service. Aborts on the first failure, so a broken build never restarts the
old binary as if it had succeeded.

`./update_db.sh` copies the digiKam database into place — needed after tagging
anything the site reads: `portfolio/*`, People, or a job's `thumbnail`.

## Layout

```
photos/          the archive (sibling of this checkout)
├── work/        client jobs
└── digikam4.db  tags: portfolio, People (faces + tag thumbnails), thumbnail
cache/           renditions and prebuilt zips — disposable
data/            subscriber logs and API credentials — not disposable
```

## Other commands

Any unrecognised subcommand prints the full usage — `./portfolio-site help` will
do it. (No arguments at all starts the web server, same as `serve`.) The usage
covers `recent` (what the `/recent` page shows), `notify` (the subscriber
mailout, with `--dry-run`), and `audit` (subscriber, download and per-job
figures).

## Tests

```
cargo test
```
