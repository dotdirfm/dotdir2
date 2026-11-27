mod engine;

use crate::archive::VirtualFile;
use anyhow::{Result, bail};
use engine::{Control, HEIGHT, Player, State, WIDTH};
use gpui_kit::{
    component::slider::{Slider, SliderEvent, SliderState},
    prelude::FluentBuilder,
    *,
};
use image::{Frame, RgbaImage};
use smallvec::smallvec;
use std::{path::Path, sync::Arc};

pub struct NativeMedia {
    player: Player,
    view: Entity<MpvView>,
}

impl NativeMedia {
    pub fn new(path: &Path, video: bool, window: &mut Window, cx: &mut App) -> Result<Self> {
        if !video {
            bail!("System AVIF decoding is currently available on macOS");
        }
        Self::from_video(VirtualFile::local(path)?, "", window, cx)
    }

    pub fn from_image(_: Vec<u8>, _: &mut Window, _: &mut App) -> Result<Self> {
        bail!("System AVIF decoding is currently available on macOS")
    }

    pub fn from_video(
        file: VirtualFile,
        _: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Self> {
        let (player, updates) = Player::new(file)?;
        let view = cx.new(|cx| MpvView::new(player.clone(), updates, window, cx));
        Ok(Self { player, view })
    }

    pub fn view(&self) -> Entity<MpvView> {
        self.view.clone()
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.read(cx).focus.clone()
    }

    pub fn toggle_play(&self) {
        self.player.send(Control::Toggle);
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.player.send(Control::Visible(visible));
    }

    pub fn playback_status(&self) -> Option<Result<(), String>> {
        let state = self.player.state();
        if let Some(error) = state.error {
            Some(Err(error))
        } else {
            state.ready.then_some(Ok(()))
        }
    }
}

pub struct MpvView {
    player: Player,
    state: State,
    frame: Option<Arc<RenderImage>>,
    seek: Entity<SliderState>,
    volume: Entity<SliderState>,
    scrubbing: bool,
    focus: FocusHandle,
    _updates: Task<()>,
    _subscriptions: [Subscription; 3],
}

impl MpvView {
    fn new(
        player: Player,
        updates: async_channel::Receiver<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let seek = cx.new(|_| SliderState::new().step(0.1));
        let volume = cx.new(|_| SliderState::new().default_value(100.));
        let subscriptions = [
            cx.subscribe(&seek, |this, _, event, _| match event {
                SliderEvent::Change(_) => this.scrubbing = true,
                SliderEvent::Release(value) => {
                    this.scrubbing = false;
                    this.player.send(Control::Seek(f64::from(value.end())));
                }
            }),
            cx.subscribe(&volume, |this, _, event, _| {
                if let SliderEvent::Release(value) = event {
                    this.player.send(Control::Volume(f64::from(value.end())));
                }
            }),
            cx.on_release(|this, cx| {
                if let Some(frame) = this.frame.take() {
                    cx.drop_image(frame, None);
                }
            }),
        ];
        let task = cx.spawn_in(window, async move |this, cx| {
            while updates.recv().await.is_ok() {
                if this
                    .update_in(cx, |this, window, cx| this.update(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            player,
            state: State::default(),
            frame: None,
            seek,
            volume,
            scrubbing: false,
            focus: cx.focus_handle(),
            _updates: task,
            _subscriptions: subscriptions,
        }
    }

    fn update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (state, pixels) = self.player.take_update();
        if let Some(image) = pixels.and_then(|pixels| RgbaImage::from_raw(WIDTH, HEIGHT, pixels)) {
            let frame = Arc::new(RenderImage::new(smallvec![Frame::new(image)]));
            if let Some(previous) = self.frame.replace(frame) {
                cx.drop_image(previous, Some(window));
            }
        }
        if state.duration > 0. && !self.scrubbing {
            let percent = (100. * state.position / state.duration).clamp(0., 100.) as f32;
            if (self.seek.read(cx).value().end() - percent).abs() >= 0.1 {
                self.seek
                    .update(cx, |slider, cx| slider.set_value(percent, window, cx));
            }
        }
        self.state = state;
        cx.notify();
    }
}

fn timestamp(seconds: f64) -> String {
    let seconds = seconds as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

impl Render for MpvView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let play_label = if self.state.paused { "Play" } else { "Pause" };
        let content = if let Some(error) = &self.state.error {
            div().p(px(24.)).child(error.clone()).into_any_element()
        } else if let Some(frame) = &self.frame {
            img(frame.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element()
        } else if self.state.ready {
            div().into_any_element()
        } else {
            div().child("Loading video…").into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .text_color(rgb(0xffffff))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(content),
            )
            .when(self.state.ready && self.state.error.is_none(), |view| {
                view.child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .gap(px(10.))
                        .px(px(12.))
                        .h(px(34.))
                        .bg(rgb(0x181818))
                        .text_size(px(11.))
                        .child(
                            div()
                                .id("mpv-toggle")
                                .role(Role::Button)
                                .aria_label(play_label)
                                .cursor_pointer()
                                .child(play_label)
                                .on_click(
                                    cx.listener(|this, _, _, _| this.player.send(Control::Toggle)),
                                ),
                        )
                        .child(div().child(timestamp(self.state.position)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Slider::new(&self.seek).disabled(self.state.duration <= 0.)),
                        )
                        .child(div().child(timestamp(self.state.duration)))
                        .child(div().child("Volume"))
                        .child(div().w(px(72.)).child(Slider::new(&self.volume))),
                )
            })
    }
}
