//! Test code: a failed unwrap is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use audioeditor_engine::{
    Command, Fade, FadeShape, GainPoint, Session, Source, export_wav, load_source, open_project, render, save_project,
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
    export_wav(s.project(), path).unwrap();
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
    let points = vec![GainPoint { at: 0, db: -3.0 }, GainPoint { at: 8000, db: -12.5 }];
    s.execute(Command::SetGainCurve { clip: clips[0], points }).unwrap();
    s.execute(Command::SetTrackGain { track: t2, db: -6.0 }).unwrap();
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
    let file = dir.join("song.audioedit");
    save_project(s.project(), &file).unwrap();

    let opened = open_project(&file).unwrap();
    assert_same_sound(&s, &opened);
    let names: Vec<_> = opened.project().tracks.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["Music", "Quiet"]);
    assert!(!opened.can_undo(), "opening is not an undo step");

    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.contains("\"audio/a.wav\""),
        "audio inside the folder is stored relative: {text}"
    );
}

#[test]
fn a_moved_project_folder_still_opens() {
    let dir = folder("moved");
    let s = edited_project(&dir);
    save_project(s.project(), &dir.join("song.audioedit")).unwrap();
    let moved = dir.with_file_name("moved-elsewhere");
    let _ = std::fs::remove_dir_all(&moved);
    std::fs::rename(&dir, &moved).unwrap();

    let opened = open_project(&moved.join("song.audioedit")).unwrap();
    assert_eq!(opened.project().len(), s.project().len());
}

#[test]
fn a_missing_audio_file_is_named() {
    let dir = folder("missing");
    let s = edited_project(&dir);
    let file = dir.join("song.audioedit");
    save_project(s.project(), &file).unwrap();
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
    let err = save_project(s.project(), &dir.join("x.audioedit"))
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
    let file = dir.join("song.audioedit");
    save_project(s.project(), &file).unwrap();
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
        ("newer version", |v| v["version"] = 2.into()),
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
