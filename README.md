# CSS Loader
Dynamically loads CSS files from storage and reloads alongside Steam UI.

This version replaces the plugin's Python backend with a native program. It works with the standard Decky Loader and with [beallio's Decky Loader fork](https://github.com/beallio/decky-loader). The fork runs the native program directly. The standard Decky Loader starts it through a small Python file in the package (`main.py`). There is no Windows version.

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

We compared this version with the standard Python version 2.1.4 on a Steam Deck virtual machine, with the same themes. We tested each version twice, taking turns between them.

### With the standard Decky Loader

Tested with version 2.1.4-rust.1-rc.6 and Decky Loader 3.3.0 on 21 October 2026.

| What we measured | Python version | This version |
|---|---:|---:|
| Theme appears on a newly opened Steam page | 0.6–4.7 s | 0.1–0.2 s |
| Theme appears on a reloaded Steam page | 0.8–2.9 s | 0.01–0.02 s |
| Background checks inside each Steam page | 20 per minute | none |
| Reset themes | about 80–170 ms | about 60–150 ms |
| Memory use | about 69–72 MB | about 56–58 MB |
| CPU time used while idle for 2 minutes | about 0.10 s | about 0.01 s |
| CPU time used during 105 s of browsing | about 0.20 s | about 0.05 s |
| Screen smoothness while browsing (time per frame) | about 13 ms | about 13 ms |

The standard Decky Loader keeps a small Python helper running next to the native program. Most of this version's memory use is that helper.

### With beallio's Decky Loader fork

Tested on 19 October 2026. The fork runs the native program without the Python helper.

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
| Screen smoothness while browsing (time per frame) | about 13 ms | about 13 ms |

The plugin does not make Steam itself scroll or move more smoothly. The time Steam needs to draw each screen depends on the theme's CSS, and both versions add the same CSS.

These numbers come from a virtual machine, not a real Steam Deck. Your results can be different.

## Troubleshooting

**An update from a ZIP file does not finish (standard Decky Loader).** The standard Decky Loader cannot replace a running plugin when the ZIP file name is different from the plugin name. The old version keeps running. To fix it, rename the file to `CSS Loader.zip` and install it again. You can also uninstall CSS Loader first and then install the ZIP. Your themes and settings stay in place in both cases. Updates from the plugin store are not affected.

## Documentation


Read the documentation for this tool at [docs.deckthemes.com](https://docs.deckthemes.com/)