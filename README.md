# Linaux Screen

**Turn your phone or tablet into a second monitor for your Linux PC, wirelessly, with nothing to install on the device.**

Linaux Screen adds a real extra monitor to your KDE Plasma desktop and shows it in your device's web browser. Drag windows onto it, play the PC's sound through it, and, if you allow it, control the PC with touch, a trackpad or a keyboard. Think *spacedesk*, but for Linux.

It's built for KDE Plasma 6 on Wayland and developed on [Bazzite](https://bazzite.gg). Day to day it runs at around 55–60 fps at 1080p over home Wi-Fi, with no noticeable delay.

## Features

- **A real second screen.** A new monitor appears in KDE's Display settings. Arrange it, set its resolution, and drag windows onto it like any plugged-in screen.
- **No app on the device.** Open an address in the browser on an iPad, iPhone, Android phone or tablet, or even another laptop.
- **Several devices at once.** Up to four devices can watch the same screen together.
- **Sound to a device.** Send the PC's sound to any connected device from the tray. It goes back to the PC the moment you switch back or the device leaves.
- **Touch, trackpad and keyboard control.** Tap to click, drag windows, scroll, and type on a Bluetooth keyboard. Off until you say so: each device has to ask, and you Accept or Deny on the PC.
- **Quality presets per device.** Battery saver, Smooth, Sharp or Ultra, switched instantly from the device.
- **Fast.** Hardware video encoding (NVIDIA, AMD or Intel) streamed over WebRTC.
- **Stays out of the way.** It starts only when you open it and lives in the system tray. It stops by itself a minute after the last device disconnects. Nothing runs at login.

## What you need

- **A Linux PC with KDE Plasma 6 on Wayland.** The ready-made download is built for Bazzite and other Fedora 44–based systems. On other distributions, [build it from source](docs/DEVELOPMENT.md).
- **Ideally a graphics card that can encode H.264.** Almost any NVIDIA, AMD or Intel GPU from the last decade can. Without one it falls back to the CPU.
- **A phone, tablet or laptop with a modern browser** (Safari, Chrome, Firefox or Edge), on the same network as the PC.

The PC side is Linux only. On Windows, use spacedesk.

## Install

1. **Download** `linaux-screen-<version>-x86_64.tar.gz` from the [latest release](https://github.com/janders1993/Linaux-Screen/releases/latest).
2. **Unpack it and run the installer** in a terminal:
   ```bash
   tar xf linaux-screen-*-x86_64.tar.gz
   cd linaux-screen-*/
   ./install.sh
   ```
   This puts the app in `~/.local/bin` and adds **Linaux Screen** to your app menu. It needs no root and no reboot, and installs nothing system-wide.

   It also checks that your PC has everything needed for streaming, and tells you if anything is missing. On Bazzite it adds one small streaming component Bazzite doesn't include (GStreamer's `nicesrc`), into your home folder.

**Updating:** download the new release and run its `./install.sh`. If the app is running, choose **Restart** in its tray menu to switch to the new version.

**Uninstalling:** `./install.sh --uninstall`. Your settings in `~/.config/linaux-screen` are left in place, in case you come back.

## Using it

### Connect a device

1. **On the PC,** open **Linaux Screen** from the app menu (or KRunner, Alt+Space). The first time, KDE asks you to allow screen sharing and remote control. The app remembers your answer, so you're normally only asked once. Allowing remote control here doesn't let any device in by itself; see [Controlling the PC](#controlling-the-pc).
2. A notification shows the address, like `http://192.168.1.20:4444`. A tray icon appears while it runs, and its menu can copy the address for you.
3. **On the device,** open that address in the browser. Tap the screen to go fullscreen. A home-screen bookmark makes this one tap next time.

**When you're done,** close the browser tab. A minute after the last device leaves, the app stops by itself and the extra monitor disappears.

### Arrange the screen

While the app runs there's a new monitor, *Virtual Output*, in **System Settings → Display Configuration**. Drag it to where it sits relative to your other screens, and set its resolution and scale; 2560×1440 suits a 13" iPad well. KDE remembers these settings for next time. If you change the resolution while the app is running, choose **Restart** in the tray.

All connected devices show the same screen, scaled to fit, with black bars where the shapes differ.

### The tray menu

- **Who's connected:** for example *iPad (192.168.1.120)*.
- **Copy address**
- **Sound output:** *This PC*, or any connected device. Nothing changes until you pick. Sound goes back to the PC when you pick *This PC*, the device disconnects, or the app stops.
- **Control requests:** *iPad wants to control this PC*, with **Accept** / **Deny**. While a device has control: *iPad has control — Revoke*.
- **Restart:** also switches to a newly installed version.
- **Disconnect all devices**
- **Stop**

### The device menu

Tap the screen with **three fingers** to open the menu on the device (or press `s` on a keyboard, when not controlling the PC). It shows connection stats and has:

**Quality buttons.** Each device picks its own quality, and it changes instantly. Every visit starts on Smooth.

| Preset | Frame rate | Data at 1080p | Good for |
|---|---|---|---|
| Battery saver | 30 fps | ~6 Mbit/s | Long sessions, weaker Wi-Fi |
| **Smooth** | 60 fps | ~12 Mbit/s | Everyday use |
| Sharp | 60 fps | ~18 Mbit/s | Lots of text, good Wi-Fi |
| Ultra | 60 fps | ~36 Mbit/s | Strong Wi-Fi 6, or an iPad on Ethernet |

Higher monitor resolutions use proportionally more data.

**Request control:** see below.

If you send sound to a device, it may show **Tap to enable sound**. Browsers only play sound after you've touched the page.

### Controlling the PC

Nothing a device does reaches the PC until you allow it.

1. On the device, open the three-finger menu and tap **Request control**.
2. On the PC, a notification asks **Allow control of this PC?** Choose **Accept** or **Deny**; the same choice is in the tray. No answer within 30 seconds counts as Deny, and a denied device has to wait 30 seconds before asking again.
3. Once accepted:

   | On the device | On the PC |
   |---|---|
   | Tap | Left click |
   | Hold (½ second) | Right click |
   | Drag one finger | Click and drag (move windows, select text) |
   | Drag two fingers | Scroll |
   | Trackpad or mouse | Works as normal: pointer, clicks, two-finger scroll |
   | Keyboard | Types on the PC. The iPad's **Cmd** key is the Super (Windows) key |
   | Three-finger tap | Opens the device menu; never sent to the PC |

4. Control ends when the device taps **Release control** or disconnects, or when you choose **Revoke**, **Disconnect all**, **Restart** or **Stop** on the PC.

There's no on-screen keyboard; use a physical (e.g. Bluetooth) keyboard. iPadOS keeps a few shortcuts for itself (Cmd+Tab, Cmd+Space, Cmd+H).

## Privacy and security

- **It stays on your home network.** Nothing goes through the internet or any outside server, and the video itself is encrypted (WebRTC).
- **Watching isn't password-protected.** While the app is running, anyone on the same network who opens the address can see the shared screen. The tray always lists who's connected, and **Disconnect all** sends them away. Use it on networks you trust.
- **Control always needs your OK,** per device, every time it connects.
- **It only runs when you start it,** and stops by itself when unused.

## Troubleshooting

- **The device can't connect.** Check it's on the same Wi-Fi as the PC, not a guest network, and use the exact address from the notification or the tray. If your PC's firewall blocks incoming connections, allow TCP port 4444 and UDP (the video uses random high ports). Bazzite's default setup works without changes.
- **Black screen or very low frame rate.** Choose **Restart** in the tray. If it keeps happening, look at the log (below).
- **No sound on the device.** Tap the device's screen once (browsers need a tap), and check the device isn't muted.
- **KDE asks for permission every time.** If KDE's prompt offers to remember the choice, tick it. The app also saves the permission separately for each way you start it (app menu or terminal), so the first launch each way asks once.
- **The log** is at `~/.local/state/linaux-screen/linaux-screen.log`. Please attach it if you [open an issue](https://github.com/janders1993/Linaux-Screen/issues).

## Building from source and contributing

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for how it works inside, building from source, command-line options and the roadmap.

## Licence

MIT. See [LICENSE](LICENSE).
