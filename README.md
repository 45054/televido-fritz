<!-- SPDX-FileCopyrightText: David Cabot <d-k-bo@mailbox.org> -->
<!-- SPDX-FileCopyrightText: 2026 Televido contributors -->
<!-- SPDX-FileContributor: Claude (Anthropic) -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

[Deutsche Version](README.de.md)

# Televido

> [!NOTE]
> ## This is a fork
>
> This repository continues [d-k-bo/televido](https://github.com/d-k-bo/televido), which its original author has discontinued (see his notice below). The fork adds [FritzTV](#fritztv-fritzbox-cable-dvb-c), which plays and records the DVB-C channels of a FRITZ!Box Cable, plays all videos with mpv and contains further changes. Its Arch Linux package is called `televido-fritz-git` and its app ID is `io.github._45054.TelevidoFritz` (earlier versions used upstream's `de.k_bo.Televido`; their settings are taken over on the first start).
>
> All changes in this fork since October 2026 were written by Claude, an AI assistant by Anthropic, according to the requirements of the fork's maintainer. Their copyright notices name “Televido contributors”, and the files name Claude as contributor (`SPDX-FileContributor`). The copyright of the original code remains with David Cabot. Everything stays licensed under the GPL-3.0-or-later.

> [!WARNING]
> ## This project is no longer maintained
>
> ### Why?
>
> As a hobbyist programmer, I have invested a significant amount of my personal time and energy into many projects. Due to increasing personal and professional commitments, I have significant less time to dedicate to software development. I am therefore focusing on the projects that I actively use myself and enjoy working on the most. Recent developments in "AI" have also contributed to a general decline in my interest in software development.
>
> ### What does this mean?
>
> - This project will not be actively maintained for the foreseeable future. Accordingly, it should be considered as deprecated.
> - The project remains available under the terms of the corresponding free software license.
> - You are welcome to redistribute, fork and continue its development under the terms of the said ‍‍‍license.


**Televido** (“Television” in Esperanto) lets you livestream, search, play and download media from German-language public television services. It is powered by [MediathekViewWeb](https://mediathekviewweb.de/)'s API and the [Zapp backend](https://github.com/mediathekview/zapp-backend) API which are both part of the [MediathekView](https://mediathekview.de/) project.

The presented content is provided directly by the respective television services, this program only facilitates finding and accessing the shows.

Televido plays videos with [mpv](https://mpv.io/), which must be installed. Additionally, Televido supports external programs that are installed on the user's system for video playback and download.  Currently supported players: [GNOME Videos (Totem)](https://flathub.org/apps/org.gnome.Totem), [Celluloid](https://flathub.org/apps/io.github.celluloid_player.Celluloid), [Clapper](https://flathub.org/apps/com.github.rafostar.Clapper), [Daikhan](https://flathub.org/apps/io.gitlab.daikhan.stable). Currently supported downloaders: [Parabolic](https://flathub.org/apps/org.nickvision.tubeconverter).

## FritzTV (FRITZ!Box Cable DVB-C)

Televido can play and record the DVB-C channels of a FRITZ!Box Cable (e.g. 6490, 6591, 6660, 6690) in your home network. Enable it in the preferences under *FritzTV*; a new *FritzTV* tab then lists the TV and radio channels of your FRITZ!Box. By default, every TV channel is listed once in the best available quality (`tv.m3u`); alternatively, the HD and SD channels can be listed separately. Radio channels can be hidden in the preferences.

- **Requirements:** the FRITZ!Box must have completed a channel scan (DVB-C settings in the FRITZ!Box web interface). Encrypted channels can't be played or recorded.
- **Address:** `fritz.box` is used by default. If that doesn't resolve in your network, enter the IP address of your FRITZ!Box (e.g. `192.168.178.1`).
- **Receiving the stream:** Televido receives the stream of the FRITZ!Box itself and passes it to mpv through a pipe. This allows recording and keeps the programme guide current while you watch. If a channel doesn't play this way, turn off *Receive the Stream in Televido* in the preferences: mpv then opens the RTSP stream directly, as in earlier versions, without recording and without the programme guide while watching.
- **mpv arguments:** the arguments passed to mpv can be edited in the preferences: general arguments for all videos (*Playback*) and additional ones for FRITZ!Box channels (*FritzTV*). The FritzTV defaults are tuned for the FRITZ!Box: longer stream analysis and an initial buffer avoid stutter, and `--demuxer-max-back-bytes=256MiB` keeps the last part of the stream in mpv's cache (about two minutes of an HD channel), so you can pause and seek back. While mpv is paused, Televido goes on receiving; once mpv's cache is full (`--demuxer-max-bytes`), the part in between is skipped. When mpv opens the stream directly, Televido adds `--rtsp-transport=udp` (the FRITZ!Box doesn't support RTSP over TCP) and a receive buffer of 4 MiB (`--demuxer-lavf-o-append=buffer_size=4194304`), unless the arguments already set them. The FritzTV arguments are not used for live channels and the Mediathek, which therefore start without delay.
- **One stream per device:** the FRITZ!Box only delivers one channel per device, so starting a channel stops the previous one. Watching the channel that is being recorded uses the same stream.
- **Programme guide:** the cards show the current and the next show of the TV channels (expand a card for its description), like the livestreams. The data comes from the cable signal itself (the DVB Event Information Table, no external service involved). The FRITZ!Box only passes on the programme data of the frequency it is tuned to. While a channel plays or is recorded, Televido reads the programme data of its frequency from the stream. While no channel is playing, Televido tunes each frequency for a few seconds in turn and remembers the result, starting with the frequencies whose channels don't show a programme yet. Many cable networks announce the programme of the other frequencies as well, which keeps the data current without visiting every frequency again. The first run takes a minute or two. Playing or recording a channel interrupts this at once, and it continues a few seconds after the playback or recording ended. With *Receive the Stream in Televido* turned off, the programme guide isn't updated while a channel plays. It can be turned off in the preferences (*Show Program Guide*).
- **Recording:** the record button on a FritzTV card records the channel, with or without watching it. While it records, the button turns into a stop button and the card shows *Recording*.
  - **Files:** recordings are saved in the folder `Televido` in your Videos folder (usually `~/Videos/Televido`); another folder can be chosen in the preferences (*Recordings Folder*). A file is named after the channel, the start time and the current show, e.g. `Das Erste HD 2026-11-04 20-15 Tagesschau.ts`. It is a plain MPEG transport stream (`.ts`) as sent by the FRITZ!Box, which mpv or VLC play directly. When a recording ends, a notification shows the file size, and its *Show in Folder* button opens the folder.
  - **End:** if the programme guide knows the current show, the recording ends 5 minutes after the show's end; otherwise it runs until you stop it.
  - **Background:** a recording continues when mpv is closed. Televido records only while it is running: closing or quitting it ends the recording after a confirmation, and the file is kept up to that point.
  - **One channel at a time:** as the FRITZ!Box delivers one channel per device, watching or recording another channel ends a running recording after a confirmation, and recording a channel ends the playback of another one after a confirmation.
  - Recording requires *Receive the Stream in Televido*; while it is turned off, the record button is hidden.
- **Filter:** the field above the channels filters them by name while typing, like on the *Live* page. Typing while a channel is selected goes to the filter as well, <kbd>Ctrl</kbd>+<kbd>F</kbd> focuses it and <kbd>Esc</kbd> clears it.
- **Channel logos** are downloaded from AVM and cached: from `https://tv.avm.de/tvapp/logos/` like in the FRITZ!Box web interface, otherwise from `https://download.avm.de/tv/logos/`.
- **Stuttering:** the FRITZ!Box sends the streams via UDP, so firewalls must allow incoming UDP traffic from the FRITZ!Box. If the picture stutters, the kernel's maximum socket receive buffer may be too small for the 4 MiB that Televido asks for when it receives the stream, and passes to mpv as `buffer_size` when mpv opens the stream directly (*Receive the Stream in Televido* turned off):
  ```
  echo 'net.core.rmem_max=4194304' | sudo tee /etc/sysctl.d/90-televido-rtp.conf && sudo sysctl --system
  ```

FritzTV starts mpv as a regular process and is therefore intended for native installations (see [Building on Arch Linux](#building-on-arch-linux)). The development Flatpak ([`build-aux/io.github._45054.TelevidoFritz.Devel.json`](build-aux/io.github._45054.TelevidoFritz.Devel.json)) bundles mpv as well and has access to the Videos folder for the recordings (`--filesystem=xdg-videos:create`).

## Building on Arch Linux

[`build-aux/arch/PKGBUILD`](build-aux/arch/PKGBUILD) builds a `televido-fritz-git` package from this repository (it replaces an installed `televido` or `televido-git`), including FritzTV and its mpv dependency:

```
cd build-aux/arch
makepkg -si
```

By default, the `main` branch is fetched from GitHub. To build another branch or the local checkout (committed changes only):

```
TELEVIDO_BRANCH=my-branch makepkg -si
TELEVIDO_SOURCE="file://$(git rev-parse --show-toplevel)" TELEVIDO_BRANCH="$(git branch --show-current)" makepkg -si
```

When switching `TELEVIDO_SOURCE`, delete the cached clone `build-aux/arch/televido-fritz/` first. Note that makepkg updates the `pkgver` line in the PKGBUILD on every build.

## Channel logos

The ARD, ORF and SRF logos were taken from [Wikimedia Commons](https://commons.wikimedia.org) and are in the public domain.

The other channel logos were extracted from the source code of [zapp](https://github.com/mediathekview/zapp) and converted to SVG using [`vd2svg`](https://github.com/seanghay/vector-drawable-svg).

## FAQ

### How can I use a different video player / use a player with custom options?

Televido supports any video player that is [DBus activatable](https://specifications.freedesktop.org/desktop-entry-spec/latest/ar01s08.html) and supports opening https:// URIs via the `org.freedesktop.Application.Open` DBus method.

To use a custom player, create a flatpak permission override to allow it to access the player. E.g.

```
flatpak override --user io.github._45054.TelevidoFritz --talk-name=org.example.VideoPlayer
```

and set the video player in the preferences.

If you want to use program that doesn't support DBus activation, you can create a wrapper script. See [d-k-bo/dbus-activatable-wrapper](https://github.com/d-k-bo/dbus-activatable-wrapper).

### Could you add support for TV channels from other countries?

Since this project is basically a client for [MediathekView](https://mediathekviewweb.de/), it's limited to the channels supported by them. The upstream project is focused on German content and is developed in German, so I doubt that there are any plans for non-German content.
ORF (Austrian TV) & SRF (Swiss TV) are supported though.

## License

Copyright (C) 2023 David Cabot

This program is free software; you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation; either version 3 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
