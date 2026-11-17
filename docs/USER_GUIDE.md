# BETTERBS user guide

## 1. Install and start

1. Download `betterbs-windows.zip` from [Arlo Maker](https://arlobruni67.github.io/arlo-maker/).
2. Unzip the whole folder anywhere, for example your Desktop.
3. Double-click **BETTERBS.exe**.

You need Windows 10 or 11 (64-bit). An NVIDIA card (NVENC) or AMD card (AMF) records with almost no slowdown;
without one BETTERBS records on the CPU with x264.

## 2. Your first recording

1. Press **Optimise** in the top bar. BETTERBS records short test clips with each preset and recommends the best one
   that stays smooth on your PC.
2. Pick a video mode in the top bar: **YouTube Wide video (16:9)**, **YouTube Short (9:16 vertical)** or
   **Custom resolution**. In Short mode the game fills the tall frame and its left and right edges are cut off.
3. Under **Sources**, press **+** and add a **Display Capture**, **Window Capture** or **Game Capture**.
4. Press **Start Recording**. **Pause** / **Resume** while recording; **Stop Recording** to finish.
5. Press **Recordings** to open the folder with your videos.

## 3. Scenes and sources

* A **scene** is a layout of sources. Add, remove, rename and reorder scenes with the buttons under **Scenes**.
* Switching scenes uses the transition chosen under the scene list: **Cut**, **Fade**, **Fade through black**,
  **Slide** or **Wipe**, with its length in milliseconds.
* Source types:
  * **Display Capture**: a whole monitor (optionally a custom region).
  * **Window Capture**: one window.
  * **Game Capture**: follows a game by its program name. It waits for the game to start and never injects
    anything into it, so it is safe with anti-cheat.
  * **Scene**: another scene, placed inside this one.
* In the preview, click a source to select it, drag to move it, drag the handles to resize or rotate.
  The **Transform** dialog sets exact values.
* The eye and lock buttons hide or lock a source.
* **Scene Collections** (top bar) keep separate sets of scenes: new, duplicate, rename or delete.

## 4. Audio

The **Audio Mixer** has **Desktop Audio** and **Microphone**. Each has a volume slider, a mute button and a
live meter. Each audio source is written to its own track, so you can balance them later in an editor.

## 5. Settings

Open **Settings** in the top bar:

| Page | What you set |
|---|---|
| Output | recordings folder, file name format, MKV or MP4, remux to MP4 after recording |
| Video | video mode, canvas and output size, frame rate (30, 29.97, 60, 59.94, 120, 144), colour (Rec. 709 or 601) |
| Encoder | the preset (SMOOTH, BALANCED, QUALITY) or Custom settings; only options your encoder really supports are shown |
| Audio | audio codec and bitrate, number of tracks, A/V sync offset |
| Preview | preview frame rate (full, half or quarter; never affects the recording), and a summary of this PC |

Presets:

* **SMOOTH**: lightest on the graphics card.
* **BALANCED**: the default.
* **QUALITY**: the best picture, recommended by Optimise when your PC keeps up.

Recording to **MKV** (the default) is safest: if the PC crashes, the file up to that moment still plays.
Turn on **remux to MP4** to get an MP4 copy when recording stops.

## 6. Screenshots and preview

* **Screenshot** saves a still of the canvas as a `.bmp` into the recordings folder.
* Untick **Preview** to save a little GPU time while recording, or set Settings > Preview to half rate.

## 7. Where things are kept

* Recordings: a folder called `EBS Recordings` on your largest non-system drive, or `Videos\EBS` when there is
  only one drive. Change it in Settings > Output.
* Settings, scenes and the log (`ebs.log`): `%APPDATA%\EBS-rs`.

## 8. Troubleshooting

* **Game Capture shows "Waiting for ..."**: start the game; BETTERBS picks it up by itself.
* **An encoder is missing**: BETTERBS tests every encoder in a separate process when it starts; ones that fail on your
  PC are hidden. Use Optimise to pick a working preset.
* **Black preview**: some apps block capture of their window; use Display Capture instead.
* Check `%APPDATA%\EBS-rs\ebs.log` for details.
