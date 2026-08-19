# Photo notifications — setup and operation

People tagged in digiKam can ask to hear when a new photo of them is published.
This file covers the one-time credential setup and the routine you run after
every upload.

Nothing here is secret in the repository: every credential lives in `data/`,
which is git-ignored and created mode 700.

## How it fits together

    photos/.recent  ──┐
                      ├──►  /recent          the page showing the current drop
                      └──►  notify           the messages announcing it
    data/logs/subscribers.log ──►  who wants to hear about whom
    data/logs/notified.log    ──►  what each of them has already been told

`photos/.recent` is the single declaration of "these folders are the new
photos". The website and the notifier read the same file, so the page and the
messages can never disagree about what is new.

## One-time setup

### Email (Resend)

1. Create an account at <https://resend.com> and add `paulborrego.com` as a
   sending domain.
2. Resend gives you three DNS records — SPF (`TXT`), DKIM (`TXT`), and a return
   path (`MX` + `TXT`). Add them at your DNS host and wait for Resend to show
   the domain as verified. **Do not skip this**: without it, mail sends but
   lands in spam, which is the same as not sending it.
3. Create an API key with send permission.
4. Write the two files:

       printf '%s\n' 're_your_api_key_here'                  > data/token/email.key
       printf '%s\n' 'Paul Borrego <photos@paulborrego.com>' > data/token/email.from
       chmod 600 data/token/email.key data/token/email.from

   The address in `email.from` must be on the domain you verified.

`data/token/email.endpoint` optionally overrides the API URL. It exists so the send
path can be pointed at a local mock and tested without mailing anyone; leave it
absent in production.

### Discord (bot)

Subscribers paste their numeric **user ID**, and the bot opens a DM with them.

1. Go to <https://discord.com/developers/applications> and hit **New
   Application**. Name it whatever you like — subscribers see this name.
2. Open the **Bot** tab and hit **Add Bot**. Under **Token**, hit **Reset
   Token** and copy the value. This is the only time it is shown.
3. No privileged intents are needed. Because subscribers supply their own user
   ID, the bot never has to look anyone up, so leave **Server Members Intent**
   off.
4. Open **OAuth2 → URL Generator**, tick the **bot** scope, and give it no
   permissions beyond the default. Open the generated URL and add the bot to a
   server you share with the people who will subscribe.
5. Save the token:

       printf '%s\n' 'your.bot.token' > data/token/discord.token
       chmod 600 data/token/discord.token

**The two constraints to know about.** A bot may only DM someone who shares a
server with it, and only if that person allows DMs from server members (Privacy
Settings → "Direct Messages" for that server). Either one failing comes back as
a `403`, which `notify` reports for that recipient and then carries on with
everyone else.

**Finding a user ID**, which is what the form asks for: open User Settings,
search for **developer**, turn on **Developer Mode**, then right-click your own
name and choose **Copy User ID**. Searching rather than naming the section
because Discord moves it — it has lived under Appearance and under Advanced. It is an 18-digit number. The form validates the shape, and
the confirmation DM proves the ID is really theirs — a mistyped ID belongs to
some other real person, which is exactly why the confirmation step exists.

## After every upload

1. Copy the photos in, and refresh the tag database:

       ./update_db.sh

2. Declare the new drop. This replaces the previous set:

       ./target/release/portfolio-site recent set 2026/some-roll 2026/another-roll

   `recent show` prints the current set. The command refuses a folder that does
   not exist or holds no visible photos, so a typo fails here rather than
   silently leaving a roll off the page.

3. Deploy, so `/recent` reflects the new set:

       ./reset.sh

4. **Dry run first, always:**

       ./target/release/portfolio-site notify --dry-run

   This prints every message it would send, to whom, and sends nothing. Read it.
   It is the only thing between a mistake in `.recent` and a message to every
   subscriber.

5. Send:

       ./target/release/portfolio-site notify

   Each recipient gets one message covering every new roll their people appear
   in. A recipient is recorded only after their message actually goes out, so
   re-running after a failure retries exactly what did not arrive, and running
   twice on an unchanged drop sends nothing.

`notify` is deliberately not wired into `reset.sh`. Deploying and announcing are
different decisions, and only one of them is reversible.

## Handling requests

**"Take that photo down."** The site hides any file with `hidden` in its name,
and any folder path containing a `negative` segment. Rename the file in digiKam,
re-run `./update_db.sh`, and it leaves every page — including the People pages
and any future notification.

**"Stop messaging me."** Append a line to `data/logs/subscribers.log` with an
empty `people` list and `all_rolls` off; the log is append-only and the last
line for a handle wins:

    {"channel":"email","handle":"them@example.com","people":[],"all_rolls":false,"ts":0,"token":""}

They can also just re-submit the form with nothing ticked — but that would send
another confirmation, so the line above is the direct route.

## Files in `data/`

Split in two, because the halves are operationally different: `logs/` is written
constantly by both the web server and `notify`, and is the thing to back up;
`token/` is written once by hand, and is the thing to guard. Both are mode 700,
and the files inside are 600.

| File | What it is |
| --- | --- |
| `logs/subscribers.log` | Confirmed subscriptions, append-only. Last line per handle wins; no people and no all-rolls means unsubscribed. |
| `logs/pending.log` | Signups awaiting confirmation. A row is deleted once its link is followed, and expired rows are swept at the same time. |
| `logs/notified.log` | One line per (recipient, folder) already announced. Deleting a line re-sends that roll. |
| `token/email.key`, `token/email.from` | Resend credentials. |
| `token/email.endpoint` | Optional API URL override, for testing. |
| `token/discord.token` | Discord bot token. |

None of it is regenerable, and none of it belongs in `cache/` — `warm --prune`
is allowed to delete from there.
