# CSS Loader
Dynamically loads CSS files from storage and reloads alongside Steam UI.

This version replaces the plugin's Python backend with a native program. It works only with [beallio's Decky Loader fork](https://github.com/beallio/decky-loader). The standard Decky Loader cannot run it, and there is no Windows version.

## Install

Install the `SDH-CssLoader-<version>.zip` package with Decky Loader's local file installer. Do not use GitHub's source-code ZIP. Your installed themes and settings stay in place.

## What's changed

Compared with the standard CSS Loader 2.1.4:

- The plugin's background program is rewritten in Rust, a faster language that uses less memory. The plugin looks and works the same.
- Themes are applied as soon as a Steam page opens or reloads, instead of on a timer.
- If Steam removes a theme's styling, the plugin restores it right away instead of checking every few seconds.
- Steam pop-up windows keep their themes after Decky Loader restarts.
- After the plugin restarts or updates, turning a theme off works right away. The Python version leaves the old styles in place until you reset themes.
- Themes load in name order, so the result is the same every time.
- Theme class-name translations come from [steamui-watch](https://github.com/beallio/steamui-watch), which checks Valve's stable and beta releases each day. The plugin checks for new translations every hour. If a download is damaged, it keeps the last working translations. The standard CSS Loader uses DeckThemes' mapping service.

## Performance

We compared this version with the standard Python version 2.1.4 on a Steam Deck virtual machine on 19 October 2026. We tested each version twice, taking turns between them, with the same themes.

| What we measured | Python version | This version |
|---|---:|---:|
| Theme appears on a newly opened Steam page | 0.8–4.9 s | 0.1–0.2 s |
| Theme appears on a reloaded Steam page | 0.6–2.7 s | about 0.02 s |
| Background checks inside each Steam page | 20 per minute | none |
| Reload the plugin until themes are back | about 75–95 ms | about 40–45 ms |
| Reset themes | about 60 ms | about 15–40 ms |
| Turn a theme on or off | about 2–3 ms | about 2–3 ms |
| Load the theme list | under 1 ms | under 1 ms |
| Memory use | about 64 MB | about 15–17 MB |
| CPU time used while idle for 2 minutes | about 0.07 s | 0–0.01 s |
| CPU time used during 105 s of browsing | about 0.12 s | about 0.03 s |
| Screen smoothness while browsing (time per frame) | about 13 ms | about 13 ms |

The plugin does not make Steam itself scroll or move more smoothly. The time Steam needs to draw each screen depends on the theme's CSS, and both versions add the same CSS.

These numbers come from a virtual machine, not a real Steam Deck. Your results can be different.

## Documentation


Read the documentation for this tool at [docs.deckthemes.com](https://docs.deckthemes.com/)