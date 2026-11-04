<!-- SPDX-FileCopyrightText: David Cabot <d-k-bo@mailbox.org> -->
<!-- SPDX-FileCopyrightText: 2026 Televido contributors -->
<!-- SPDX-FileContributor: Claude (Anthropic) -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

[English Version](README.md)

# Televido

> [!NOTE]
> ## Dies ist ein Fork
>
> Dieses Repository führt [d-k-bo/televido](https://github.com/d-k-bo/televido) fort, das der ursprüngliche Autor eingestellt hat (siehe seinen Hinweis unten). Der Fork ergänzt [FritzTV](#fritztv-fritzbox-cable-dvb-c), das die DVB-C-Sender einer FRITZ!Box Cable abspielt und aufnimmt, spielt alle Videos mit mpv ab und enthält weitere Änderungen. Das Arch-Linux-Paket heißt `televido-fritz-git` und die App-ID `io.github._45054.TelevidoFritz` (frühere Versionen nutzten die ID `de.k_bo.Televido` des Originals; ihre Einstellungen werden beim ersten Start übernommen).
>
> Alle Änderungen in diesem Fork seit Oktober 2026 hat Claude geschrieben, ein KI-Assistent von Anthropic, nach den Anforderungen des Fork-Maintainers. Ihre Copyright-Vermerke nennen „Televido contributors“, und die Dateien nennen Claude als Mitwirkenden (`SPDX-FileContributor`). Das Copyright am ursprünglichen Code liegt weiterhin bei David Cabot. Alles steht weiterhin unter der GPL-3.0-or-later.

> [!WARNING]
> ## Dieses Projekt ist eingestellt
>
> ### Warum?
>
> Als Hobby-Programmierer habe ich eine beträchtliche Menge an persönlicher Zeit und Energie in viele Projekte investiert. Aufgrund zunehmender privater und beruflicher Verpflichtungen habe ich jedoch deutlich weniger Zeit, die ich in die Softwareentwicklung stecken kann. Daher möchte ich mich lieber auf die Projekte konzentrieren, die ich selbst aktiv nutze und bei denen mir die Arbeit am meisten Spaß macht. Darüber hinaus verspüre ich insgesamt durch den aktuellen Einfluss von „KI“ einen leichten Interessensverlust an Softwareentwicklung.
>
> ### Was bedeutet das?
>
> - Dieses Projekt wird auf absehbare Zeit nicht mehr gepflegt und sollte dementsprechend als veraltet (deprecated) betrachtet werden.
> - Das Projekt bleibt weiterhin gemäß den Bedingungen der entsprechenden Frei-Software-Lizenz verfügbar.
> - Gerne kann es unter den Bedingungen der genannten Lizenz weiterverbreitet, geforkt und weiterentwickelt werden.


**Televido** („Fernsehen“ auf Esperanto) ermöglicht das Empfangen von Livestreams sowie das Suchen, Abspielen und Herunterladen von Inhalten aus Mediatheken öffentlich-rechtlicher Sender aus dem DACH-Raum. Es basiert auf den APIs von [MediathekViewWeb](https://mediathekviewweb.de/) und des [Zapp-Backends](https://github.com/mediathekview/zapp-backend), die beide Teil des [MediathekView](https://mediathekview.de/)-Projekts sind.

Die präsentierten Inhalte werden direkt von den jeweiligen Sendern angeboten, dieses Programm erleichtert dabei nur das Auffinden und Abrufen der Sendungen.

Televido spielt Videos mit [mpv](https://mpv.io/) ab, das installiert sein muss. Darüber hinaus unterstützt Televido für Videowiedergabe und -download externe Programme, die auf dem System installiert sind. Derzeit unterstützte Player: [GNOME Videos (Totem)](https://flathub.org/apps/org.gnome.Totem), [Celluloid](https://flathub.org/apps/io.github.celluloid_player.Celluloid), [Clapper](https://flathub.org/apps/com.github.rafostar.Clapper), [Daikhan](https://flathub.org/apps/io.gitlab.daikhan.stable). Derzeit unterstützte Videodownloader: [Parabolic](https://flathub.org/apps/org.nickvision.tubeconverter).

## FritzTV (FRITZ!Box Cable DVB-C)

Televido kann die DVB-C-Sender einer FRITZ!Box Cable (z. B. 6490, 6591, 6660, 6690) im Heimnetz abspielen und aufnehmen. Die Funktion wird in den Einstellungen unter *FritzTV* aktiviert; anschließend listet der neue Reiter *FritzTV* die TV- und Radiosender der FRITZ!Box auf. Standardmäßig erscheint jeder TV-Sender einmal in der besten verfügbaren Qualität (`tv.m3u`); wahlweise lassen sich HD- und SD-Sender getrennt anzeigen. Die Radiosender lassen sich in den Einstellungen ausblenden.

- **Voraussetzungen:** Die FRITZ!Box muss einen Sendersuchlauf abgeschlossen haben (DVB-C-Einstellungen in der Benutzeroberfläche der FRITZ!Box). Verschlüsselte Sender können weder abgespielt noch aufgenommen werden.
- **Adresse:** Standardmäßig wird `fritz.box` verwendet. Falls das im eigenen Netz nicht aufgelöst wird, die IP-Adresse der FRITZ!Box eintragen (z. B. `192.168.178.1`).
- **Empfang des Streams:** Televido empfängt den Stream der FRITZ!Box selbst und gibt ihn über eine Pipe an mpv weiter. Das ermöglicht Aufnahmen und hält die Programmvorschau während der Wiedergabe aktuell. Spielt ein Sender auf diese Weise nicht, lässt sich in den Einstellungen *Stream in Televido empfangen* ausschalten: Dann öffnet mpv den RTSP-Stream wie in früheren Versionen direkt, ohne Aufnahme und ohne Programmvorschau während der Wiedergabe.
- **mpv-Argumente:** Die an mpv übergebenen Argumente lassen sich in den Einstellungen bearbeiten: allgemeine Argumente für alle Videos (*Wiedergabe*) und zusätzliche für FRITZ!Box-Sender (*FritzTV*). Die FritzTV-Voreinstellungen sind auf die FRITZ!Box abgestimmt: Längere Stream-Analyse und ein Anfangspuffer vermeiden Ruckler, und `--demuxer-max-back-bytes=256MiB` hält den zuletzt gesehenen Teil des Streams im Zwischenspeicher von mpv (etwa zwei Minuten eines HD-Senders), sodass sich anhalten und zurückspulen lässt. Während mpv angehalten ist, empfängt Televido weiter; sobald der Zwischenspeicher von mpv voll ist (`--demuxer-max-bytes`), wird der Teil dazwischen übersprungen. Öffnet mpv den Stream direkt, ergänzt Televido `--rtsp-transport=udp` (die FRITZ!Box unterstützt kein RTSP über TCP) und einen Empfangspuffer von 4 MiB (`--demuxer-lavf-o-append=buffer_size=4194304`), sofern die Argumente sie nicht schon festlegen. Für Live-Sender und die Mediathek werden die FritzTV-Argumente nicht verwendet, diese starten daher ohne Verzögerung.
- **Ein Stream pro Gerät:** Die FRITZ!Box liefert nur einen Sender pro Gerät, deshalb beendet der Start eines Senders den vorherigen. Der Sender, der gerade aufgenommen wird, lässt sich über denselben Stream ansehen.
- **Programmvorschau:** Die Karten zeigen wie bei den Livestreams die aktuelle und die nächste Sendung der TV-Sender (die Beschreibung erscheint beim Aufklappen einer Karte). Die Daten stammen aus dem Kabelsignal selbst (DVB Event Information Table, kein externer Dienst). Die FRITZ!Box gibt die Programmdaten nur für die Frequenz weiter, auf die sie gerade abgestimmt ist. Während ein Sender läuft oder aufgenommen wird, liest Televido die Programmdaten seiner Frequenz aus dem Stream. Solange kein Sender läuft, stimmt Televido nacheinander jede Frequenz für einige Sekunden ab und merkt sich das Ergebnis; Frequenzen, deren Sender noch keine Sendung anzeigen, kommen zuerst an die Reihe. Viele Kabelnetze senden außerdem das Programm der anderen Frequenzen mit, sodass nicht jede Frequenz erneut besucht werden muss. Der erste Durchlauf dauert ein bis zwei Minuten. Das Abspielen oder Aufnehmen eines Senders unterbricht das sofort, nach dem Ende der Wiedergabe oder Aufnahme geht es nach wenigen Sekunden weiter. Ist *Stream in Televido empfangen* ausgeschaltet, wird die Programmvorschau während der Wiedergabe nicht aktualisiert. In den Einstellungen lässt sich die Funktion abschalten (*Programmvorschau anzeigen*).
- **Aufnahme:** Der Aufnahmeknopf auf einer FritzTV-Karte nimmt den Sender auf, ob er dabei läuft oder nicht. Während der Aufnahme wird der Knopf zum Stoppknopf, und die Karte zeigt *Aufnahme*.
  - **Dateien:** Aufnahmen landen im Ordner `Televido` im Videos-Ordner (meist `~/Videos/Televido`); in den Einstellungen lässt sich ein anderer Ordner wählen (*Aufnahmeordner*). Eine Datei ist nach Sender, Startzeit und laufender Sendung benannt, z. B. `Das Erste HD 2026-11-04 20-15 Tagesschau.ts`. Sie ist ein einfacher MPEG-Transportstrom (`.ts`), wie ihn die FRITZ!Box sendet, und lässt sich direkt mit mpv oder VLC abspielen. Endet eine Aufnahme, nennt eine Benachrichtigung die Dateigröße, und ihr Knopf *Im Ordner anzeigen* öffnet den Ordner.
  - **Ende:** Kennt die Programmvorschau die laufende Sendung, endet die Aufnahme 5 Minuten nach dem Ende der Sendung; andernfalls läuft sie, bis sie gestoppt wird.
  - **Hintergrund:** Eine Aufnahme läuft weiter, wenn mpv geschlossen wird. Televido nimmt nur auf, solange es läuft: Schließen oder Beenden beendet die Aufnahme nach einer Rückfrage, die Datei bleibt bis zu diesem Zeitpunkt erhalten.
  - **Ein Sender gleichzeitig:** Da die FRITZ!Box nur einen Sender pro Gerät liefert, beendet das Ansehen oder Aufnehmen eines anderen Senders eine laufende Aufnahme nach einer Rückfrage, und die Aufnahme eines Senders beendet nach einer Rückfrage die Wiedergabe eines anderen.
  - Aufnehmen setzt *Stream in Televido empfangen* voraus; ist die Einstellung ausgeschaltet, ist der Aufnahmeknopf ausgeblendet.
- **Filter:** Das Feld über den Sendern filtert sie schon beim Tippen nach Namen, wie auf der Seite *Live*. Tippen bei ausgewähltem Sender landet ebenfalls im Filter, <kbd>Strg</kbd>+<kbd>F</kbd> setzt den Fokus hinein und <kbd>Esc</kbd> leert ihn.
- **Senderlogos** werden von AVM heruntergeladen und zwischengespeichert: von `https://tv.avm.de/tvapp/logos/` wie in der Benutzeroberfläche der FRITZ!Box, ersatzweise von `https://download.avm.de/tv/logos/`.
- **Ruckeln:** Die FRITZ!Box sendet die Streams per UDP, Firewalls müssen daher eingehenden UDP-Verkehr von der FRITZ!Box zulassen. Ruckelt das Bild, ist eventuell der maximale Empfangspuffer des Kernels zu klein für die 4 MiB, die Televido anfordert, wenn es den Stream empfängt, und als `buffer_size` an mpv übergibt, wenn mpv den Stream direkt öffnet (*Stream in Televido empfangen* ausgeschaltet):
  ```
  echo 'net.core.rmem_max=4194304' | sudo tee /etc/sysctl.d/90-televido-rtp.conf && sudo sysctl --system
  ```

FritzTV startet mpv als normalen Prozess und ist daher für native Installationen gedacht (siehe [Bauen unter Arch Linux](#bauen-unter-arch-linux)). Das Entwicklungs-Flatpak ([`build-aux/io.github._45054.TelevidoFritz.Devel.json`](build-aux/io.github._45054.TelevidoFritz.Devel.json)) enthält mpv ebenfalls und hat für die Aufnahmen Zugriff auf den Videos-Ordner (`--filesystem=xdg-videos:create`).

## Bauen unter Arch Linux

[`build-aux/arch/PKGBUILD`](build-aux/arch/PKGBUILD) baut aus diesem Repository ein Paket `televido-fritz-git` (es ersetzt ein installiertes `televido` oder `televido-git`), inklusive FritzTV und der Abhängigkeit mpv:

```
cd build-aux/arch
makepkg -si
```

Standardmäßig wird der Branch `main` von GitHub geholt. Um einen anderen Branch oder den lokalen Checkout (nur committete Änderungen) zu bauen:

```
TELEVIDO_BRANCH=mein-branch makepkg -si
TELEVIDO_SOURCE="file://$(git rev-parse --show-toplevel)" TELEVIDO_BRANCH="$(git branch --show-current)" makepkg -si
```

Beim Wechsel von `TELEVIDO_SOURCE` vorher den zwischengespeicherten Klon `build-aux/arch/televido-fritz/` löschen. makepkg aktualisiert bei jedem Bau die Zeile `pkgver` im PKGBUILD.

## Senderlogos

Die ARD-, ORF- and SRF-Logos wurden [Wikimedia Commons](https://commons.wikimedia.org) entnommen und sind gemeinfrei.

Die anderen Logos wurden aus dem Quellcode von [zapp](https://github.com/mediathekview/zapp) extrahiert und mit [`vd2svg`](https://github.com/seanghay/vector-drawable-svg) ins SVG-Format konvertiert.

## FAQ

Siehe [README.md#faq](README.md#faq) (Englisch)

## Lizenz

Copyright (C) 2023 David Cabot

Dieses Programm ist freie Software. Sie können es unter den Bedingungen der GNU General Public License, wie von der Free Software Foundation veröffentlicht, weitergeben und/oder modifizieren, entweder gemäß Version 3 der Lizenz oder (nach Ihrer Option) jeder späteren Version.

Die Veröffentlichung dieses Programms erfolgt in der Hoffnung, daß es Ihnen von Nutzen sein wird, aber OHNE IRGENDEINE GARANTIE, sogar ohne die implizite Garantie der MARKTREIFE oder der VERWENDBARKEIT FÜR EINEN BESTIMMTEN ZWECK. Details finden Sie in der GNU General Public License.
