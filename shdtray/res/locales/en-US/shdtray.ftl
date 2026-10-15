### The tray's text, and the notices shieldd sends to it. US English, and the fallback for
### anything another language leaves out.
###
### An & marks the menu shortcut letter, && a literal one. Menu labels are sentence case. Each
### paragraph of a message box is one line; Windows wraps it.

## Menu

menu-title = shieldd (v{ $version })
# After " > "
menu-connected = { $count } connected.
# After " > "
menu-network = { $ssid }, ch.{ $channel }
menu-none-connected = No controllers connected
menu-pair = &Pair controller...
menu-reconfigure = Re&configure network
menu-exit = E&xit

## A controller's line and submenu

# For one with no name on record
controller-unnamed = Controller
# $name carries its shortcut. $port is "?" until Windows assigns one.
controller-line = { $state ->
        [charged] { $name }: Port #{ $port } (charged)
        [charging] { $name }: Port #{ $port } (charging, { $pct }%)
        [level] { $name }: Port #{ $port } ({ $pct }%)
       *[unknown] { $name }: Port #{ $port }
    }
controller-mac = MAC { $mac }
controller-firmware = Firmware v{ $version }
controller-identify = &Identify
controller-rename = &Rename...
controller-audio-on = Enable &audio
controller-audio-off = Disable &audio
controller-disconnect = &Disconnect
controller-forget = &Forget

## Icon tooltip

# One for each connected controller: its menu line without the port, to fit four
tip-line = { $state ->
        [charged] { $name } (charged)
        [charging] { $name } (charging, { $pct }%)
        [level] { $name } ({ $pct }%)
       *[unknown] { $name }
    }
tip-pairing = SHIELD controller: pairing
tip-joining = SHIELD controller: connecting...
tip-disconnected = SHIELD controller: not connected

## Battery alerts

alert-charging-title = { $name } charging
alert-charging = Plugged in at { $pct }%.
alert-charged-title = { $name } charged
alert-charged = Full; ready to unplug.
alert-low-title = { $name } battery low
alert-low = { $pct }% left.
alert-critical-title = { $name } battery critical
alert-critical = { $pct }% left. Rumble is off; charge it now.
alert-empty-title = { $name } battery empty
alert-empty = Disconnected; plug it in to use it.

## Dialogs

vban-on-title = shieldd: VBAN on
vban-on =
    Controller audio over VBAN, for Voicemeeter:

    Mic: stream { $mic_stream }, sent to 127.0.0.1 port { $mic_port }
    Headset: stream { $pad_stream }, received on port { $pad_port }

    Point an incoming stream at the first and an outgoing one at the second, PCM 16-bit. Every controller with audio on shares them: their mics are mixed, and each headset plays the same audio.

pipewire-on-title = shieldd: audio on
pipewire-on =
    Controller audio is in your sound settings:

    Mic: { $mic }
    Headset: { $headset }

    Every controller with audio on shares them: their mics are mixed, and each headset plays the same audio.

# The rename dialog is a fixed 220 dialog units wide; the prompt gets one line
rename-title = shieldd: rename controller
rename-prompt = Name for { $mac }, up to { $max } { $max ->
        [one] character
       *[other] characters
    }:
dialog-ok = OK
dialog-cancel = Cancel

pair-title = shieldd: pair controller
pair =
    Hold the NVIDIA button on the controller until its light flashes.

    Pairing stays open for 2 minutes, or until one controller pairs; pair again for each more. Controllers already connected stay connected.

forget-title = shieldd: forget controller
forget =
    This erases the connection details from { $name }'s memory. It will need to be paired again to work with the network.

    Forget it?

reconfigure-title = shieldd: reconfigure network
reconfigure =
    The network will be made over with a new name and passphrase. All paired controllers will be disconnected, and each will need to be paired again.

    Pairing opens right after, for the first of them.

    Reconfigure the network?

about-title = About shieldd
about =
    shieldd v{ $version }
    Built { $built }
    Copyright © 2026 { $author }

    Native Windows and Linux host for the 2015 NVIDIA SHIELD controller.

    shieldd comes with ABSOLUTELY NO WARRANTY. It is free software, and you are welcome to redistribute it under the terms of the GNU General Public License, version 2 only. In accordance with section 3 of the license, its complete source code is available at { $source }.

    Includes code derived from rtw88, © 2018-2024 Realtek Corporation; from Realtek's RTL8812AU/8821AU driver and its halmac layer, © Realtek Corporation; and from ozwpan, © 2011 Ozmo Inc., all under GPL-2.0.

    Embeds Realtek's RTL8821A firmware, © 2010 Realtek Semiconductor Corporation, unmodified under its own redistribution license, provided AS IS without warranty.

    Tray icons from Ikono Bold Line Icons, © Will Kelly; Clarity, © VMware, Inc.; and Phosphor, © Phosphor Icons, all MIT. Rust crates under MIT, Apache-2.0, BSD and similar licenses.

    Full license texts and copyright notices are in LICENSE and THIRD_PARTY.md, which come with the source.

    Most of shieldd was written by an AI model, Anthropic's Claude, under its maintainer's direction and testing.

## Notices from shieldd. $error is an error message, in English.

problem-audio-title = Controller audio failed
problem-rename-title = Couldn't rename the controller
problem-first-network-title = Setting up a network for the controller
problem-first-network = There was none saved yet. Hold the NVIDIA button on the controller until its light flashes to pair it.
problem-pair-soon-title = Pairing shortly
problem-pair-soon = Pairing starts when shieldd tries the adapter again.
problem-no-adapter-title = Waiting for the adapter
problem-no-adapter = Pairing starts once the adapter is plugged in.
problem-lost-title = Lost connection to the adapter
problem-lost = Unable to pair or connect until it's accessible once more.
problem-save-title = Couldn't save the new network
problem-save =
    { $error }

    Controllers paired to it work until shieldd exits. Retrying.
problem-too-many-title = Too many controllers
problem-too-many = Only { $max } can be connected at once; { $mac } was sent away.
problem-nothing-paired-title = No controller paired
problem-nothing-paired = Nothing paired in 2 minutes; pairing is off again.
problem-pad-title = Couldn't add a virtual pad
problem-pad =
    { $error }

    The controller was sent away.
problem-pair-title = Can't pair
