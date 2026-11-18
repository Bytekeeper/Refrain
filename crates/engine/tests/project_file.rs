//! Test code: a failed unwrap is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use refrain_engine::{
    ChannelMode, Command, Effect, ExportFormat, ExportSettings, ExportTarget, Fade, FadeShape, GainPoint, Generator,
    NoiseColor, Session, Source, export, load_source, open_project, render, save_project,
};

const RATE: u32 = 8000;

/// An empty folder for one test.
fn folder(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("audio")).unwrap();
    dir
}

/// Writes a stereo WAV file with a different tone on each side.
fn write_wav(path: &Path, freq: f32, secs: f32) {
    let frames = (secs * RATE as f32) as usize;
    let tone = |f: f32| {
        (0..frames)
            .map(|i| 0.5 * (i as f32 * f * std::f32::consts::TAU / RATE as f32).sin())
            .collect()
    };
    let source = Arc::new(Source {
        path: None,
        name: "tmp".into(),
        sample_rate: RATE,
        channels: vec![tone(freq), tone(freq * 1.5)],
        generated: None,
    });
    let mut s = Session::default();
    s.execute(Command::AddTrack { name: "t".into() }).unwrap();
    let track = s.project().tracks[0].id;
    s.execute(Command::AddClip {
        track,
        source,
        start: 0,
    })
    .unwrap();
    export(s.project(), &ExportSettings::default(), path).unwrap();
}

/// Two tracks with a crossfade, free fades, a volume curve, a clip before 0 and a muted, quieter track.
fn edited_project(dir: &Path) -> Session {
    write_wav(&dir.join("audio/a.wav"), 220.0, 2.0);
    write_wav(&dir.join("audio/b.wav"), 330.0, 2.0);
    let a = Arc::new(load_source(&dir.join("audio/a.wav")).unwrap());
    let b = Arc::new(load_source(&dir.join("audio/b.wav")).unwrap());
    let mut s = Session::default();
    s.execute(Command::AddTrack { name: "Music".into() }).unwrap();
    s.execute(Command::AddTrack { name: "Quiet".into() }).unwrap();
    let (t1, t2) = (s.project().tracks[0].id, s.project().tracks[1].id);
    s.execute(Command::AddClip {
        track: t1,
        source: a.clone(),
        start: -2000,
    })
    .unwrap();
    s.execute(Command::AddClip {
        track: t1,
        source: b,
        start: 12000,
    })
    .unwrap();
    s.execute(Command::AddClip {
        track: t2,
        source: a,
        start: 4000,
    })
    .unwrap();
    // Generated noise, its start dragged out so it begins before the sound it was first made with.
    s.execute(Command::AddGeneratedClip {
        track: Some(t2),
        generator: Generator::Noise {
            color: NoiseColor::Pink,
            level_db: -18.0,
            wide: true,
            variation: 7,
        },
        start: 24_000,
        len: 8000,
    })
    .unwrap();
    let noise = s.project().tracks[1].clips[1].id;
    s.execute(Command::TrimClip {
        clip: noise,
        start: 22_000,
        end: 32_000,
    })
    .unwrap();
    let clips: Vec<_> = s.project().tracks[0].clips.iter().map(|c| c.id).collect();
    s.execute(Command::SetCrossfadeShape {
        clip: clips[1],
        shape: FadeShape::Linear,
    })
    .unwrap();
    s.execute(Command::SetFadeOut {
        clip: clips[1],
        fade: Fade {
            len: 3000,
            shape: FadeShape::EqualPower,
        },
    })
    .unwrap();
    s.execute(Command::SetChannels {
        clips: vec![clips[1]],
        mode: ChannelMode::Left,
    })
    .unwrap();
    let points = vec![GainPoint { at: 0, db: -3.0 }, GainPoint { at: 8000, db: -12.5 }];
    s.execute(Command::SetGainCurve { clip: clips[0], points }).unwrap();
    s.execute(Command::SetTrackGain { track: t2, db: -6.0 }).unwrap();
    // A track effect across both clips, a switched-off one below it, and one on the mix.
    for (track, peak_db) in [(Some(t1), -1.0), (Some(t1), -20.0), (None, -3.0)] {
        s.execute(Command::AddEffectLane {
            track,
            effect: Effect::Normalize { peak_db },
        })
        .unwrap();
    }
    let lanes: Vec<_> = s.project().tracks[0].effects.iter().map(|l| l.id).collect();
    s.execute(Command::AddEffectRange {
        lane: lanes[0],
        start: 8000,
        end: 20000,
    })
    .unwrap();
    s.execute(Command::AddEffectRange {
        lane: lanes[1],
        start: 0,
        end: 4000,
    })
    .unwrap();
    s.execute(Command::SetEffectEnabled {
        lane: lanes[1],
        enabled: false,
    })
    .unwrap();
    let tail_effects = [
        Effect::LowPass {
            hz: 2000.0,
            steep: true,
        },
        Effect::Echo {
            delay_ms: 120.0,
            fade_db: 8.0,
            level_db: -9.0,
            reversed: false,
        },
        Effect::Reverb {
            seconds: 0.8,
            level_db: -10.0,
            reversed: true,
        },
    ];
    for (i, effect) in tail_effects.into_iter().enumerate() {
        s.execute(Command::AddEffectLane {
            track: Some(t2),
            effect,
        })
        .unwrap();
        let lane = s.project().tracks[1].effects[i].id;
        s.execute(Command::AddEffectRange {
            lane,
            start: 6000,
            end: 14_000,
        })
        .unwrap();
    }
    let mix = s.project().mix_effects[0].id;
    s.execute(Command::AddEffectRange {
        lane: mix,
        start: 0,
        end: 30000,
    })
    .unwrap();
    let range = s.project().mix_effects[0].ranges[0].id;
    s.execute(Command::SetRangeFades {
        range,
        fade_in: 3000,
        fade_out: 500,
    })
    .unwrap();
    s
}

fn assert_same_sound(a: &Session, b: &Session) {
    let len = a.project().len();
    assert_eq!(len, b.project().len());
    let (x, y) = (
        render(a.project(), 0, len as usize),
        render(b.project(), 0, len as usize),
    );
    assert_eq!(x, y);
}

#[test]
fn saved_project_opens_with_the_same_sound() {
    let dir = folder("roundtrip");
    let s = edited_project(&dir);
    let file = dir.join("song.refrain");
    save_project(s.project(), &file, None).unwrap();

    let (opened, _) = open_project(&file).unwrap();
    assert_same_sound(&s, &opened);
    let names: Vec<_> = opened.project().tracks.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["Music", "Quiet"]);
    assert!(!opened.can_undo(), "opening is not an undo step");
    let lanes = &opened.project().tracks[0].effects;
    assert_eq!((lanes.len(), lanes[1].enabled), (2, false));

    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.contains("\"audio/a.wav\""),
        "audio inside the folder is stored relative: {text}"
    );
}

#[test]
fn mono_export_mixes_both_sides_into_one_channel() {
    let dir = folder("mono");
    let source = Arc::new(Source {
        path: None,
        name: "lr".into(),
        sample_rate: RATE,
        channels: vec![vec![0.5; 800], vec![0.25; 800]],
        generated: None,
    });
    let mut s = Session::default();
    s.execute(Command::AddTrack { name: "t".into() }).unwrap();
    let track = s.project().tracks[0].id;
    s.execute(Command::AddClip {
        track,
        source,
        start: 0,
    })
    .unwrap();
    let file = dir.join("mono.wav");
    let mono = ExportSettings {
        mono: true,
        ..ExportSettings::default()
    };
    export(s.project(), &mono, &file).unwrap();
    let back = load_source(&file).unwrap();
    assert_eq!(back.channels.len(), 1);
    assert!(
        (back.channels[0][400] - 0.375).abs() < 1e-4,
        "{}",
        back.channels[0][400]
    );
}

#[test]
fn a_moved_project_folder_still_opens() {
    let dir = folder("moved");
    let s = edited_project(&dir);
    save_project(s.project(), &dir.join("song.refrain"), None).unwrap();
    let moved = dir.with_file_name("moved-elsewhere");
    let _ = std::fs::remove_dir_all(&moved);
    std::fs::rename(&dir, &moved).unwrap();

    let (opened, _) = open_project(&moved.join("song.refrain")).unwrap();
    assert_eq!(opened.project().len(), s.project().len());
}

#[test]
fn a_missing_audio_file_is_named() {
    let dir = folder("missing");
    let s = edited_project(&dir);
    let file = dir.join("song.refrain");
    save_project(s.project(), &file, None).unwrap();
    std::fs::remove_file(dir.join("audio/b.wav")).unwrap();

    let err = open_project(&file).unwrap_err().to_string();
    assert!(err.contains("b.wav"), "{err}");
}

#[test]
fn audio_made_in_memory_cannot_be_saved() {
    let dir = folder("memory");
    let source = Arc::new(Source {
        path: None,
        name: "beep".into(),
        sample_rate: RATE,
        channels: vec![vec![0.1; 100]],
        generated: None,
    });
    let mut s = Session::default();
    s.execute(Command::AddTrack { name: "t".into() }).unwrap();
    let track = s.project().tracks[0].id;
    s.execute(Command::AddClip {
        track,
        source,
        start: 0,
    })
    .unwrap();
    let err = save_project(s.project(), &dir.join("x.refrain"), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("beep"), "{err}");
}

/// Changes one thing in a saved project file.
type Damage = fn(&mut serde_json::Value);

/// Broken or hand-edited files give an error, never a panic or a half-loaded project.
#[test]
fn damaged_files_are_refused() {
    let dir = folder("damaged");
    let s = edited_project(&dir);
    let file = dir.join("song.refrain");
    save_project(s.project(), &file, None).unwrap();
    let good: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();

    let cases: Vec<(&str, Damage)> = vec![
        ("negative length", |v| v["tracks"][0]["clips"][0]["len"] = (-5).into()),
        ("past the audio end", |v| {
            v["tracks"][0]["clips"][0]["offset"] = 15_999.into()
        }),
        ("huge numbers", |v| {
            v["tracks"][0]["clips"][0]["start"] = i64::MAX.into()
        }),
        ("unknown source", |v| v["tracks"][0]["clips"][0]["source"] = 7.into()),
        ("fade too long", |v| {
            v["tracks"][0]["clips"][1]["fade_out"]["len"] = 1_000_000.into()
        }),
        ("a clip twice on one track", |v| {
            let clip = v["tracks"][0]["clips"][1].clone();
            v["tracks"][0]["clips"].as_array_mut().unwrap().push(clip);
        }),
        ("newer version", |v| v["version"] = 3.into()),
        ("loop end before 0", |v| v["loop_end"] = (-1).into()),
        ("clip after the loop end", |v| v["loop_end"] = 100.into()),
        ("wrong type", |v| v["tracks"] = "none".into()),
    ];
    for (name, damage) in cases {
        let mut v = good.clone();
        damage(&mut v);
        std::fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
        let err = open_project(&file)
            .err()
            .unwrap_or_else(|| panic!("{name} was accepted"));
        println!("{name}: {err}");
    }
    for garbage in [&b""[..], b"{", b"\xff\xfe", b"[1,2,3]"] {
        std::fs::write(&file, garbage).unwrap();
        assert!(open_project(&file).is_err());
    }
}

/// Each format reads back at the rate it promises and with the exact length, except MP3. MP3 has no 96 kHz, so it is
/// written at 48 kHz.
#[test]
fn every_export_format_reads_back() {
    let dir = folder("formats");
    let rate = 96_000;
    let tone = (0..rate)
        .map(|i| 0.5 * (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin())
        .collect::<Vec<f32>>();
    let source = Arc::new(Source {
        path: None,
        name: "tone".into(),
        sample_rate: rate,
        channels: vec![tone.clone(), tone],
        generated: None,
    });
    let mut s = Session::default();
    s.execute(Command::AddTrack { name: "t".into() }).unwrap();
    let track = s.project().tracks[0].id;
    s.execute(Command::AddClip {
        track,
        source,
        start: 0,
    })
    .unwrap();
    for format in ExportFormat::ALL {
        let file = dir.join(format!("mix.{}", format.extension()));
        let settings = ExportSettings {
            format,
            bits: 16,
            kbps: 160,
            ..ExportSettings::default()
        };
        let written = export(s.project(), &settings, &file).unwrap();
        let back = load_source(&file).unwrap();
        let expected = if format == ExportFormat::Mp3 { 48_000 } else { rate };
        assert_eq!((written.rate, back.sample_rate), (expected, expected), "{format:?}");
        assert_eq!(back.channels.len(), 2, "{format:?}");
        if format == ExportFormat::Mp3 {
            // The silence MP3 adds at the ends: why a loop exported as MP3 gets a gap.
            assert!(back.frames() > 48_000, "{}", back.frames());
        } else {
            assert_eq!(back.frames(), i64::from(rate), "{format:?}");
        }
    }
}

/// After the file of some audio is overwritten and the original kept elsewhere, a rate change, undo and saving
/// all use the kept original; audio read afresh from the overwritten path is the new audio.
#[test]
fn relocated_audio_is_read_from_its_new_place() {
    let dir = folder("relocated");
    let file = dir.join("audio/a.wav");
    let kept = dir.join("kept.wav");
    write_wav(&file, 220.0, 1.0);
    let session = |source| {
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        s.execute(Command::AddClip {
            track,
            source,
            start: 0,
        })
        .unwrap();
        s
    };
    let mut untouched = session(Arc::new(load_source(&file).unwrap()));
    let mut s = session(Arc::new(load_source(&file).unwrap()));

    untouched.execute(Command::SetSampleRate { rate: 16_000 }).unwrap();
    std::fs::copy(&file, &kept).unwrap();
    write_wav(&file, 330.0, 0.5);
    s.relocate(&file, &kept);
    let fresh = Arc::new(load_source(&file).unwrap());
    assert_eq!(s.project().file_of(&fresh), Some(file.as_path()));

    s.execute(Command::SetSampleRate { rate: 16_000 }).unwrap();
    assert_eq!(render(s.project(), 0, 16_000), render(untouched.project(), 0, 16_000));
    s.undo().unwrap();
    let source = &s.project().tracks[0].clips[0].source;
    assert_eq!(s.project().file_of(source), Some(kept.as_path()));

    save_project(s.project(), &dir.join("song.refrain"), None).unwrap();
    let text = std::fs::read_to_string(dir.join("song.refrain")).unwrap();
    assert!(text.contains("kept.wav") && !text.contains("a.wav"), "{text}");
}

/// Files saved before ranges had their own settings kept them in the lane.
#[test]
fn ranges_in_older_files_take_the_lane_settings() {
    let dir = folder("older");
    let s = edited_project(&dir);
    let file = dir.join("song.refrain");
    save_project(s.project(), &file, None).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let mut removed = 0;
    for track in json["tracks"].as_array_mut().unwrap() {
        for lane in track["effects"].as_array_mut().unwrap() {
            for range in lane["ranges"].as_array_mut().unwrap() {
                let range = range.as_object_mut().unwrap();
                removed += usize::from(range.remove("effect").is_some());
                range.remove("fade_in");
                range.remove("fade_out");
            }
        }
    }
    assert!(removed > 0);
    std::fs::write(&file, json.to_string()).unwrap();

    let (opened, _) = open_project(&file).unwrap();
    for lane in opened.project().tracks.iter().flat_map(|t| &t.effects) {
        assert!(lane.ranges.iter().all(|r| r.effect == lane.effect));
        // The fade every range had before fades could be set: 10 ms.
        let fade = i64::from(opened.project().sample_rate) / 100;
        assert!(lane.ranges.iter().all(|r| (r.fade_in, r.fade_out) == (fade, fade)));
    }
    assert_same_sound(&s, &opened);
}

/// Ranges in version 1 files ended where the effect stopped hearing; now they hold the tail too, and ranges
/// whose tails crossed go to a lane of their own.
#[test]
fn older_ranges_grow_by_their_tail() {
    let dir = folder("older_tails");
    let s = edited_project(&dir);
    let file = dir.join("song.refrain");
    save_project(s.project(), &file, None).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let echo = serde_json::json!({"kind": "echo", "delay_ms": 100.0, "fade_db": 6.0, "level_db": -6.0, "reversed": false});
    let range = |start: i64, end: i64| serde_json::json!({"start": start, "end": end, "effect": echo, "fade_in": 0, "fade_out": 0});
    json["version"] = 1.into();
    json["tracks"][0]["effects"] = serde_json::json!([{
        "effect": echo,
        "enabled": true,
        "ranges": [range(0, 1000), range(2000, 3000), range(200_000, 201_000)],
    }]);
    std::fs::write(&file, json.to_string()).unwrap();

    let (opened, _) = open_project(&file).unwrap();
    let p = opened.project();
    let lanes = &p.tracks[0].effects;
    let tail = lanes[0].ranges[0].effect.tail(p.sample_rate);
    let spans = |lane: usize| lanes[lane].ranges.iter().map(|r| (r.start, r.end)).collect::<Vec<_>>();
    assert_eq!(lanes.len(), 2);
    assert_eq!(spans(0), [(0, 1000 + tail), (200_000, 201_000 + tail)]);
    assert_eq!(spans(1), [(2000, 3000 + tail)]);
}

/// What the window does around exporting and saving, done by the engine alone, as another program would.
#[test]
fn exporting_over_used_audio_keeps_it_and_saving_takes_it_along() {
    let dir = folder("export_keeps");
    let mut s = edited_project(&dir);
    let audio = dir.join("audio/a.wav");
    let before = render(s.project(), 0, 30_000);
    let target = ExportTarget {
        file: Some(audio.clone()),
        settings: ExportSettings::default(),
    };
    s.execute(Command::SetExportTarget { target }).unwrap();

    let kept_dir = dir.join("kept");
    let done = s.export(|name| Some(kept_dir.join(format!("x-{name}")))).unwrap();
    assert!(done.kept_original);
    assert_eq!(
        render(s.project(), 0, 30_000),
        before,
        "the clips still play the original"
    );
    assert!(
        s.export(|_| None).is_ok_and(|d| !d.kept_original),
        "the project no longer reads it"
    );

    let file = dir.join("song.refrain");
    s.save(&file, None, Some(&kept_dir)).unwrap();
    assert!(dir.join("originals/a.wav").exists(), "under the name it had");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.contains("\"originals/a.wav\"") && text.contains("\"file\": \"audio/a.wav\""),
        "{text}"
    );
    let (opened, _) = open_project(&file).unwrap();
    assert_eq!(opened.project().export.file.as_deref(), Some(audio.as_path()));
    assert_eq!(render(opened.project(), 0, 30_000), before);
}

/// Files saved before the export target was part of the project kept it with the window's view.
#[test]
fn an_export_target_in_an_older_view_is_read() {
    let dir = folder("older_export");
    let s = edited_project(&dir);
    let file = dir.join("song.refrain");
    save_project(s.project(), &file, None).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let object = json.as_object_mut().unwrap();
    object.remove("export");
    object.insert(
        "view".into(),
        serde_json::json!({"export": {"file": "mix.ogg", "settings": {
            "format": "ogg", "bits": 16, "kbps": 160, "mono": true, "sample_rate": null}}}),
    );
    std::fs::write(&file, json.to_string()).unwrap();
    let (opened, _) = open_project(&file).unwrap();
    let export = &opened.project().export;
    assert_eq!(export.file.as_deref(), Some(dir.join("mix.ogg").as_path()));
    assert!(export.settings.mono);
}

/// The level row's peaks are those of one render of the whole mix, effects and all.
#[test]
fn an_export_reports_the_samples_over_0_db() {
    let dir = folder("overs");
    let mut s = edited_project(&dir);
    let track = s.project().tracks[0].id;
    s.execute(Command::SetTrackGain { track, db: 12.0 }).unwrap();
    let [left, right] = render(s.project(), 0, s.project().len() as usize);
    let over = left.iter().chain(&right).filter(|v| v.abs() > 1.0).count();
    let peak = left.iter().chain(&right).fold(0.0f32, |m, v| m.max(v.abs()));
    let written = export(s.project(), &ExportSettings::default(), &dir.join("loud.wav")).unwrap();
    assert!(over > 0);
    assert_eq!((written.over, written.peak), (over, peak));
    let at = written.peak_at as usize;
    assert!(left[at].abs() == peak || right[at].abs() == peak);
}
