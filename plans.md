- [x] Increase Visibility for looking up website name
      Canonical URLs, robots.txt, sitemap.xml, Person JSON-LD, name-bearing
      titles and one h1 per page are in. Still to do by hand, outside the code:
      submit https://paulborrego.com/sitemap.xml in Google Search Console, and
      add the profile links to KNOWS_ABOUT/sameAs once there are any.
- [x] Reverse all tab to be most to least recent by year
      Top level of the /all tree now sorts newest year first, numerically, in
      both the sidebar and the sections. Non-year folders sink below the years
      keeping their alphabetical order; sub-folders are unchanged.
- [x] Write a proper intro for the Google result description
      HOME_INTRO and SITE_DESCRIPTION are filled in with your own words, and
      OWNER_TAGLINE is now "Full stack film photographer" (owner_title derives
      both page titles from it, so that one edit moved the titles too). The
      intro renders above the galleries, which is what gives Google prose to
      quote instead of the folder headings. Left to do outside the code:
      request reindexing of / in Search Console; snippets lag by days, and
      Google still picks its own for some queries.
      Note: the About page still opens "Self hosting enjoyer", which no longer
      matches the home page. Separate writing job.
- [x] Add an email list
      Grew into notifications by person: someone picks the people they want to
      hear about at /notify, or "Any new set of photos" for every roll, and
      chooses email or a Discord DM. Nothing is sent to an unconfirmed handle —
      a signup writes data/logs/pending.log and gets one message with a confirm
      link; following it moves them to data/logs/subscribers.log and deletes the
      pending row. Both channels confirm, because a mistyped Discord user ID is
      a valid ID belonging to a stranger.
      Sending is a deploy-time step, not a daemon: `portfolio-site notify
      --dry-run` prints what would go out, `notify` sends it and records each
      message in data/logs/notified.log. "New" comes from photos/.recent, not
      from EXIF, so a negative scanned today counts as new. Each recipient gets
      one message per drop naming only their people who actually appear, and
      re-running only sends what did not arrive.
      Wording lives in the site-copy block in src/views.rs; NOTIFY_INTRO and
      NOTIFY_CONFIRM_INTRO are still empty and waiting on you. Setup and the
      per-upload routine are in NOTIFY.md. Email needs a verified Resend
      domain before it will land anywhere but spam; Discord is live (Photo-Bot,
      in one server).
- [x] Remove browse tab, add a most recent tab
      /recent renders the folders named in photos/.recent, in that file's order,
      at Browse's density but with the photos uncropped. Favourites lead each
      roll with a rule between them and the rest, rather than sitting in a
      section of their own. The Browse tab is gone from the nav but /browse
      still serves — its URLs are indexed, the /all crumbs link into it, and
      every notification links rolls by their /browse path.
      Set the current drop with `portfolio-site recent set <dir>...`, which
      replaces the previous set and refuses a folder holding no visible photos.
      No rebuild needed; the file is read per request.
- Let someone change their preferences without starting over
      Re-subscribing already works — the last line for a handle wins, so a new
      confirmed signup supersedes the old one — but it is the wrong shape for
      "also tell me about Judy" or "stop telling me about Guin". The form does
      not know who you already follow, so it opens with every toggle off and you
      have to remember and re-tick your whole list, then confirm again. Getting
      one name wrong silently drops the rest.
      Wants a link that opens /notify with the current choices already ticked,
      which means a stable per-subscriber token in the message rather than the
      one-shot confirmation token, and an unsubscribe that is a button rather
      than an empty list. Worth deciding at the same time whether that page also
      shows what they have already been sent.

