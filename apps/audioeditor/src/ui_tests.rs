//! Renders the window without a screen. Images go to `target/ui-snapshots` for a person to look at.

use std::path::PathBuf;
use std::sync::Arc;

use audioeditor_engine::{Command, Source};
use egui::{Event, Modifiers, MouseWheelSource, MouseWheelUnit, TouchPhase, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use super::Editor;

const RATE: u32 = 8000;

fn tone(freq: f32, secs: f32) -> Arc<Source> {
    let frames = (secs * RATE as f32) as usize;
    let wave = (0..frames)
        .map(|i| 0.6 * (i as f32 * freq * std::f32::consts::TAU / RATE as f32).sin())
        .collect();
    Arc::new(Source {
        path: None,
        name: format!("{freq} Hz"),
        sample_rate: RATE,
        channels: vec![wave],
    })
}

/// One track: A at 0..3 s and B at 2..5 s, overlapping by one second; A is selected.
fn overlapping() -> Editor {
    let mut e = Editor::default();
    e.run(Command::AddTrack { name: "Track".into() });
    let track = e.session.project().tracks[0].id;
    e.run(Command::AddClip {
        track,
        source: tone(220.0, 3.0),
        start: 0,
    });
    e.run(Command::AddClip {
        track,
        source: tone(330.0, 3.0),
        start: 2 * RATE as i64,
    });
    e.selected = Some(e.session.project().tracks[0].clips[0].id);
    e.frames_per_px = 50.0;
    e
}

fn harness(editor: Editor) -> Harness<'static, Editor> {
    harness_sized(editor, 300.0)
}

fn harness_sized(editor: Editor, height: f32) -> Harness<'static, Editor> {
    let mut h = Harness::builder()
        .with_size([900.0, height])
        .wgpu()
        .build_ui_state(|ui, e: &mut Editor| e.show(ui), editor);
    h.run();
    h
}

fn save(h: &mut Harness<'_, Editor>, name: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    h.render().unwrap().save(dir.join(format!("{name}.png"))).unwrap();
}

fn wheel(h: &mut Harness<'_, Editor>, delta_y: f32, modifiers: Modifiers) {
    h.hover_at(egui::pos2(500.0, 120.0));
    h.event_modifiers(
        Event::MouseWheel {
            unit: MouseWheelUnit::Line,
            delta: vec2(0.0, delta_y),
            phase: TouchPhase::Move,
            source: MouseWheelSource::Unknown,
            modifiers,
        },
        modifiers,
    );
    h.run();
}

fn drag(h: &mut Harness<'_, Editor>, from: egui::Pos2, to: egui::Pos2) {
    drag_with(h, egui::PointerButton::Primary, from, to);
}

fn drag_with(h: &mut Harness<'_, Editor>, button: egui::PointerButton, from: egui::Pos2, to: egui::Pos2) {
    let press = |pos, pressed| Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: Modifiers::NONE,
    };
    h.hover_at(from);
    h.run();
    h.event(press(from, true));
    h.run();
    for step in 1..=10 {
        h.hover_at(from + (to - from) * (step as f32 / 10.0));
        h.run();
    }
    h.event(press(to, false));
    h.remove_cursor();
    h.run();
}

fn click(h: &mut Harness<'_, Editor>, x: f32, y: f32) {
    let pos = egui::pos2(x, y);
    h.hover_at(pos);
    h.event(Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    h.event(Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
    h.run();
}

/// Regression: the selected clip was hidden under the next clip where they overlap.
#[test]
fn selected_clip_is_on_top_of_an_overlap() {
    let mut h = harness(overlapping());
    let a = h.state().session.project().tracks[0].clips[0].id;
    save(&mut h, "overlap_first_selected");

    // Inside the overlap (2..3 s): the visible clip is the one hit.
    click(&mut h, 520.0, 110.0);
    assert_eq!(h.state().selected, Some(a));
    let b = h.state().session.project().tracks[0].clips[1].id;
    h.state_mut().selected = Some(b);
    h.run();
    save(&mut h, "overlap_second_selected");
}

/// Regression: there was no way to see or change the visible part without knowing Shift+wheel.
#[test]
fn scrollbar_and_shift_wheel_pan() {
    let mut h = harness(overlapping());
    for _ in 0..5 {
        wheel(&mut h, -3.0, Modifiers::SHIFT);
    }
    let after_wheel = h.state().scroll;
    assert!(after_wheel > 0.0);
    save(&mut h, "after_shift_wheel");

    // Drag the scrollbar handle (the bar sits below the track and the new-track row) by 60 px to the left.
    drag(&mut h, egui::pos2(500.0, 242.0), egui::pos2(440.0, 242.0));
    let after_drag = h.state().scroll;
    assert!(
        after_drag < after_wheel,
        "{after_drag} should be less than {after_wheel}"
    );
    drag(&mut h, egui::pos2(440.0, 242.0), egui::pos2(-500.0, 242.0));
    assert_eq!(h.state().scroll, 0.0);
    save(&mut h, "scrolled_back");
}

#[test]
fn scrolling_stops_after_the_end() {
    let mut h = harness(overlapping());
    for _ in 0..200 {
        wheel(&mut h, -10.0, Modifiers::SHIFT);
    }
    let project_len = h.state().session.project().len() as f64;
    assert!(
        h.state().scroll < project_len,
        "scroll {} past the end {}",
        h.state().scroll,
        project_len
    );
    save(&mut h, "scrolled_to_end");
}

#[test]
fn stereo_clip_shows_both_channels() {
    let left: Vec<f32> = tone(220.0, 2.0).channels[0].clone();
    let right = vec![0.0; left.len()];
    let mut e = Editor::default();
    e.run(Command::AddTrack { name: "Stereo".into() });
    let track = e.session.project().tracks[0].id;
    let source = Arc::new(Source {
        path: None,
        name: "left only".into(),
        sample_rate: RATE,
        channels: vec![left, right],
    });
    e.run(Command::AddClip {
        track,
        source,
        start: 0,
    });
    e.frames_per_px = 50.0;
    let mut h = harness(e);
    save(&mut h, "stereo");
}

#[test]
fn middle_drag_pans_without_moving_clips() {
    let mut h = harness(overlapping());
    h.state_mut().scroll = 20_000.0;
    h.run();
    let starts = |h: &Harness<'_, Editor>| {
        h.state().session.project().tracks[0]
            .clips
            .iter()
            .map(|c| c.start)
            .collect::<Vec<_>>()
    };
    let before = starts(&h);
    drag_with(
        &mut h,
        egui::PointerButton::Middle,
        egui::pos2(500.0, 110.0),
        egui::pos2(600.0, 110.0),
    );
    // 100 px to the right at 50 frames per pixel shows 5000 frames earlier.
    assert!(
        (h.state().scroll - 15_000.0).abs() < 1_000.0,
        "scroll {}",
        h.state().scroll
    );
    assert_eq!(starts(&h), before);
}

#[test]
fn clip_can_be_dragged_past_the_start() {
    let mut h = harness(overlapping());
    drag(&mut h, egui::pos2(250.0, 120.0), egui::pos2(150.0, 120.0));
    let first = h.state().session.project().tracks[0].clips[0].clone();
    assert!(first.start < 0, "start {}", first.start);
    save(&mut h, "dragged_past_start");
}

/// The header's mute button, volume (one drag is one undo step) and trash can.
#[test]
fn track_header_controls() {
    let mut h = harness(overlapping());
    let track = |h: &Harness<'_, Editor>| h.state().session.project().tracks.first().map(|t| (t.muted, t.gain_db));
    click(&mut h, 34.0, 107.0);
    assert_eq!(track(&h), Some((true, 0.0)));

    drag(&mut h, egui::pos2(84.0, 107.0), egui::pos2(124.0, 107.0));
    let (_, db) = track(&h).unwrap();
    assert!((3.0..=5.0).contains(&db), "{db} dB");
    save(&mut h, "track_header");
    h.state_mut().undo();
    assert_eq!(track(&h), Some((true, 0.0)), "the whole drag is one undo step");

    click(&mut h, 40.0, 160.0);
    assert!(h.state().session.project().tracks.is_empty());
    assert_eq!(h.state().selected, None);
}

#[test]
fn dropping_below_the_tracks_makes_a_new_track() {
    let mut h = harness_sized(overlapping(), 420.0);
    let b = h.state().session.project().tracks[0].clips[1].id;
    let (from, to) = (egui::pos2(700.0, 120.0), egui::pos2(700.0, 230.0));
    let press = |pos, pressed| Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    h.hover_at(from);
    h.run();
    h.event(press(from, true));
    for step in 1..=10 {
        h.hover_at(from + (to - from) * (step as f32 / 10.0));
        h.run();
    }
    save(&mut h, "phantom_track");
    assert_eq!(h.state().session.project().tracks.len(), 1, "no track before the drop");
    h.event(press(to, false));
    h.remove_cursor();
    h.run();
    save(&mut h, "after_drop_on_new_track");

    let tracks = &h.state().session.project().tracks;
    assert_eq!(tracks.len(), 2);
    assert!(tracks[1].clips.iter().any(|c| c.id == b));
    h.state_mut().session.undo().unwrap();
    assert_eq!(h.state().session.project().tracks.len(), 1);
}

#[test]
fn add_track_button() {
    let mut h = harness_sized(overlapping(), 420.0);
    // The button sits at the top of the row below the last track.
    click(&mut h, 50.0, 190.0);
    assert_eq!(h.state().session.project().tracks.len(), 2);
    save(&mut h, "after_add_track");
}

#[test]
fn save_and_open_track_unsaved_changes() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-test-files/save");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Saved projects refer to audio files, so the test audio has to exist on disk.
    let wav = dir.join("tone.wav");
    audioeditor_engine::export_wav(overlapping().session.project(), &wav, false).unwrap();
    let mut e = Editor::default();
    e.run(Command::AddTrack { name: "Tone".into() });
    let track = e.session.project().tracks[0].id;
    let source = Arc::new(audioeditor_engine::load_source(&wav).unwrap());
    e.run(Command::AddClip {
        track,
        source,
        start: 0,
    });
    let mut h = harness(e);
    assert!(h.state().title.starts_with("• Untitled"), "{}", h.state().title);

    let project = dir.join("song.audioedit");
    h.state_mut().track_heights.insert(track, 250.0);
    h.state_mut().frames_per_px = 77.0;
    assert!(h.state_mut().save_to(&project));
    h.run();
    assert_eq!(h.state().title, "song – Audio editor");

    let clip = h.state().session.project().tracks[0].clips[0].id;
    h.state_mut().run(Command::MoveClip {
        clip,
        track,
        start: 4000,
    });
    h.run();
    assert!(h.state().title.starts_with("• song"));
    // Undo back to the saved state counts as saved again.
    h.state_mut().undo();
    h.run();
    assert_eq!(h.state().title, "song – Audio editor");

    let mut other = harness(Editor::default());
    other.state_mut().open_from(&project);
    other.run();
    assert_eq!(other.state().title, "song – Audio editor");
    assert_eq!(other.state().session.project().tracks[0].name, "Tone");
    let opened = other.state().session.project().tracks[0].id;
    assert_eq!(other.state().track_heights.get(&opened), Some(&250.0));
    assert_eq!(other.state().frames_per_px, 77.0);
    save(&mut other, "opened_project");
}

/// Trimming hides audio without deleting it: the edge can be dragged back out up to the file's end.
#[test]
fn trimmed_edges_can_be_dragged_back_out() {
    let mut e = Editor::default();
    e.run(Command::AddTrack { name: "Track".into() });
    let track = e.session.project().tracks[0].id;
    // 3 s at 1 s: x = 286..766 at 50 frames per pixel.
    e.run(Command::AddClip {
        track,
        source: tone(220.0, 3.0),
        start: RATE as i64,
    });
    e.frames_per_px = 50.0;
    let mut h = harness(e);
    let clip = || -> fn(&Editor) -> audioeditor_engine::Clip { |e| e.session.project().tracks[0].clips[0].clone() };

    drag(&mut h, egui::pos2(764.0, 120.0), egui::pos2(616.0, 120.0));
    drag(&mut h, egui::pos2(288.0, 120.0), egui::pos2(366.0, 120.0));
    let c = clip()(h.state());
    assert_eq!((c.start, c.end()), (12_000, 24_500));
    assert_eq!(c.hidden(), (4_000, 7_500));
    save(&mut h, "trim_hidden_both_ends");

    // Hold the edge far past the file's end: the outline shows the whole file, the clip stops at its end.
    let press = |pos, pressed| Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    let from = egui::pos2(614.0, 120.0);
    h.hover_at(from);
    h.run();
    h.event(press(from, true));
    for x in [640.0, 720.0, 820.0, 880.0] {
        h.hover_at(egui::pos2(x, 120.0));
        h.run();
    }
    save(&mut h, "trim_dragging_out");
    h.event(press(egui::pos2(880.0, 120.0), false));
    h.run();
    let c = clip()(h.state());
    assert_eq!(c.end(), 4 * RATE as i64);
    assert_eq!(c.hidden(), (4_000, 0));
}

/// Regression: with the right clip selected, the left clip's volume points in the overlap were hidden under it.
#[test]
fn volume_points_in_an_overlap_stay_visible() {
    let mut e = overlapping();
    let a = e.session.project().tracks[0].clips[0].id;
    let b = e.session.project().tracks[0].clips[1].id;
    let points = vec![
        audioeditor_engine::GainPoint { at: 8000, db: 0.0 },
        audioeditor_engine::GainPoint { at: 22000, db: -6.0 },
    ];
    e.run(Command::SetGainCurve { clip: a, points });
    e.run(Command::SetGainCurve {
        clip: b,
        points: vec![audioeditor_engine::GainPoint { at: 12000, db: -12.0 }],
    });
    e.selected = Some(b);
    let mut h = harness(e);
    save(&mut h, "overlap_points_right_selected");
}

/// Regression: at the deepest zoom a long clip is millions of pixels wide, and drawing all of it
/// made a buffer bigger than the graphics card allows, which crashed the window.
#[test]
fn deep_zoom_on_a_long_clip_does_not_crash() {
    let mut e = Editor::default();
    e.run(Command::AddTrack { name: "Track".into() });
    let track = e.session.project().tracks[0].id;
    e.run(Command::AddClip {
        track,
        source: tone(220.0, 1500.0),
        start: 0,
    });
    let clip = e.session.project().tracks[0].clips[0].id;
    let points = vec![audioeditor_engine::GainPoint { at: 0, db: 0.0 }];
    e.run(Command::SetGainCurve { clip, points });
    e.selected = Some(clip);
    e.frames_per_px = 1.0;
    let mut h = harness(e);
    save(&mut h, "deep_zoom");
}

/// The loop end flag on the ruler moves the loop end; audio past it shows again at the start.
#[test]
fn dragging_the_loop_flag_wraps_the_end_onto_the_start() {
    let mut e = Editor::default();
    e.run(Command::AddTrack { name: "Track".into() });
    let track = e.session.project().tracks[0].id;
    // 3 s at 0: x = 126..606 at 50 frames per pixel.
    e.run(Command::AddClip {
        track,
        source: tone(220.0, 3.0),
        start: 0,
    });
    e.run(Command::SetLoopEnd {
        at: Some(3 * RATE as i64),
    });
    e.frames_per_px = 50.0;
    let mut h = harness(e);
    drag(&mut h, egui::pos2(604.0, 50.0), egui::pos2(526.0, 50.0));
    assert_eq!(h.state().session.project().loop_end, Some(20_000));
    save(&mut h, "loop_wrapped");
}

/// Right-clicking a crossfade opens a menu that sets its curve.
#[test]
fn right_click_on_a_crossfade_sets_its_curve() {
    let mut h = harness(overlapping());
    let b = h.state().session.project().tracks[0].clips[1].id;
    let pos = egui::pos2(520.0, 110.0);
    h.hover_at(pos);
    for pressed in [true, false] {
        h.event(Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::NONE,
        });
    }
    h.run();
    save(&mut h, "crossfade_menu");
    h.get_by_label("Linear").click();
    h.run();
    let shape = h.state().session.project().clip(b).map(|(_, c)| c.crossfade_shape);
    assert_eq!(shape, Some(audioeditor_engine::FadeShape::Linear));
    save(&mut h, "crossfade_linear");
}

/// Dragging the line below a track makes it taller, and the crossfade bands with it; no clip moves.
#[test]
fn dragging_the_line_below_a_track_changes_its_height() {
    let mut h = harness_sized(overlapping(), 600.0);
    let starts = |h: &Harness<'_, Editor>| -> Vec<i64> {
        h.state().session.project().tracks[0]
            .clips
            .iter()
            .map(|c| c.start)
            .collect()
    };
    let before = starts(&h);
    drag(&mut h, egui::pos2(500.0, 176.0), egui::pos2(500.0, 386.0));
    let track = h.state().session.project().tracks[0].id;
    let height = h.state().track_heights.get(&track).copied().unwrap_or(0.0);
    assert!((300.0..=330.0).contains(&height), "height {height}");
    assert_eq!(starts(&h), before);
    save(&mut h, "tall_track");
}

/// Regression: after a sample rate change the view was zoomed in by the rate ratio, because zoom is in samples.
#[test]
fn changing_the_sample_rate_keeps_the_view() {
    let mut h = harness(overlapping());
    click(&mut h, 185.0, 20.0);
    h.get_by_label("48 kHz").click();
    h.run();
    let p = h.state().session.project();
    assert_eq!(p.sample_rate, 48_000);
    assert_eq!(p.tracks[0].clips[1].start, 12 * 8000);
    assert_eq!(h.state().frames_per_px, 300.0);
    save(&mut h, "rate_48k");
    h.state_mut().undo();
    assert_eq!(h.state().frames_per_px, 50.0);
}

/// The selected clip has a trash can in its bottom right corner that deletes it.
#[test]
fn the_trash_can_on_a_clip_deletes_it() {
    let mut h = harness(overlapping());
    save(&mut h, "trash_on_selected");
    click(&mut h, 591.0, 160.0);
    let clips = &h.state().session.project().tracks[0].clips;
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].start, 2 * RATE as i64, "the first clip was deleted");
}

fn right_click(h: &mut Harness<'_, Editor>, x: f32, y: f32) {
    let pos = egui::pos2(x, y);
    h.hover_at(pos);
    for pressed in [true, false] {
        h.event(Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::NONE,
        });
    }
    h.run();
}

/// A clip's right-click menu picks its channels; the track header's menu sets them for all clips at once.
#[test]
fn channels_of_a_clip_and_of_a_whole_track() {
    let stereo = |f: f32| {
        let frames = 3 * RATE as usize;
        let ch = |k: f32| {
            (0..frames)
                .map(|i| 0.6 * (i as f32 * f * k * std::f32::consts::TAU / RATE as f32).sin())
                .collect()
        };
        Arc::new(Source {
            path: None,
            name: format!("{f} Hz"),
            sample_rate: RATE,
            channels: vec![ch(1.0), ch(3.0)],
        })
    };
    let mut e = Editor::default();
    e.run(Command::AddTrack { name: "Track".into() });
    let track = e.session.project().tracks[0].id;
    e.run(Command::AddClip {
        track,
        source: stereo(5.0),
        start: 0,
    });
    e.run(Command::AddClip {
        track,
        source: stereo(7.0),
        start: 2 * RATE as i64,
    });
    e.frames_per_px = 50.0;
    let mut h = harness(e);
    let modes = |h: &Harness<'_, Editor>| -> Vec<audioeditor_engine::ChannelMode> {
        h.state().session.project().tracks[0]
            .clips
            .iter()
            .map(|c| c.channels)
            .collect()
    };
    use audioeditor_engine::ChannelMode::{Left, Mono, Stereo};

    right_click(&mut h, 200.0, 110.0);
    h.get_by_label("Left only").click();
    h.run();
    assert_eq!(modes(&h), [Left, Stereo]);
    save(&mut h, "channels_left_only");

    right_click(&mut h, 60.0, 135.0);
    save(&mut h, "channels_track_menu");
    h.get_by_label("Mono (mix of both)").click();
    h.run();
    assert_eq!(modes(&h), [Mono, Mono]);
    h.state_mut().undo();
    assert_eq!(modes(&h), [Left, Stereo], "all clips change in one undo step");
}

/// A dragged clip snaps to the edge of a clip on another track, with a guide line; without snap it stays put.
#[test]
fn a_dragged_clip_snaps_to_other_clip_edges() {
    let setup = || {
        let mut e = Editor::default();
        e.run(Command::AddTrack { name: "A".into() });
        e.run(Command::AddTrack { name: "B".into() });
        let (a, b) = (e.session.project().tracks[0].id, e.session.project().tracks[1].id);
        // A ends at 3 s (x 606); B starts at 3.5 s (x 686).
        e.run(Command::AddClip {
            track: a,
            source: tone(220.0, 3.0),
            start: 0,
        });
        e.run(Command::AddClip {
            track: b,
            source: tone(330.0, 1.0),
            start: 28_000,
        });
        e.frames_per_px = 50.0;
        harness_sized(e, 450.0)
    };
    let b_start = |h: &Harness<'_, Editor>| h.state().session.project().tracks[1].clips[0].start;
    let press = |pos, pressed| Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };

    // Dropped 75 px to the left, B would start 250 frames (5 px) after A's end: close enough to snap.
    let mut h = setup();
    let (from, to) = (egui::pos2(760.0, 230.0), egui::pos2(685.0, 230.0));
    h.hover_at(from);
    h.run();
    h.event(press(from, true));
    for step in 1..=10 {
        h.hover_at(from + (to - from) * (step as f32 / 10.0));
        h.run();
    }
    save(&mut h, "snap_guide");
    h.event(press(to, false));
    h.run();
    assert_eq!(b_start(&h), 24_000);

    let mut h = setup();
    h.state_mut().snap = false;
    drag(&mut h, from, to);
    assert_eq!(b_start(&h), 24_250);
}
