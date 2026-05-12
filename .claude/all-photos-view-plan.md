# Plan: Add "All" view — flat-page gallery of every PNG, grouped by folder

## Context

The portfolio site currently exposes photos only via `/` (the `portfolio` subdir) and `/browse[/*path]` (one folder at a time). To survey the full collection the user has to click through each subdirectory.

The user wants a single page that walks the entire photos root and renders every non-hidden PNG, **grouped by the folder it lives in**, accessible from a new top-nav entry. Hidden filtering (filenames containing `hidden`, case-insensitive; dotfiles) stays in place. This makes the full collection scannable at a glance without changing the existing per-folder browse flow.

## Branching

Working tree is on `main`. Per project rules, all work happens on a feature branch — first action of the implementation phase is `git checkout -b feat/all-photos-view` (or similar). No commits to `main`.

## Approach

1. **New route**: `GET /all` → `handlers::all_photos`.
2. **Recursive walk** of `state.photos_root()` using an iterative BFS over `tokio::fs::read_dir`. For each directory visited, collect its non-hidden PNGs into a group keyed by the directory's path relative to the photos root. Empty groups are dropped.
3. **Grouped render**: a new view function emits one `<section>` per group with a heading (the relative folder path, linked to `/browse/<path>`) followed by the existing `.grid` of tiles. Reuses the current thumbnail/image URL scheme so caching, etag, and thumbnailing all keep working unchanged.
4. **Nav link**: add `<a href="/all">All</a>` to the topnav in `views::page` so it appears on every page.

## Files to modify

- `src/main.rs` — register the `/all` route.
- `src/handlers.rs` — add `all_photos` handler + a small `walk_groups` helper.
- `src/views.rs` — add `FolderGroup` struct, add `all_page(...)` function, and add the new nav link in the shared `<header>` (extract the header into a helper or duplicate two lines — see Step 3 below).

No changes needed to `paths.rs`, `state.rs`, `thumbs.rs`, or `static/style.css` (existing `.grid` / `.gallery` / `.dirs` styles cover the new layout).

## Implementation steps

### Step 1 — `views.rs`

- Add:
  ```rust
  pub struct FolderGroup {
      pub rel_path: String,   // "" for photos root, else "portfolio/2025"
      pub label: String,      // display label, e.g. "Photos (root)" or "portfolio/2025"
      pub browse_url: String, // "/browse" or "/browse/<encoded>"
      pub images: Vec<ImageEntry>,
  }
  ```
- Add a public function `pub fn all_page(title: &str, crumbs: &[Crumb], groups: &[FolderGroup]) -> Markup` that reuses the same `<head>`, `<header>`, breadcrumbs, and empty-state pattern, but renders one `<section class="gallery">` per group with an `<h2>` containing a link to the group's `browse_url` and a `<ul class="grid">` of tiles.
- Update the topnav in **both** `page()` and `all_page()` to include `<a href="/all">All</a>`. To avoid drift, extract the `<header class="site">…</header>` markup into a private `fn site_header() -> Markup` helper used by both views.

### Step 2 — `handlers.rs`

- Promote `is_png`, `is_hidden`, `encode_path`, and `join_rel` to `pub(crate)` (or keep private and call from inside this module — they're already used here). No external visibility change needed.
- Add a helper:
  ```rust
  async fn walk_groups(root: &Path) -> Result<Vec<FolderGroup>, StatusCode>
  ```
  Iterative BFS:
  - Queue: `VecDeque<(PathBuf abs, String rel)>` seeded with `(root, "")`.
  - For each dequeued dir, `read_dir` it; collect PNGs into a local `Vec<ImageEntry>`; push subdirectories onto the queue. Skip names starting with `.` and (for files) names where `is_hidden` returns true.
  - Sort PNGs in each folder alphabetically (case-insensitive), matching existing behavior.
  - After the loop, sort groups by `rel_path` ascending (so root comes first, then alphabetical descent).
  - Drop groups with empty `images`.
  - Build each group's `browse_url`: `"/browse"` if `rel_path` is empty, else `format!("/browse/{}", encode_path(&rel_path))`.
  - Build each group's `label`: `"Photos (root)"` if empty, else the `rel_path` itself.
- Add:
  ```rust
  pub async fn all_photos(State(state): State<AppState>) -> Response {
      match walk_groups(state.photos_root()).await {
          Ok(groups) => {
              let crumbs = vec![
                  Crumb { label: "Home".into(), url: Some("/".into()) },
                  Crumb { label: "All".into(),  url: None },
              ];
              views::all_page("All", &crumbs, &groups).into_response()
          }
          Err(status) => status.into_response(),
      }
  }
  ```

### Step 3 — `main.rs`

Add the route alongside the existing ones:
```rust
.route("/all", get(handlers::all_photos))
```

## Reused utilities (do not duplicate)

- `is_png`, `is_hidden`, `encode_path`, `join_rel` — `src/handlers.rs`
- `ImageEntry`, `Crumb` — `src/views.rs`
- Thumbnail generation via `/thumb/*path` — already routes through `thumbs::ensure_thumb`, nothing extra to wire up
- `safe_resolve` is **not** needed here — the walk starts from `state.photos_root()` directly and never accepts user-supplied paths

## Edge cases handled

- **Empty photos root** → empty `groups` vec → `all_page` shows the existing "Nothing here yet." empty state.
- **Hidden dotfile dirs** (`.git`, `.cache`, etc.) → skipped at walk time.
- **Files with `hidden` in name** → filtered via existing `is_hidden`.
- **Non-PNG files** → ignored via existing `is_png`.
- **Very deep trees** → iterative BFS, no recursion limit risk.
- **Symlinks** → uses `entry.file_type()` (does not follow symlinks for typing); matches behavior of existing `render_dir`, no new attack surface.

## Verification

1. `cargo build` — must compile.
2. `cargo test` — `paths::tests` must still pass (no changes there but verify nothing regressed).
3. `cargo run` from the project root, then in a browser:
   - Visit `http://localhost:3000/` — confirm the new "All" link appears in the topnav.
   - Click "All" — confirm one section per folder that contains PNGs, headings link to the matching `/browse/...` page, thumbnails load.
   - Confirm files with `hidden` in the name do **not** appear.
   - Confirm dotfile directories (if any exist under photos root) are skipped.
   - Visit `/browse` — confirm "All" link is also present there (shared header).
   - Click a folder heading on `/all` — should navigate to that folder's browse page.
4. Smoke-check with an empty photos directory (temporarily rename your real photos dir) — `/all` should render the empty state without panicking.

## Out of scope (explicitly not doing)

- Pagination / lazy loading beyond the existing `loading="lazy"` on thumbnails.
- A per-folder "view all here" button (rejected during clarification).
- Bypassing the hidden filter (rejected during clarification).
- Sorting controls / filters in the UI.
- Caching the walk result — directory listings are cheap; revisit only if the tree grows large.
