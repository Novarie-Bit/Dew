# dew

Canvas and Gradescope assignments in one terminal list, sorted by due date.

```
 dew  [all sources] [-done] [-past]                                         synced 2m ago
┌ 6 of 9 ─────────────────────────────────────────────────────────────────────────────────┐
│ Due              Left       Course        Assignment                       Status       │
│▌Fri Oct 16 23:59  -1d 6h CV Writing 1     Reading Response 7               Missing      │
│ Sat Oct 17 23:59  4h 59m GS CSE 247       Lab 5: Heaps                     To do        │
│ Sun Oct 18 23:59 19h 58m CV CSE 247       Problem Set 5                    To do        │
│ Mon Oct 19 23:59  2d 12h GS CSE 247       Homework 3                       To do        │
│ Fri Oct 23 23:59  5d 23h CV Writing 1     Midterm Essay                    To do        │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

- Pulls from **Canvas** (official REST API) and **Gradescope** (scrapes the web pages,
  since there's no public API)
- Hides finished work and items more than 14 days past due by default
- Drops Canvas copies of assignments that are really submitted on Gradescope
- Search, filter by source, open the selected item in your browser
- Keeps the last fetch on disk, so it opens instantly and still works offline
- `dew list` for plain output, `dew waybar` for a status bar module

## Install (Arch / CachyOS)

```sh
sudo pacman -S --needed rust cmake
git clone https://github.com/novarie-bit/dew && cd dew/packaging/arch
makepkg -si
```

Or without packaging it: `cargo install --path .` (puts `dew` in `~/.cargo/bin`).

## Sign in

Run `dew` and press `L` (the accounts screen also opens on first launch), or use the CLI.

### Canvas

Canvas wants a personal access token instead of your password, which also gets around
WashU SSO / 2FA:

1. Go to `https://canvas.wustl.edu/profile/settings` (**Account → Settings**)
2. **+ New Access Token**, give it a name, and copy the token
3. Paste it into dew, or run `dew login canvas`

For a different school, change the URL (`dew login canvas --url school.instructure.com`).

### Gradescope

There are two options:

- **Email + password**: `dew login gradescope`. If you normally use *School Credentials*
  (SSO), you can usually add a Gradescope password: log out, choose **Forgot your
  password?**, and enter your school email.
- **Browser cookies**: in a browser that's signed in to gradescope.com, open DevTools →
  Storage (Firefox) or Application (Chromium) → Cookies, and copy `signed_token` and
  `_gradescope_session`. Then paste `signed_token=…; _gradescope_session=…` into the
  cookies field or run `dew login gradescope --cookies`.

dew never saves your Gradescope password, only the session cookie (with "remember me"
on). When that expires, dew asks you to sign in again.

## Keys

| key | action |
| --- | --- |
| `j`/`k`, arrows | move |
| `g`/`G` | first / last |
| `Enter`, `o` | open in browser |
| `r` | refresh |
| `d` | show/hide finished work |
| `p` | show/hide items long past due |
| `s`, `Tab` | cycle Canvas / Gradescope / all |
| `/` | search (`Esc` clears it) |
| `L` | accounts |
| `?` | help |
| `q` | quit |

Rows are red when overdue, bold red when due within 24h, and yellow when due within 3 days.

## CLI

```
dew                 interactive view
dew list [-a] [--json] [--cached]
dew sync
dew waybar
dew login canvas|gradescope
dew logout [canvas|gradescope]
dew status          sign-in state and file locations
```

## Waybar

```jsonc
"custom/dew": {
  "exec": "dew waybar",
  "return-type": "json",
  "interval": 300,
  "format": "󰃭 {}",
  "on-click": "kitty -e dew"
}
```

The text is the number of items due in the next 7 days, including overdue ones. The
tooltip lists them, and the CSS class is `urgent`, `warning`, `normal`, or `empty`. See
`packaging/waybar/` for a style snippet. `dew waybar` only hits the network when the
cache is older than `refresh_minutes`.

## Files

| path | contents |
| --- | --- |
| `~/.config/dew/config.toml` | settings (created with defaults on first run) |
| `~/.config/dew/credentials.toml` | Canvas URL + token (mode 600) |
| `~/.local/share/dew/gradescope_cookies.json` | Gradescope session (mode 600) |
| `~/.cache/dew/assignments.json` | last fetch |

```toml
# config.toml
keep_past_days = 14          # keep showing items this many days after they were due
dedupe = true                # hide Canvas mirrors of Gradescope assignments
gradescope_all_terms = false # only the newest Gradescope term by default
refresh_minutes = 30         # auto-refresh interval (TUI and waybar)
opener = "xdg-open"
```

## Notes

- The Gradescope scraper depends on Gradescope's page markup. If they change it,
  assignments might come back empty or with missing dates. The parser lives in
  `src/gradescope.rs`, with HTML fixtures in `tests/fixtures/`.
- If your school has turned off Canvas access tokens, `dew login canvas` will fail with a
  401.
