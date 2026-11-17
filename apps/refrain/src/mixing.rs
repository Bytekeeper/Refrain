//! The mixdown of the open project: the whole project as heard, made in the background after each edit, so
//! editing never waits for it. Playback and the Mix level row use the latest one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use refrain_engine::{Gain, Levels, Project, RangeId};

/// What the window shows of the latest mixdown, and the project it was made from.
pub(crate) struct Mixed {
    pub(crate) project: Arc<Project>,
    pub(crate) levels: Levels,
    pub(crate) gains: HashMap<RangeId, Gain>,
}

#[derive(Default)]
pub(crate) struct Mixer {
    pub(crate) latest: Option<Mixed>,
    working: Option<Working>,
}

type Mixdown = (Arc<[Vec<f32>; 2]>, Levels, HashMap<RangeId, Gain>);

/// A mixdown being made in the background.
struct Working {
    project: Arc<Project>,
    stop: Arc<AtomicBool>,
    arrives: mpsc::Receiver<Option<Mixdown>>,
}

fn make(project: &Project, keep_going: impl Fn() -> bool) -> Option<Mixdown> {
    let mixdown = refrain_engine::mixdown(project, keep_going)?;
    let levels = Levels::of(&mixdown.sound);
    Some((Arc::new(mixdown.sound), levels, mixdown.gains))
}

impl crate::Editor {
    /// Hands a finished mixdown to playback, and starts one when the project changed since the last.
    pub(crate) fn mix(&mut self, ctx: &egui::Context) {
        let project = self.session.project().clone();
        if let Some(w) = &self.mixer.working {
            match w.arrives.try_recv() {
                Ok(Some(mixdown)) => {
                    let made_from = w.project.clone();
                    self.mixer.working = None;
                    self.use_mixdown(made_from, mixdown);
                }
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => self.mixer.working = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let current = |p: &Arc<Project>| Arc::ptr_eq(p, &project);
        if self.mixer.latest.as_ref().is_some_and(|m| current(&m.project))
            || self.mixer.working.as_ref().is_some_and(|w| current(&w.project))
        {
            return;
        }
        if let Some(w) = self.mixer.working.take() {
            w.stop.store(true, Ordering::Relaxed);
        }
        // Tests mix at once, so every frame they look at is complete and the same each run.
        if cfg!(test) {
            if let Some(mixdown) = make(&project, || true) {
                self.use_mixdown(project, mixdown);
            }
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (send, arrives) = mpsc::channel();
        let (mixed, stopped, ctx) = (project.clone(), stop.clone(), ctx.clone());
        let started = std::thread::Builder::new().name("mixdown".into()).spawn(move || {
            let _ = send.send(make(&mixed, || !stopped.load(Ordering::Relaxed)));
            ctx.request_repaint();
        });
        if started.is_ok() {
            self.mixer.working = Some(Working { project, stop, arrives });
        }
    }

    fn use_mixdown(&mut self, project: Arc<Project>, (sound, levels, gains): Mixdown) {
        if let Err(e) = self.player.set_mix(&project, sound) {
            self.fail(e);
        }
        self.mixer.latest = Some(Mixed { project, levels, gains });
    }
}
