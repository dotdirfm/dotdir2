//! Platform media backends. macOS uses AVKit and the system AVIF decoder.

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub use crate::media_mpv::NativeMedia;
#[cfg(target_os = "macos")]
mod platform {
    use crate::archive::VirtualFile;
    use anyhow::{Context, Result, ensure};
    use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
    use dotdir_core::copy::CancelToken;
    use gpui_kit::{App, FocusHandle, Focusable, NativeView, Window, rgb};
    use objc2::{
        AnyThread, DefinedClass, MainThreadMarker, class, define_class, msg_send,
        rc::Retained,
        runtime::{AnyObject, ProtocolObject},
    };
    use objc2_av_foundation::{
        AVAssetResourceLoader, AVAssetResourceLoaderDelegate, AVAssetResourceLoadingRequest,
        AVPlayerItem, AVURLAsset,
    };
    use objc2_core_media::CMTime;
    use objc2_foundation::{NSData, NSError, NSObject, NSObjectProtocol, NSString, NSURL};
    use objc2_uniform_type_identifiers::UTType;
    use std::{
        io::{Read, Seek, SeekFrom},
        path::Path,
        ptr::NonNull,
    };

    #[link(name = "AVKit", kind = "framework")]
    unsafe extern "C" {}
    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {}
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    struct VideoSource {
        file: VirtualFile,
        content_type: Retained<NSString>,
        cancel: CancelToken,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[ivars = VideoSource]
        struct ResourceDelegate;

        unsafe impl NSObjectProtocol for ResourceDelegate {}

        unsafe impl AVAssetResourceLoaderDelegate for ResourceDelegate {
            #[unsafe(method(resourceLoader:shouldWaitForLoadingOfRequestedResource:))]
            fn load_resource(
                &self,
                _: &AVAssetResourceLoader,
                request: &AVAssetResourceLoadingRequest,
            ) -> bool {
                // SAFETY: AVFoundation calls this with live request objects on the serial
                // resource queue. All data returned to it is owned by immutable NSData.
                unsafe {
                    let result = self.ivars().respond(request);
                    if !request.isCancelled() {
                        if result.is_ok() {
                            request.finishLoading();
                        } else {
                            let error = NSError::errorWithDomain_code_userInfo(
                                &NSString::from_str("dev.dotdir.native.media"),
                                1,
                                None,
                            );
                            request.finishLoadingWithError(Some(&error));
                        }
                    }
                }
                true
            }
        }
    );

    impl VideoSource {
        unsafe fn respond(&self, request: &AVAssetResourceLoadingRequest) -> Result<()> {
            // SAFETY: the resource loader owns the request, and response calls
            // are made on its delegate queue before the request is finished.
            unsafe {
                let length = self.file.len();
                if let Some(info) = request.contentInformationRequest() {
                    if let Some(allowed) = info.allowedContentTypes() {
                        ensure!(
                            allowed.is_empty() || allowed.containsObject(&self.content_type),
                            "AVFoundation does not accept the video's content type"
                        );
                    }
                    info.setContentType(Some(&self.content_type));
                    info.setContentLength(i64::try_from(length)?);
                    info.setByteRangeAccessSupported(true);
                }
                if let Some(data) = request.dataRequest() {
                    let start = u64::try_from(data.requestedOffset())?;
                    let offset = u64::try_from(data.currentOffset())?.max(start);
                    let end = if data.requestsAllDataToEndOfResource() {
                        length
                    } else {
                        start
                            .saturating_add(u64::try_from(data.requestedLength())?)
                            .min(length)
                    };
                    let mut remaining = end.saturating_sub(offset);
                    let mut file = self.file.clone();
                    file.seek(SeekFrom::Start(offset))?;
                    let mut buffer = [0; 128 * 1024];
                    while remaining > 0 {
                        if request.isCancelled() {
                            return Ok(());
                        }
                        ensure!(!self.cancel.is_cancelled(), "Media closed");
                        let count = remaining.min(buffer.len() as u64) as usize;
                        file.read_exact(&mut buffer[..count])?;
                        data.respondWithData(&NSData::with_bytes(&buffer[..count]));
                        remaining -= count as u64;
                    }
                }
            }
            Ok(())
        }
    }

    struct VideoResource {
        delegate: Retained<ResourceDelegate>,
        _queue: DispatchRetained<DispatchQueue>,
    }

    pub struct NativeMedia {
        view: NativeView,
        _object: Retained<AnyObject>,
        player: Option<Retained<AnyObject>>,
        resource: Option<VideoResource>,
        visible: bool,
        resume: bool,
    }
    impl NativeMedia {
        pub fn new(path: &Path, video: bool, window: &mut Window, cx: &mut App) -> Result<Self> {
            let path = NSString::from_str(path.to_str().context("Media path is not valid UTF-8")?);
            let url = NSURL::fileURLWithPath(&path);
            // SAFETY: these framework constructors are called on the UI thread,
            // with retained arguments of the types expected by the selectors.
            unsafe {
                if video {
                    let player = msg_send![class!(AVPlayer), playerWithURL: &*url];
                    Self::video(player, None, window, cx)
                } else {
                    let allocated: objc2::rc::Allocated<AnyObject> =
                        msg_send![class!(NSImage), alloc];
                    let image = msg_send![allocated, initWithContentsOfURL: &*url];
                    Self::image(image, window, cx)
                }
            }
        }

        pub fn from_image(bytes: Vec<u8>, window: &mut Window, cx: &mut App) -> Result<Self> {
            let data = NSData::from_vec(bytes);
            // SAFETY: AppKit receives retained immutable image data on the UI thread.
            unsafe {
                let allocated: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSImage), alloc];
                let image = msg_send![allocated, initWithData: &*data];
                Self::image(image, window, cx)
            }
        }

        pub fn from_video(
            file: VirtualFile,
            extension: &str,
            window: &mut Window,
            cx: &mut App,
        ) -> Result<Self> {
            let mtm = MainThreadMarker::new().context("Video must be opened on the UI thread")?;
            ensure!(
                extension.bytes().all(|byte| byte.is_ascii_alphanumeric()),
                "Invalid video extension"
            );
            let url = NSURL::URLWithString(&NSString::from_str(&format!(
                "dotdir-media://file/video.{extension}"
            )))
            .context("Could not create a URL for the video")?;
            let content_type = UTType::typeWithFilenameExtension(&NSString::from_str(extension))
                .context("Unknown video content type")?
                .identifier();
            let delegate = ResourceDelegate::alloc().set_ivars(VideoSource {
                file,
                content_type,
                cancel: CancelToken::new(),
            });
            // SAFETY: the asset uses a private scheme serviced by a retained
            // delegate. AVFoundation invokes it on a dedicated serial queue.
            unsafe {
                let delegate: Retained<ResourceDelegate> = msg_send![super(delegate), init];
                let queue =
                    DispatchQueue::new("dev.dotdir.native.media", DispatchQueueAttr::SERIAL);
                let asset = AVURLAsset::URLAssetWithURL_options(&url, None);
                asset
                    .resourceLoader()
                    .setDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
                let item = AVPlayerItem::playerItemWithAsset(&asset, mtm);
                let player: Retained<AnyObject> =
                    msg_send![class!(AVPlayer), playerWithPlayerItem: &*item];
                let _: () = msg_send![&*player, setAutomaticallyWaitsToMinimizeStalling: false];
                Self::video(
                    player,
                    Some(VideoResource {
                        delegate,
                        _queue: queue,
                    }),
                    window,
                    cx,
                )
            }
        }

        fn video(
            player: Retained<AnyObject>,
            resource: Option<VideoResource>,
            window: &mut Window,
            cx: &mut App,
        ) -> Result<Self> {
            // SAFETY: player is an AVPlayer and the view is created and attached on the UI thread.
            let object = unsafe {
                let object: Retained<AnyObject> = msg_send![class!(AVPlayerView), new];
                let _: () = msg_send![&*object, setPlayer: &*player];
                let _: () = msg_send![&*object, setControlsStyle: 1isize]; // Inline
                let _: () = msg_send![&*object, setShowsSharingServiceButton: false];
                let _: () = msg_send![&*object, setShowsFullScreenToggleButton: false];
                let _: () = msg_send![&*player, play];
                object
            };
            Self::attach(object, Some(player), resource, window, cx)
        }

        fn image(
            image: Option<Retained<AnyObject>>,
            window: &mut Window,
            cx: &mut App,
        ) -> Result<Self> {
            let image = image.context("The system image decoder could not open this file")?;
            // SAFETY: image is an NSImage initialized from file or immutable bytes.
            let object = unsafe {
                let valid: bool = msg_send![&*image, isValid];
                ensure!(valid, "The system image decoder could not decode this file");
                let object: Retained<AnyObject> = msg_send![class!(NSImageView), new];
                let _: () = msg_send![&*object, setImage: &*image];
                let _: () = msg_send![&*object, setImageScaling: 0usize]; // ProportionallyDown
                let _: () = msg_send![&*object, setImageAlignment: 0usize]; // Center
                object
            };
            Self::attach(object, None, None, window, cx)
        }

        fn attach(
            object: Retained<AnyObject>,
            player: Option<Retained<AnyObject>>,
            resource: Option<VideoResource>,
            window: &mut Window,
            cx: &mut App,
        ) -> Result<Self> {
            // SAFETY: object is a retained, newly created NSView which has no parent.
            let view = unsafe { NativeView::new(NonNull::from(&*object).cast(), window, cx) }?;
            view.set_background(rgb(if player.is_some() { 0x000000 } else { 0x1a1a1a }), cx);
            view.reserve_action::<crate::commands::RunCommand>();
            Ok(Self {
                view,
                _object: object,
                player,
                resource,
                visible: true,
                resume: false,
            })
        }
        pub fn view(&self) -> NativeView {
            self.view.clone()
        }
        pub fn focus_handle(&self, cx: &App) -> FocusHandle {
            self.view.focus_handle(cx)
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
            if let Some(resource) = &self.resource {
                resource.delegate.ivars().cancel.cancel();
            }
            if let Some(player) = &self.player {
                // SAFETY: retained AVPlayer; teardown takes place on the UI thread.
                unsafe {
                    let _: () = msg_send![&**player, pause];
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod platform {
    use crate::archive::VirtualFile;
    use anyhow::{Result, bail};
    use gpui_kit::{App, FocusHandle, NativeView, Window};
    use std::path::Path;
    pub struct NativeMedia;
    impl NativeMedia {
        pub fn new(_: &Path, _: bool, _: &mut Window, _: &mut App) -> Result<Self> {
            bail!(
                "Native video and AVIF decoding are currently available on macOS. Use Open externally for this file."
            )
        }
        pub fn from_image(_: Vec<u8>, _: &mut Window, _: &mut App) -> Result<Self> {
            bail!("Native AVIF decoding is currently available on macOS")
        }
        pub fn from_video(_: VirtualFile, _: &str, _: &mut Window, _: &mut App) -> Result<Self> {
            bail!("Native video playback is currently available on macOS")
        }
        pub fn view(&self) -> NativeView {
            unreachable!()
        }
        pub fn focus_handle(&self, _: &App) -> FocusHandle {
            unreachable!()
        }
        pub fn toggle_play(&self) {}
        pub fn set_visible(&mut self, _: bool) {}
        pub fn playback_status(&self) -> Option<Result<(), String>> {
            None
        }
    }
}
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub use platform::NativeMedia;
