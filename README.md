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
- Themes load in name order, so the result is the same every time.

## Performance

We compared this version with the standard Python version on a Steam Deck virtual machine:

- **Turning a theme on or off** takes about 3 ms instead of about 13 ms.
- **Reloading the plugin** brings your themes back in about 65–85 ms instead of about 130 ms.
- **Memory use** is about 18 MB instead of about 42 MB.
- **Idle CPU use** is about a quarter of the Python version's.
- **Loading the theme list and resetting themes** take the same time in both versions.

These numbers come from a virtual machine, not a real Steam Deck. Your results can be different.

## Documentation


Read the documentation for this tool at [docs.deckthemes.com](https://docs.deckthemes.com/)