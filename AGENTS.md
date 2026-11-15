# Audio editor: rules for agents and contributors

A small multitrack audio editor in pure Rust: tracks, clips, fades, crossfades and volume curves.
It is not a DAW. When a feature only makes sense in a DAW (mixer busses, MIDI, plugins), leave it out.

## Layout

```text
apps/audioeditor   the egui window. Reads the project, sends commands, shows errors.
  main.rs          editor state, toolbar, shortcuts
  timeline.rs      drawing: tracks, clips, crossfades, ruler, scrollbar
  mouse.rs         mouse editing: hit-testing and drags
  files.rs         open, save, import, export, unsaved-changes question
  playback.rs      audio output (cpal)
crates/engine      project model, commands, undo, mixing, project files, import and export.
                   No UI or OS audio crates.
```

Only the window depends on `eframe`, `egui` and `rfd`. Everything the window can do must be possible
through `engine::Session::execute`, so a CLI or an agent interface can be added later without a rewrite.
Add a new crate only when a part has grown large enough to need its own boundary.

## 1. Never crash

People trust the editor with their recordings. A broken file, an odd click or a full disk gives an
error message, never a crash and never lost work.

- Return `Result`; no `unwrap`, `expect`, `panic!`, `unreachable!`, `todo!` or slice indexing outside
  tests. The workspace lints deny them; `clippy.toml` allows them in tests.
- No `unsafe` (`unsafe_code = "forbid"`).
- Every number from a file or from the UI is hostile: use `get()`, `checked_*`/`saturating_*`, and
  never divide by a sample rate or length that can be zero.
- A failed command leaves the project unchanged. `Session::execute` edits a copy and only keeps it on success.

## 2. Clean room

- Learn from how other editors behave and from public specifications. Do not copy code, icons,
  strings or layouts from other products. This applies to Audacity in particular: it is GPL, so
  reading its source and writing similar code here is not allowed.
- Every asset (icon, sound, font) is original or has a licence that allows reuse, and is listed in
  `ATTRIBUTION.md` in the same change.
- Check a dependency's licence before adding it: MIT, Apache-2.0, BSD, ISC, zlib, CC0 are fine.

## 3. Time

All edit math uses whole sample frames (`i64`) at the project sample rate. No `f64` seconds in the
model; seconds are only for display.

## 4. Tests

Test the mixing maths and commands with a few concrete samples. Every crash fix gets a regression
test that failed before the fix.

Check window changes without a screen: `apps/audioeditor/src/ui_tests.rs` drives the real window
with `egui_kittest` (mouse, wheel, keys) and saves images to `target/ui-snapshots/`. Look at the
images before saying a window change works. Add a test there only for a real window bug.

Run before you finish: `cargo clippy --all-targets` and `cargo test`.
