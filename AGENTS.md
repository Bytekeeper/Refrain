# Refrain: rules for agents and contributors

Refrain is a small multitrack audio editor in pure Rust: tracks, clips, fades, crossfades, volume curves,
effects and generated sound (tone, noise, blips).
It is not a DAW. When a feature only makes sense in a DAW (mixer busses, MIDI, plugins), leave it out.

## Layout

```text
apps/refrain       the egui window. Reads the project, sends commands, shows errors.
  main.rs          editor state, toolbar, shortcuts
  timeline.rs      the timeline: track rows and headers, ruler, scrollbar, layout
  clips.rs         drawing one clip and the overlap zone of a crossfade
  mixing.rs        the mixdown of the project, made in the background after each edit
  levels.rs        the Mix level row: the mixdown's peaks over the whole project
  mouse.rs         mouse editing: hit-testing, drags, snapping, and the clip, fade and channel menus
  lanes.rs         effect lanes under tracks and the mix: layout, drawing, range editing
  effect_menu.rs   an effect lane's menu (settings, fades, order) and the FX menu
  regions.rs       export lanes above the level row (regions in the engine): named ranges of the mix,
                   each exported as a file of its own
  generate.rs      adding a generated clip (tone, noise, blips) and its settings panel
  files.rs         open, save, import, unsaved-changes question
  export.rs        export panel and "export again" to the project's export target
  recovery.rs      copies of unsaved work, restored after a crash
  playback.rs      audio output (cpal): plays the latest mixdown
crates/engine      project model, commands, undo, mixing, effects, generated sound, project files,
                   import and export.
                   No UI or OS audio crates.
```

Only the window depends on `eframe`, `egui` and `rfd`. Everything the window can do must be possible
through `engine::Session::execute`, so a CLI or an agent interface can be added later without a rewrite.
Add a new crate only when a part has grown large enough to need its own boundary.

## 1. Never crash

People trust the editor with their recordings. A broken file, an odd click or a full disk gives an
error message, never a crash and never lost work.

- Return `Result`; no `unwrap`, `expect`, `panic!`, `unreachable!`, `todo!` or slice indexing outside
  tests. The workspace lints deny them; `clippy.toml` allows `unwrap`, `expect`, `panic!` and indexing
  in tests.
- No `unsafe` (`unsafe_code = "forbid"`).
- Every number from a file or from the UI is hostile: use `get()`, `checked_*`/`saturating_*`, and
  never divide by a sample rate or length that can be zero.
- A failed command leaves the project unchanged. `Session::execute` edits a copy and only keeps it on success.

## 2. Non-destructive

Every edit stays editable. Trims, fades, volume curves, crossfades, channels and effects are settings
stored in the project; the audio is computed from the files (or a generator's settings) whenever it
plays or is exported. Nothing
is ever applied to the audio for good, so a trim can be dragged back out and an effect changed or
removed later. Results that depend on the audio (a normalize gain, a match) are measured from the audio
each time, never stored as a fixed number.
Sound the editor makes (tone, noise, blips) is stored as its settings, never as samples, and made again
for a new length, loop length or sample rate.

Effects live in lanes below a track, or below the mix for effects across all tracks. A lane is one kind
of effect and the time ranges where it applies, each range with its own settings; lanes apply from the
top down.
The project is heard through its mixdown (`engine::mixdown`): the whole project rendered layer by layer,
each track's clips, then its lanes, then the mix lanes. Every effect hears all of its input, before and
after, so a reversed echo or reverb is the same effect run backwards. A range holds its whole effect:
the part the effect hears and its tail (at the end, or at the start when reversed), so nothing changes
outside it and ranges in a lane never meet. New settings keep the range's edges and move the tail's
boundary inside it. With a loop a range past the loop end goes on at the start. The window makes the
mixdown in the background after each edit; playback, the level row and export use it.

Region lanes (export lanes in the window) hold named parts of the mix. Each region is exported as a plain cut of the mixdown to
`folder/name` with its lane's format; a `/` in a name makes a subfolder. One export call writes the mix
and any regions from one mixdown.

## 3. Clean room

- Learn from how other editors behave and from public specifications. Do not copy code, icons,
  strings or layouts from other products. This applies to Audacity in particular: it is GPL, so
  reading its source and writing similar code here is not allowed.
- Every asset (icon, sound, font) is original or has a licence that allows reuse, and is listed in
  `ATTRIBUTION.md` in the same change.
- Check a dependency's licence before adding it: MIT, Apache-2.0, BSD, ISC, zlib, CC0 are fine.

## 4. Time

All edit math uses whole sample frames (`i64`) at the project sample rate. No `f64` seconds in the
model; seconds are only for display. Settings that mean the same at any rate, such as an echo's delay or
a blip's length, are kept in ms or seconds and turned into frames where they are used.

A project has one working sample rate. Audio at another rate is converted to it when it is added
(`engine::resample`); the files on disk are never changed. Changing the project rate converts again
from the files, not from already converted audio.

The only file the editor overwrites is the project's export target, which the user chose. When that is
audio the project or its undo history uses, `Session::export` copies the original aside first and points
the audio at the copy (`Session::relocate`), so clips, undo and rate changes keep the original.
`Session::save` moves such copies into an `originals` folder next to the project file. Both live in the
engine so every front end keeps the files safe the same way.

## 5. Tests

Test the mixing maths and commands with a few concrete samples. Every crash fix gets a regression
test that failed before the fix.

Check window changes without a screen: `apps/refrain/src/ui_tests.rs` drives the real window
with `egui_kittest` (mouse, wheel, keys) and saves images to `target/ui-snapshots/`. Look at the
images before saying a window change works. Add a test there only for a real window bug.

Run before you finish: `cargo clippy --all-targets` and `cargo test`.
