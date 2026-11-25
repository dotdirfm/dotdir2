//! Small platform bridge: AVKit controls and the system AVIF decoder on macOS.
//! No browser or bundled codec library is involved.
#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Context, Result};
    use gpui_kit::{App, NativeView, Window, rgb};
    use objc2::{class, msg_send, rc::Retained, runtime::AnyObject};
    use objc2_core_media::CMTime;
    use objc2_foundation::{NSError, NSString, NSURL};
    use std::{path::Path, ptr::NonNull};

    #[link(name = "AVKit", kind = "framework")]
    unsafe extern "C" {}
    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {}
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    pub struct NativeMedia {
        view: NativeView,
        _object: Retained<AnyObject>,
        player: Option<Retained<AnyObject>>,
        visible: bool,
        resume: bool,
    }
    impl NativeMedia {
        pub fn new(path: &Path, video: bool, window: &mut Window, cx: &mut App) -> Result<Self> {
            let path = NSString::from_str(path.to_str().context("Media path is not valid UTF-8")?);
            let url = NSURL::fileURLWithPath(&path);
            // SAFETY: called on GPUI's main/UI thread. The linked frameworks own
            // these Objective-C classes; every selector and scalar type below
            // matches their public API. Retained keeps objects alive and
            // NativeView owns the NSView attachment until its last clone drops.
            let (object, player) = unsafe {
                if video {
                    let player: Retained<AnyObject> =
                        msg_send![class!(AVPlayer), playerWithURL: &*url];
                    let object: Retained<AnyObject> = msg_send![class!(AVPlayerView), new];
                    let _: () = msg_send![&*object, setPlayer: &*player];
                    let _: () = msg_send![&*object, setControlsStyle: 1isize]; // Inline
                    let _: () = msg_send![&*object, setShowsSharingServiceButton: false];
                    let _: () = msg_send![&*object, setShowsFullScreenToggleButton: false];
                    let _: () = msg_send![&*player, play];
                    (object, Some(player))
                } else {
                    let allocated: objc2::rc::Allocated<AnyObject> =
                        msg_send![class!(NSImage), alloc];
                    let image: Option<Retained<AnyObject>> =
                        msg_send![allocated, initWithContentsOfURL: &*url];
                    let image =
                        image.context("The system image decoder could not open this file")?;
                    let valid: bool = msg_send![&*image, isValid];
                    anyhow::ensure!(valid, "The system image decoder could not decode this file");
                    let object: Retained<AnyObject> = msg_send![class!(NSImageView), new];
                    let _: () = msg_send![&*object, setImage: &*image];
                    let _: () = msg_send![&*object, setImageScaling: 0usize]; // ProportionallyDown
                    let _: () = msg_send![&*object, setImageAlignment: 0usize]; // Center
                    (object, None)
                }
            };
            // SAFETY: object is a retained, newly created NSView which has no parent.
            let view = unsafe { NativeView::new(NonNull::from(&*object).cast(), window, cx) }?;
            view.set_background(rgb(if video { 0x000000 } else { 0x1a1a1a }), cx);
            view.reserve_action::<crate::commands::RunCommand>();
            Ok(Self {
                view,
                _object: object,
                player,
                visible: true,
                resume: false,
            })
        }
        pub fn view(&self) -> NativeView {
            self.view.clone()
        }
        pub fn toggle_play(&self) {
            if let Some(player) = &self.player {
                // SAFETY: the retained AVPlayer is used only on the main thread.
                unsafe {
                    let rate: f32 = msg_send![&**player, rate];
                    if rate == 0. {
                        let item: Option<Retained<AnyObject>> = msg_send![&**player, currentItem];
                        if let Some(item) = item {
                            let current: CMTime = msg_send![&**player, currentTime];
                            let duration: CMTime = msg_send![&*item, duration];
                            let (current, duration) = (current.seconds(), duration.seconds());
                            if duration.is_finite() && duration > 0. && current >= duration - 0.001
                            {
                                let _: () = msg_send![&**player, seekToTime: CMTime::new(0, 1)];
                            }
                        }
                        let _: () = msg_send![&**player, play];
                    } else {
                        let _: () = msg_send![&**player, pause];
                    }
                }
            }
        }
        pub fn set_visible(&mut self, visible: bool) {
            if self.visible == visible {
                return;
            }
            self.visible = visible;
            if let Some(player) = &self.player {
                // SAFETY: as above. A hidden tab must not continue playing audio.
                unsafe {
                    if !visible {
                        let rate: f32 = msg_send![&**player, rate];
                        self.resume = rate != 0.;
                        let _: () = msg_send![&**player, pause];
                    } else if self.resume {
                        let _: () = msg_send![&**player, play];
                        self.resume = false;
                    }
                }
            }
        }
        pub fn playback_status(&self) -> Option<Result<(), String>> {
            let player = self.player.as_ref()?;
            // SAFETY: public AVPlayer/AVPlayerItem accessors on live retained objects.
            unsafe {
                let item: Option<Retained<AnyObject>> = msg_send![&**player, currentItem];
                let item = item?;
                let status: isize = msg_send![&*item, status];
                match status {
                    1 => Some(Ok(())),
                    2 => {
                        let error: Option<Retained<NSError>> = msg_send![&*item, error];
                        Some(Err(error
                            .map(|e| e.localizedDescription().to_string())
                            .unwrap_or_else(|| {
                                "This media format or codec is not supported by macOS".into()
                            })))
                    }
                    _ => None,
                }
            }
        }
    }
    impl Drop for NativeMedia {
        fn drop(&mut self) {
            if let Some(player) = &self.player {
                // SAFETY: retained AVPlayer; teardown takes place on the UI thread.
                unsafe {
                    let _: () = msg_send![&**player, pause];
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use anyhow::{Result, bail};
    use gpui_kit::{App, NativeView, Window};
    use std::path::Path;
    pub struct NativeMedia;
    impl NativeMedia {
        pub fn new(_: &Path, _: bool, _: &mut Window, _: &mut App) -> Result<Self> {
            bail!(
                "Native video and AVIF decoding are currently available on macOS. Use Open externally for this file."
            )
        }
        pub fn view(&self) -> NativeView {
            unreachable!()
        }
        pub fn toggle_play(&self) {}
        pub fn set_visible(&mut self, _: bool) {}
        pub fn playback_status(&self) -> Option<Result<(), String>> {
            None
        }
    }
}
pub use platform::NativeMedia;
