# BETTERBS

A light screen and game recorder for Windows, written in Rust, that does not drop frames.

* Display, window and game capture (Windows Graphics Capture, no injection into the game)
* Zero-copy NVENC encoding on NVIDIA cards, plus x264 and SVT-AV1 on the CPU
* Desktop audio and microphone, each on its own track
* Scenes, nested scenes, scene transitions and scene collections
* YouTube Wide (16:9) and YouTube Short (9:16) modes
* **Optimise**: tests your PC and picks the best smooth quality preset

## Download

Get the ready-to-run app from **[Arlo Maker](https://arlobruni67.github.io/arlo-maker/)**:
unzip it and double-click `BETTERBS.exe`.

## Documentation

* **[User guide](docs/USER_GUIDE.md)**: install, first recording, scenes and sources, audio, settings, troubleshooting
* [Building from source](docs/BUILDING.md)

## Code layout

| Folder | What it is |
|---|---|
| `crates/engine` | capture, GPU compositor, audio, encoders, muxer, recorder, benchmark |
| `crates/app` | the BETTERBS window (egui) |
| `crates/bench` | `ebs-bench`, a command-line tool for recording and encoder tests |

## Licence

GPL-3.0-or-later. See [LICENSE.txt](LICENSE.txt) and [NOTICE.txt](NOTICE.txt). BETTERBS uses FFmpeg (GPL-3.0).
