# Pumpkin User Manual

Pumpkin displays X-ray diffraction images from DECTRIS EIGER detectors. It can
follow a detector live through the DCU, or open finished datasets from disk.

For installation, building and the full configuration reference see
[README.md](README.md). This manual explains how to *use* the program.

## Contents

1. [Starting Pumpkin](#1-starting-pumpkin)
2. [The window](#2-the-window)
3. [Looking at an image](#3-looking-at-an-image)
4. [Contrast and colour](#4-contrast-and-colour)
5. [Opening data](#5-opening-data)
6. [Live monitoring](#6-live-monitoring)
7. [Browsing a dataset](#7-browsing-a-dataset)
8. [Measuring: hover, loupe and line profile](#8-measuring-hover-loupe-and-line-profile)
9. [Overlays and resolution rings](#9-overlays-and-resolution-rings)
10. [Dozor quality plot](#10-dozor-quality-plot)
11. [Saving a PNG](#11-saving-a-png)
12. [Remote control](#12-remote-control)
13. [Files Pumpkin keeps](#13-files-pumpkin-keeps)
14. [Keyboard and mouse reference](#14-keyboard-and-mouse-reference)
15. [Troubleshooting](#15-troubleshooting)

---

## 1. Starting Pumpkin

```
pumpkin [--dcu-url <URL>] [--poll-period-ms <MS>] [--config <PATH>]
```

| Option | Meaning |
|---|---|
| `--dcu-url <URL>` | Address of the DCU, e.g. `http://192.168.1.100`. Pumpkin connects automatically at start-up. |
| `--poll-period-ms <MS>` | How often to ask the detector for new frames. |
| `--config <PATH>` | Use this config file instead of `~/.config/pumpkin/config.toml`. |

With no options, Pumpkin starts with an empty viewport (or a splash image if
one is configured). You can then open a file or connect to a detector from the
side panel.

Everything described here can be preset in the config file; see the README.

## 2. The window

The window has two parts:

* **The viewport** (right, large): the diffraction image, with overlays.
* **The side panel** (left): everything else. Press **Tab** to hide or show it.

The side panel is divided into accordion sections. Click a section title to
open it (only one is open at a time):

* **Current dataset**: metadata, frame controls and the contrast controls.
* **Data browser**: appears only if a `[data_browser]` section is configured
  (see [Opening data](#5-opening-data)).

Two more windows open on demand:

* **Actions…** (link at the bottom of the side panel): open a file, save a
  PNG, connect/disconnect, commands-file settings, overlay settings, zoom speed
  and line-profile width.
* **Help (?)**: the keyboard shortcut list. Press **?** to toggle it.

Press **F11** for fullscreen (and again to leave it).

## 3. Looking at an image

| Action | Effect |
|---|---|
| Scroll wheel | Zoom in and out around the mouse pointer. |
| Left-button drag | Pan. |
| **Ctrl+0** | Fit the whole image into the viewport. |
| **Ctrl+1** | Zoom to 1:1 (one screen pixel per detector pixel). |

The zoom speed of the wheel can be changed in **Actions… → Viewport**.

When you zoom in far enough (about 15 screen pixels per detector pixel), the
raw value of each pixel is printed on top of it.

**Black pixels** are either masked detector pixels (gaps, dead pixels) or
saturated pixels.

## 4. Contrast and colour

The **Contrast** section (in *Current dataset*) controls how raw counts map to
colours. The image is mapped linearly between two values:

* **Background**: counts at or below this are shown as the lowest colour.
* **Foreground**: counts at or above this are shown as the highest colour.

Both sliders are logarithmic, so small values are easy to reach. You can also
click the number next to a slider and type an exact value.

### Auto contrast

* **Auto**: when ticked, the contrast is recomputed for every new frame. The
  sliders are greyed out while it is on. Ticking is the default.
* **Run**: compute the auto contrast once for the current frame.
* **Region**: compute it from only the part of the image currently visible.
  Useful after zooming in on a weak feature.
* **Auto-region**: recompute the region contrast after every pan or zoom.

Auto contrast looks only at valid pixels (not masked, not saturated, not zero)
and uses percentiles rather than the maximum, so a few hot pixels don't wash
out the image.

### Adjusting by hand

* Drag the **Background** and **Foreground** sliders.
* **Hold F and drag the left mouse button sideways over the image** to change
  Foreground without touching the side panel. Dragging right brightens the
  image, dragging left darkens it. This switches Auto off. The image does not
  pan while F is held.
* **Gamma**: raises the normalised value to a power. 1.0 is linear; larger
  values darken the background and keep bright peaks visible.

### Histogram

The plot under the sliders shows the distribution of pixel values. The green
line marks Background and the red line marks Foreground. **Log** switches the
vertical axis to a logarithmic scale and **Bins** sets the resolution.

### Colormap

Choose from *Inferno, Viridis, Plasma, Standard, Grayscale, Rocket, Heat*.
"Standard" runs black to white; "Grayscale" is the reverse (white for low
counts). A preview bar shows the current map. The default map can be set in
the config file (`[contrast] colormap`, which accepts Standard, Grayscale,
Inferno, Rocket and Heat).

### Force saturation

Tick **Force saturation** and enter a value to treat pixels at or above that
value as saturated (drawn black), instead of the saturation value stored in the
file or reported by the detector.

## 5. Opening data

There are four ways to load images.

### Open an HDF5 file

Press **Ctrl+O** (or **Actions… → Open HDF5…**) and pick the `*_master.h5` file
of a dataset. The data files must be in the same directory. Opening a file
disconnects from the live monitor.

Pumpkin remembers the folder of the last file you opened.

### The data browser

If your beamline configures a `[data_browser]`, the **Data browser** section
lists proposals you belong to (found from your OS group memberships). Expand a
proposal, then its date and sample folders, and click a dataset to open it. Use
the filter box to narrow long lists.

At the top of the data browser is **Recent monitored**: the datasets that
appeared while Pumpkin was connected to the detector. Newest first, up to 30,
with a note of how long ago each was seen. Click one to open it. Entries whose
file can't be found on disk are greyed out and struck through. The list is
remembered between sessions.

### Live from the detector

See [Live monitoring](#6-live-monitoring).

### From another program

External tools can tell Pumpkin which file and frame to show; see
[Remote control](#12-remote-control).

## 6. Live monitoring

In **Actions… → Connection**, enter the **DCU URL** and press **Connect**
(or start Pumpkin with `--dcu-url`, or set `auto_connect` in the config).
Pumpkin polls the DCU and shows the newest frame as it arrives.

* **Monitor browser** (in *Current dataset*): while connected you can step
  back through frames still held in the DCU buffer with the large ◀ / ▶
  buttons, or pick a frame from the **Go to frame** list.
* **Zooming and panning pause live updates** for a couple of seconds
  (`monitor_pause_ms`), so the image doesn't change under you. It catches up
  once you stop.
* **Slower when not needed:** when the window is not focused Pumpkin polls
  less often, and after 10 minutes without any input it stops polling
  altogether to spare the detector (`pause_if_idle_after`). Any input resumes
  it.
* **Disconnect** stops monitoring. Opening a file also disconnects.

Each new detector series is added to the **Recent monitored** list of the data
browser.

## 7. Browsing a dataset

When an HDF5 file is open, the **HDF5 frame browser** section shows:

* the number of frames,
* **Grouping**: sum this many consecutive frames into one displayed image
  (useful for weak data). The frame index always snaps to a multiple of the
  grouping,
* a **group slider** to scrub through the series,
* buttons: **|◀** first, **◀** previous, **▶** next, **▶|** last.

**Left / Right arrow keys** step through frames when the mouse is over the
viewport. **Ctrl+G** opens a small box to jump to a frame number.

Frames are read in the background, so the window stays responsive. The next
frame is read ahead of time, which makes stepping forward feel instant.

### Movie mode

Press **Ctrl+P** (or the **🎞 Movie** button) to play through the series
automatically; press it again to stop. The speed is set in the **fps** box next
to the button (0.5 to 60 frames per second, default 10).

* Playback stops at the last frame. Starting from the last frame rewinds to
  the first.
* Movie mode only works with an open HDF5 file, not while connected to the
  live monitor.
* If frames take longer to read than the frame interval, playback runs slower
  than the chosen rate.

## 8. Measuring: hover, loupe and line profile

### Hover read-out

Rest the mouse on the image. A small balloon shows the pixel coordinates, the
raw value and, if the file contains the beam geometry, the resolution
(d-spacing in Å) at that position.

### Loupe

**Hold Z** to show a magnified view around the cursor with the pixel values.
Its size is set by `loupe_radius` in the config file (half-width in detector pixels).

### Line profile

Drag with the **right** mouse button across the image. A **Line Profile**
window shows the summed intensity along the line. Peaks are marked and, where
the geometry is known, labelled with their d-spacing; the spacing between
neighbouring peaks is annotated too.

* The profile is summed across a band; set its width in
  **Actions… → Line profile → Width** (1 to 200 pixels).
* Close the window (or press **Esc**) to remove the line.

## 9. Overlays and resolution rings

Open **Actions… → Overlays** to configure:

* **Beam center**: a crosshair at the beam position, with colour and line
  width.
* **Resolution rings**: circles at chosen d-spacings, with colour, line width
  and label size.

Toggle the resolution rings quickly with **Ctrl+R**.

Both need the beam geometry (beam centre, detector distance, pixel size and
wavelength or energy) in the file or from the detector. Without it nothing is
drawn.

The rings assume a flat detector perpendicular to the beam. Which d-spacings
are drawn is set in the config file with `[[rings]]` entries; for example, to
mark the typical hexagonal-ice rings:

```toml
[[rings]]
resolution = 3.90
label = "ice 3.9"

[[rings]]
resolution = 3.67

[[rings]]
resolution = 3.44

[[rings]]
resolution = 2.67

[[rings]]
resolution = 2.25
```

Sharp rings at these positions mean crystalline ice in the sample. A broad
diffuse band around 3.7 Å is water or vitreous ice rather than crystalline ice.

## 10. Dozor quality plot

If Pumpkin finds a Dozor result for the open dataset, a chart appears along the
bottom of the viewport with per-frame score, spot count and visible
resolution. Click a point to jump to that frame. Hover for the numbers. The
**▼ Dozor / ▲ Dozor** button hides or shows the chart.

Pumpkin looks for the result next to the standard MAX IV processing layout:

```
<date>/raw/<protein>/<sample>/<sample>_<run>_master.h5
<date>/process/<protein>/<sample>/xds_<sample>_<run>_*/ControlPyDozor_*/outDataControlPyDozor.json
```

If the file isn't in that layout, no chart is shown.

## 11. Saving a PNG

Press **Ctrl+S** (or **Actions… → Save PNG**) to save the current image, with
the current contrast, colormap and overlays, as a PNG in your home directory.
The file name comes from the dataset name and frame number. Experiment
metadata (distance, wavelength, pixel size, and so on) is stored in the PNG
text chunks. The PNG is full detector resolution, independent of zoom.

## 12. Remote control

Other programs can make Pumpkin open a file and frame by sending one JSON
object per line to its TCP port (default 8100):

```bash
echo '{"file": "/data/run1_master.h5", "frame": 42}' | nc localhost 8100
```

The same JSON can be appended to a **commands file**. Enable it and set its
path and check interval in **Actions… → Commands file**, or in the config
(`commands_file`, `commands_file_enabled`, `commands_file_poll_interval_ms`).
Both mechanisms open the file if it isn't already open and jump to the frame.

While a remote frame is shown, live updates are held so the image doesn't
change; a new detector series ends the hold.

## 13. Files Pumpkin keeps

All in `~/.config/pumpkin/`:

| File | Content |
|---|---|
| `config.toml` | Your settings (you write this). |
| `monitored_files.json` | The **Recent monitored** list. |
| `last_location.txt` | Folder of the last file you opened. |
| `proposals_cache.json` | Proposals found for the data browser (refreshed daily). |

You can delete any of these except `config.toml` to reset that item; Pumpkin
recreates them.

## 14. Keyboard and mouse reference

### Keyboard

| Key | Action |
|---|---|
| **Ctrl+O** | Open an HDF5 master file |
| **Ctrl+G** | Go to a frame number |
| **Ctrl+S** | Save the current view as PNG |
| **Ctrl+0** | Fit image to the viewport |
| **Ctrl+1** | Zoom to 1:1 |
| **Ctrl+P** | Play / stop movie (HDF5 only) |
| **Ctrl+R** | Show / hide resolution rings |
| **Ctrl+Q** | Quit |
| **← / →** | Previous / next frame (mouse over the image) |
| **Tab** | Hide / show the side panel |
| **F11** | Fullscreen |
| **?** | Help window |
| **Esc** | Close the open dialog or window |
| **Hold Z** | Loupe |
| **Hold F + left-drag** | Adjust contrast (Foreground) |

### Mouse (over the image)

| Action | Effect |
|---|---|
| Scroll | Zoom |
| Left-drag | Pan |
| Right-drag | Line profile |
| Hold F + left-drag | Change contrast |
| Hover | Pixel value and resolution balloon |

## 15. Troubleshooting

**The image is black or flat.**
Auto contrast may be off. Tick **Auto** and press **Run**, or lower
**Foreground**. If "Force saturation" is set very low, everything counts as
saturated and is drawn black.

**No resolution rings, or no resolution in the hover balloon.**
The beam geometry is missing from the file or the detector. Check the
*Metadata* list in *Current dataset*.

**HDF5 file fails to open, or frames don't load.**
EIGER files use bitshuffle compression, which needs the HDF5 bitshuffle
plugin. Set `hdf5_plugin_path` in the config (or the `HDF5_PLUGIN_PATH`
environment variable) to the folder containing it. Errors are printed on the
terminal Pumpkin was started from.

**Nothing updates in live mode.**
Zooming or panning pauses updates for a moment. After 10 minutes with no input
monitoring pauses; move the mouse or press a key. Check the **DCU URL** in
*Actions… → Connection*.

**Movie mode / Ctrl+P does nothing.**
It needs an open HDF5 series and does not work while connected to the live
monitor. Disconnect first.

**A "Recent monitored" entry is struck through.**
Pumpkin can't find that file from this computer. It may have been moved, or
the storage isn't mounted here.

**The Data browser section is missing.**
It appears only when the config file has a `[data_browser]` section.
