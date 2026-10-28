# trax

A Material Design 3 music player written in Rust ([Slint](https://slint.dev) UI, [rodio](https://github.com/RustAudio/rodio) audio). Dark and light mode, Spotify-like behaviour, one library across several sources.

> Runs on Windows and Linux (x86-64; X11 and Wayland) from the same code, as a native binary on both. macOS is planned.

## Features

- **Sources (connectors)** — each one can be switched on or off in Settings
  - **Local music**: any number of folders. Tags, embedded covers and `cover.jpg`/`folder.jpg` are read automatically. Rescans are incremental (unchanged files are skipped).
  - **Single files**: open any song on your PC with trax (double-click once trax is the default player, or right-click → Open with). It plays right away and joins the library on its own, without adding its folder. If the file is gone when you play it later, it leaves the library again. Starting trax while it is already running hands the songs to the open window.
  - **Nextcloud**: WebDAV with an app password. Tracks are streamed into a local cache on first play, and the next track is prefetched.
  - **YouTube**: search and play through [yt-dlp](https://github.com/yt-dlp/yt-dlp), which you install yourself (see [Setting up yt-dlp](#setting-up-yt-dlp)). Audio is fetched as AAC/m4a, so ffmpeg isn't needed.
  - **SoundCloud**: same yt-dlp, switched on separately (Settings → Sources). Search results show the track artwork; audio is cached as MP3 or AAC. Go+ tracks only play their 30-second preview.
  - **Spotify + Wikipedia (metadata)**: artist images, genres and followers from the Spotify Web API (client credentials), short bios from Wikipedia, and missing album artwork (also replaces YouTube video thumbnails). Can update automatically while you listen (artist info older than 30 days is refreshed); wrong matches can be fixed by hand and are then kept.
- **Playback**: queue with animated drag-to-reorder, crossfade (0.5–5 s, optional), preloading of upcoming Nextcloud/YouTube/SoundCloud tracks (optional), play next / add to queue, shuffle, repeat all / one, seek, volume with mouse-wheel support, mute.
- **10-band equalizer** with your own "Custom" curve plus presets (Bass boost, Rock, Vocal, …) and automatic headroom so boosts don't clip.
- **Sound effects** (Sound panel → Effects / Speed & pitch): tempo and pitch independently (phase vocoder), reverb, echo, sidechain pump, 8D rotation, stereo width / balance, low- / high-pass, vocal remover (karaoke), limiter, and presets (Nightcore, Slowed + reverb, Vaporwave, 8D, Club pump, Lo-fi, Concert hall, Karaoke).
- **Crossfade options**: curve (linear, equal power, S-curve), crossfade on manual skips, fade on pause/resume.
- **Favourite song sections**: per track start and end points (Edit info), so only your favourite part plays.
- **Favourites**: star any track (lists, player bar); starred tracks form the built-in Favourites playlist.
- **Visualizer**: bars in the player bar (optional); fullscreen background can be the artwork, the visualizer only, or both, in five styles (bars with falling peaks, mirror, oscilloscope wave, circle, LED blocks).
- **Resume where you left off** (optional): the queue, the last song and its position come back after a restart.
- **Audio cache limit**: downloaded YouTube / Nextcloud songs are kept until the limit; the least recently played are removed first.
- **Appearance**: accent colour (palette generated from it), light / dark / system, corner style, list density, interface size, font, list columns, compact sidebar. The corner style also applies to the window corners on Windows 11 (including the tray mini player); on Linux the desktop draws the window corners. Themes can be exported and imported as JSON (see below). **Reset to default** puts the whole design back to the defaults (saved themes are kept).
- **Settings** are split into tabs: Appearance, Playback, Sources, Storage & data, Performance.
- **Low-end mode** (Settings → Performance): turns off all animations, shadows, the loading animation, the visualizer and the sound effects to keep CPU/GPU use minimal. Options that need them are greyed out; your choices are kept and come back when you switch it off.
- **Playlists**: create, delete, and edit on screen (drag to reorder, remove tracks, name, description, artwork). Add tracks from any list through the ⋮ menu or by right-clicking; if you're playing that playlist, the track is queued too.
- **Share playlists as a code**: the share button on a playlist creates a code (one line starting with `trax1:`) that holds the whole playlist; the other person pastes it under **+ → Import…**. No server or account is involved. Only YouTube and SoundCloud songs are shared, since songs from your PC or Nextcloud can't be played by others.
- **Import from YouTube Music, YouTube and Spotify**: paste the link to a public playlist under **+ → Import…** (the same box as share codes). YouTube and YouTube Music playlists (and albums) are read with yt-dlp. For Spotify playlists and albums, trax reads the song list from Spotify's public embed player (no login or Spotify app needed, the first 100 songs) and looks up each song on YouTube, preferring the version whose length matches; songs it can't find are left out. Needs YouTube switched on. Private playlists, Liked Songs and `spotify.link` short links can't be imported (open the short link and copy the full `open.spotify.com` link).
- **Edit track info**: title, artist, album and artwork per track. Stored in trax (the file isn't touched) and kept across rescans.
- **Artist profiles**: click any artist name (track lists, player bar, fullscreen, Home) for a page with image, genres, bio, albums and all their tracks in your library. Library has an Artists view. Image and bio can be set by hand.
- **Auto playlists** on the Home page: Top this week / month / year, all-time favourites, recently played, recently added, not played in a long time, forgotten favourites, never played. Any of them can be saved as a regular playlist, and they can be switched off entirely.
- **Stats**: hours listened this week, this year and in total, plus your top artists this year.
- **Search** across your library and YouTube.
- **Fullscreen view** with artist artwork, set manually or fetched from Spotify (Esc to exit, Space to play/pause, ←/→ to skip).
- **Default music player** (Settings → System & updates): registers trax for MP3, FLAC, M4A, AAC, OGG and WAV (per user, no admin rights). On Windows it opens Default apps, where you confirm trax; the uninstaller removes the registration. On Linux it adds trax to your app launcher and "Open with" (a desktop entry and icon in `~/.local/share`) and makes it the default in `~/.config/mimeapps.list`. `trax --register` / `--unregister` do the registration from the command line.
- **System tray**: shows the current song. Left-click opens a mini player with prev/play/next and volume; right-click opens a menu. Closing the window keeps trax running in the tray (optional). On Linux the icon needs a desktop with a StatusNotifier tray (KDE Plasma, XFCE, Cinnamon, Budgie, or GNOME with the AppIndicator extension, which Ubuntu has on by default); without one, closing the window quits trax. Some desktops open the menu on left-click as well.
- **Media keys** and the Windows media overlay (SMTC); on Linux, MPRIS (media keys, the desktop's media controls, `playerctl`).
- **Listening parties**: host a party with a room code (and optional password), friends join and hear the same music in sync. No server; see [Listening parties](#listening-parties).
- **Friends**: add friends with their friend code, see what they're listening to, invite them to your party or ask to join theirs. No server or account; see [Friends](#friends).
- **Export / import**: one `.traxbak` file holds the library, your track edits, play history, playlists (with descriptions and artwork), EQ, all settings, covers and artwork. Paths to covers/artwork are re-pointed automatically on the new PC (covered by an automated round-trip test). Import it on a new PC, then use **Re-map** if your music folder moved (e.g. `D:\Music` → `E:\Music`).

## Setting up yt-dlp

YouTube and SoundCloud need [yt-dlp](https://github.com/yt-dlp/yt-dlp), a separate program. trax doesn't include it and doesn't download it for you.

1. Get it for your system:
   - **Windows**: `yt-dlp.exe` from the [yt-dlp releases](https://github.com/yt-dlp/yt-dlp/releases), or `winget install yt-dlp`.
   - **Linux** (including the .deb): `yt-dlp_linux` from the [yt-dlp releases](https://github.com/yt-dlp/yt-dlp/releases) (not `yt-dlp.exe`, which only runs on Windows), or `pipx install yt-dlp`. trax makes a downloaded `yt-dlp_linux` executable itself and also finds a pipx install in `~/.local/bin`. Distribution packages (`apt install yt-dlp`) are often too old for YouTube.
2. Make it findable by trax, in one of these ways (checked in this order):
   - enter its full path in **Settings → Sources → yt-dlp path**,
   - put `yt-dlp.exe` / `yt-dlp_linux` in trax's yt-dlp folder, `%APPDATA%\trax\data\bin` or `~/.local/share/trax/bin` (**Open yt-dlp folder** in the same card opens it),
   - or have `yt-dlp` on your `PATH` (winget and package managers do this).
3. Click **Save**. The status line shows the yt-dlp version once trax finds it.

Keep yt-dlp up to date yourself (`yt-dlp -U` / `yt-dlp_linux -U`, `winget upgrade yt-dlp` or `pipx upgrade yt-dlp`); old versions stop working when the sites change.

> **This project advises against using trax to stream or download copyrighted material** without the rights holder's permission. Use the online sources only for content you are allowed to access this way, and follow the terms of the services and the copyright law of your country.

## Listening parties

Listen together: one person hosts, everyone else hears the same song at the same moment.

**How to use it**

1. Everyone sets a display name (Party page, or Settings → Sources → Profile & friends). It's shown to everyone in the party.
2. The host opens **Party** in the sidebar, optionally enters a password, and clicks **Create party**. They get a room code like `XYAS-KDHG-ASDF` to send to friends.
3. Guests open **Party**, enter the code (and the password if there is one), and click **Join party**.
4. The host's music is the party's music. Whatever the host plays, skips, pauses or seeks, guests follow, and the host's queue is the party queue.
5. Guests start with no control. In the participant list the host can let each guest skip, seek, pause, add songs or remove songs (or **Grant all**), and can **Kick** someone; kicked guests can't rejoin that party.
6. Everyone sees everyone in the party, the queue, and who added each song. Messages like "Skipped by Alex" show what happened.

**What guests can hear:** only YouTube and SoundCloud songs. Each guest's own trax fetches them, so YouTube / SoundCloud must be switched on there (Settings → Sources). If the host plays a song from their PC or Nextcloud, guests stay in sync but silent, with a note.

**How it works:** there is no trax server. Peers find each other through public [Nostr](https://nostr.com) relays and then connect directly to the host with WebRTC (a DataChannel, control messages only, no audio). The room code is never sent. Relays see a hash of it and messages encrypted with a key from the code and password. The host is the only one who changes anything: guests send requests, the host checks them (permission, current track, rate limits) and broadcasts the result in one order. Guests keep their playback within a few dozen milliseconds of the host, nudging the speed by 2 % for small differences and jumping for bigger ones. Your identity (a certificate in the data folder) stays the same across reconnects, so permissions survive a dropped connection.

**Known limitations**

- **Strict networks:** some networks (strict / symmetric NAT, some company or mobile networks) block direct connections. trax then says it can't connect; a TURN server (Settings → Sources → Listening party) relays the connection in that case.
- **The host is the party:** if the host leaves or loses the connection for more than about two minutes, the party ends. There is no handover to another host.
- **Only online songs are shared**, as above.
- **Who can join:** anyone with the code (and password, if set) can join without any permissions, and the host can kick them. The password can't be changed while the party runs.
- **Relays:** they only see encrypted messages and timing. The relay list can be changed in settings; at least one must be reachable.
- **Low-end mode:** with the effects chain off, small drift is corrected by jumping instead of speed nudges.

## Friends

Friends see what you're listening to and can join your parties without passing codes around. It's off until you switch it on (**Friends** in the sidebar, or Settings → Sources → Profile & friends).

**How to use it**

1. Set a display name and turn on **Friends**. trax creates your profile and shows your **friend code** (`npub1…`); **Copy** it and send it to a friend.
2. Your friend pastes it under **Add a friend** → **Send request**. You get the request under **Requests**: **Accept**, **Decline** or **Block**. Nobody becomes your friend without your yes.
3. Each friend shows their name with a short tag (`Alex · k3f9`), whether they're online, and what they're listening to, e.g. "Listening to *Song* — *Artist*" or "Hosting a party". For YouTube / SoundCloud songs, ▶ plays the same song for you.
4. While you host a party, **Invite** sends a friend the room code and password. They get a banner: **Join** or **Dismiss**.
5. When a friend hosts, **Ask to join** sends them a request. They get a banner with **Let in** or **Decline**; if they let you in, you join straight away.
6. In a party, members who are your friends get a **Friend** badge and their tag.

**What friends see:** your name, whether you're online, the song you're playing (title and artist from any source, paused or not), and whether you're hosting or in a party. In Settings you can turn off **Share what I'm listening to** (friends only see that you're online) or **Appear offline**.

**How it works:** there is no trax server. Your profile is a [Nostr](https://nostr.com) keypair; the friend code is its public key, so profiles are unique and can't be impersonated. Friend requests, invites and join requests are [gift-wrapped](https://github.com/nostr-protocol/nips/blob/master/59.md) messages, encrypted to the recipient, so relays see neither the sender nor the content; relays keep them for a while, so a request reaches a friend who is offline. What you're listening to is one event, encrypted with a key that only your friends get. When you remove or block a friend, trax makes a new key and gives it only to the remaining friends. Invites and join requests are only valid for 10 minutes.

**Good to know**

- **Relays** can see that your profile publishes something and roughly when, but not what or for whom. They are the same relays as for parties (Settings → Sources → Listening party → Relays).
- **Your profile moves with an export** (`.traxbak`), like your other settings; treat the file as private. **Reset profile** makes a new friend code; your friends have to add you again.
- **While friends are on**, trax keeps a connection to the relays open. It also tells party hosts which profile you are, so they can see you're their friend.


## Building

Requirements: Rust (stable) and, on Windows, the MSVC C++ build tools. On Linux, a C compiler, `pkg-config` and the ALSA and fontconfig headers:

```bash
sudo apt install build-essential pkg-config libasound2-dev libfontconfig1-dev   # Debian, Ubuntu
sudo dnf install gcc pkgconf-pkg-config alsa-lib-devel fontconfig-devel         # Fedora
```

```bash
cargo build --release
```

The binary is `target/release/trax.exe` on Windows and `target/release/trax` on Linux. X11, Wayland, OpenGL and xkbcommon are loaded at runtime, so they aren't needed to build.

## Installer, portable version and updates

Every [release](https://github.com/NUCL3ARN30N/trax/releases) has four downloads:

- **`trax-setup-<version>.exe`**: Windows installer for the current user (no admin rights), into `%LOCALAPPDATA%\Programs\trax`, with a Start menu entry and an uninstaller. Uninstalling keeps your library and settings.
- **`trax-<version>-portable.zip`**: just `trax.exe`; run it from any folder.
- **`trax_<version>-1_amd64.deb`**: package for Debian 12, Ubuntu 22.04 and newer, and distributions based on them (Linux Mint, Pop!_OS, elementary OS, Zorin OS, …). Install it with `sudo apt install ./trax_<version>-1_amd64.deb` (or double-click it); trax then appears in your app launcher and in "Open with" for audio files. Updates download the new package and install it after asking for your password. Remove it with `sudo apt remove trax`; your library and settings stay.
- **`trax-<version>-linux-x86_64.tar.gz`**: the Linux `trax` binary for other distributions, for any distribution with glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36 and later). Unpack it anywhere you can write to, e.g. `~/.local/opt/trax`, and run `trax`. **Settings → System & updates → Make trax the default player** adds it to your app launcher. It needs ALSA (`libasound2`) and fontconfig, which every desktop has; sound goes through PipeWire or PulseAudio as usual. Updates replace the binary in place, like the portable version.

## Setting up Spotify (artist info & artwork)

trax uses the Spotify Web API only to look up artist images, genres, followers and album artwork. It needs a free Spotify developer app; you don't log in to Spotify inside trax. The same steps are shown in **Settings → Artist info & artwork (Spotify)**.

1. Open the [Spotify Developer Dashboard](https://developer.spotify.com/dashboard) and log in with your Spotify account. Accept the developer terms if asked.
2. Click **Create app**.
3. **App name**: `trax`. **App description**: anything, e.g. "Personal music player".
4. **Redirect URI**: enter `http://127.0.0.1:8888/callback` and click **Add**. trax never uses it, but Spotify requires one.
5. Under **Which API/SDKs are you planning to use?** tick **Web API**.
6. Accept the terms and click **Save**.
7. In the new app, open **Settings**: copy the **Client ID**, then click **View client secret** and copy the secret.
8. In trax: **Settings → Artist info & artwork (Spotify)** → switch it on, paste both values, click **Save**.

Then use **Fill missing artwork** / **Update all artists**, or open any artist page. The app can stay in Spotify's development mode. If the dashboard won't let you create an app, check Spotify's current developer requirements there.

## Where data lives

| What | Windows | Linux |
|---|---|---|
| Database, settings | `%APPDATA%\trax\data\trax.db` | `~/.local/share/trax/trax.db` |
| Covers / artist artwork | `%APPDATA%\trax\data\covers`, `…\artwork` | `~/.local/share/trax/covers`, `…/artwork` |
| Streaming cache (safe to delete) | `%APPDATA%\trax\data\cache` | `~/.local/share/trax/cache` |
| yt-dlp (if you put it there) | `%APPDATA%\trax\data\bin` | `~/.local/share/trax/bin` |
| Profile and friends | in the database (`trax.db`) | in the database (`trax.db`) |

On Linux `$XDG_DATA_HOME` replaces `~/.local/share` when it is set.

Credentials (Nextcloud app password, Spotify client secret) and your profile key are stored in the local database so they move with an export. Treat `.traxbak` files as private.

## Project layout

```
src/
  main.rs          entry point
  app.rs           controller: wires UI ⇄ player ⇄ database
  player.rs        audio output (rodio) and the queue
  eq.rs            10-band biquad equalizer source
  fx.rs            effects: tempo/pitch, reverb, echo, filters, … and the visualizer tap
  app_sound.rs     effects UI, visualizer, favourites, song sections, cache limit
  app_theme.rs     palettes, JSON themes, appearance settings
  db.rs            SQLite schema, queries, auto playlists, stats
  backup.rs        export / import
  tray.rs          system tray icon + menu
  media_keys.rs    OS media controls (souvlaki)
  ui_models.rs     lazy Slint models for track lists
  app_artists.rs   artist profiles and online metadata (Spotify, Wikipedia)
  updater.rs       update check, download + checksum, installer / portable / Linux swap
  app_update.rs    update UI (banner, Settings → System & updates)
  app_files.rs     songs opened from anywhere (single-file library entries)
  instance.rs      single instance: later starts hand their files to the running trax
  system.rs        default music player: Windows registry, Linux desktop entry + mimeapps.list
  app_system.rs    default player settings
  share.rs         playlist share codes (encode / decode)
  party/           listening parties: protocol.rs (the whole wire protocol), code.rs (room codes, keys),
                   identity.rs, signaling.rs (Nostr), transport.rs (WebRTC), host.rs / guest.rs (state
                   machines), clock.rs (clock sync, drift)
  app_party.rs     party ⇄ player integration and the party page
  net/             relays.rs: the Nostr relay pool shared by parties and friends
  social/          friends: profile.rs (keys, friend codes), messages.rs (gift wraps), presence.rs
                   (what you're listening to), friends.rs (friend list state machine), service.rs
  app_social.rs    friends ⇄ app integration, presence publishing, invites and the Friends page
about.toml, about.hbs  cargo-about configuration for THIRD_PARTY_LICENSES.txt
  app_share.rs     share and import dialogs
  connectors/      local, nextcloud, youtube, soundcloud, spotify, wikipedia
ui/                Slint UI (Material 3 theme, pages, player bar, tray popup)
installer/         Inno Setup script for the Windows installer
.github/workflows/ release build (installer, portable zip, .deb, Linux tarball, checksums)
assets/linux/      desktop entry and Debian copyright file for the .deb
assets/logo/       trax logo: trax_wordmark.png (full logo with name), AppIcon.iconset (X mark, PNG 16–1024 px) and trax.ico (exe, window, taskbar, tray)
```

## Theme files

Settings → Appearance → **Export current look…** writes a JSON file you can edit and share; **Import theme…** loads one. Every field is optional:

```json
{
  "name": "Ocean",
  "mode": "dark",
  "accent": "#0061a4",
  "corners": 3,
  "font": "Segoe UI",
  "colors": {
    "dark":  { "primary": "#9ecaff", "surface": "#0f1418" },
    "light": { "primary": "#0061a4" }
  }
}
```

`mode` is `system`, `light` or `dark`; `corners` is 0 (square) to 3 (round). Fields left at their default (`system` mode, corners 2, empty font) keep your own setting. Other appearance settings (density, size, columns, sidebar) are never touched by a theme, and choosing **Default** brings back the look you had before picking a theme. The palette is generated from `accent`, then anything in `colors` overrides it. Colour names are the Material 3 roles: surface, surface-dim, surface-container-lowest/low/(none)/high/highest, on-surface, on-surface-variant, outline, outline-variant, primary, on-primary, primary-container, on-primary-container, secondary-container, on-secondary-container, tertiary, tertiary-container, on-tertiary-container, error, inverse-surface, inverse-on-surface.

## License

Copyright (C) 2026 NUCL3ARN30N

trax is free software: you can redistribute it and/or modify it under the terms of the [GNU General Public License v3.0](LICENSE) (GPL-3.0-only). It is distributed in the hope that it will be useful, but **without any warranty**; see the license for details.

trax builds on many open-source projects. [THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt) lists every component compiled into trax.exe with its license; it is generated with [cargo-about](https://github.com/EmbarkStudios/cargo-about) (`cargo about generate --output-file THIRD_PARTY_LICENSES.txt about.hbs`, configuration in `about.toml`) and shipped with every release. Notable parts: the [Slint](https://slint.dev) UI toolkit (used under GPL-3.0), [Symphonia](https://github.com/pdeljanov/Symphonia) audio decoding (MPL-2.0) and Google's [Material Icons](https://github.com/google/material-design-icons) (Apache-2.0). [yt-dlp](https://github.com/yt-dlp/yt-dlp) is not part of trax and isn't downloaded by it; it is a separate program you install yourself if you want the online sources.

## Disclaimer

trax is an independent project. It is not affiliated with, endorsed by or sponsored by YouTube / Google, SoundCloud, Spotify, Nextcloud, Wikipedia or any other service it can connect to. All product names, logos and trademarks belong to their respective owners and are used only to describe what trax works with.

Online sources are off until you switch them on. This project advises against using them to stream or download copyrighted material without permission. When you use them, you are responsible for following the terms of those services and the copyright law of your country. For the optional Spotify integration you create your own Spotify developer app, and its use is subject to the [Spotify Developer Terms](https://developer.spotify.com/terms). Artist bios come from Wikipedia and are shown with attribution under [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/). trax is provided as is, without warranty.
