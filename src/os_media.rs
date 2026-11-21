//! Native desktop media controls.
//!
//! Linux/BSD uses MPRIS through `mpris-server`. macOS exposes the track in Now
//! Playing and receives `MPRemoteCommandCenter` events through `souvlaki`.
//! Windows uses System Media Transport Controls through the same crate. SMTC
//! requires an HWND, so termify creates a tiny hidden window on the media
//! thread; it is never shown and has no interaction with the TUI.

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub use crate::mpris::MprisHandle as MediaControlsHandle;

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub fn spawn(
    action_tx: tokio::sync::mpsc::Sender<crate::ui::Action>,
) -> anyhow::Result<Option<MediaControlsHandle>> {
    crate::mpris::spawn(action_tx)
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod native {
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[cfg(target_os = "windows")]
    use anyhow::bail;
    use anyhow::{Context, Result};
    use souvlaki::{
        MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition,
        PlatformConfig, SeekDirection,
    };
    use tokio::sync::{mpsc, watch};

    use crate::mpris::{MprisState, track_id};
    use crate::ui::Action;

    /// Cloneable handle used by the main loop to publish state snapshots.
    #[derive(Debug, Clone)]
    pub struct MediaControlsHandle {
        tx: watch::Sender<MprisState>,
    }

    impl MediaControlsHandle {
        pub fn publish(&self, state: MprisState) {
            let _ = self.tx.send(state);
        }
    }

    /// Start the platform media-control backend on a dedicated thread.
    pub fn spawn(action_tx: mpsc::Sender<Action>) -> Result<Option<MediaControlsHandle>> {
        let (state_tx, state_rx) = watch::channel(MprisState::empty());
        thread::Builder::new()
            .name("termify-media-controls".to_string())
            .spawn(move || {
                if let Err(err) = run(action_tx, state_rx) {
                    tracing::warn!("Native media controls stopped: {err:#}");
                }
            })
            .context("spawning native media controls thread")?;
        Ok(Some(MediaControlsHandle { tx: state_tx }))
    }

    fn run(
        action_tx: mpsc::Sender<Action>,
        mut state_rx: watch::Receiver<MprisState>,
    ) -> Result<()> {
        let current_track_id = Arc::new(Mutex::new(String::new()));
        let mut controls = create_controls()?;

        {
            let current_track_id = current_track_id.clone();
            controls
                .attach(move |event| {
                    if let Some(action) = event_action(event, &current_track_id) {
                        let _ = action_tx.try_send(action);
                    }
                })
                .context("attaching native media control events")?;
        }

        #[cfg(target_os = "macos")]
        controls
            .set_shuffle(Some(false))
            .context("publishing initial shuffle state")?;

        let mut last: Option<MprisState> = None;
        loop {
            pump_messages();
            match state_rx.has_changed() {
                Ok(false) => {}
                Ok(true) => {
                    let state = state_rx.borrow_and_update().clone();
                    apply_state(&mut controls, &state, last.as_ref(), &current_track_id)?;
                    last = Some(state);
                }
                Err(_) => break, // the main loop dropped the handle
            }
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    fn apply_state(
        controls: &mut MediaControls,
        state: &MprisState,
        last: Option<&MprisState>,
        current_track_id: &Mutex<String>,
    ) -> Result<()> {
        let metadata_changed = last.is_none_or(|prev| prev.track != state.track);
        if metadata_changed {
            *current_track_id.lock().unwrap() = state
                .track
                .as_ref()
                .map_or_else(String::new, |track| track_id(&track.uri));
            let metadata = match &state.track {
                Some(track) => MediaMetadata {
                    title: Some(&track.title),
                    album: if track.album.is_empty() {
                        None
                    } else {
                        Some(&track.album)
                    },
                    artist: Some(&track.artist),
                    cover_url: track.art_url.as_deref(),
                    duration: Some(track.duration),
                },
                None => MediaMetadata::default(),
            };
            controls.set_metadata(metadata)?;
        }

        let playback_changed = last.is_none_or(|prev| {
            prev.playing != state.playing
                || prev.position != state.position
                || prev.track.is_none() != state.track.is_none()
        });
        if playback_changed {
            let progress = state.track.as_ref().map(|_| MediaPosition(state.position));
            let playback = if !state.playing && state.track.is_none() {
                MediaPlayback::Stopped
            } else if state.playing {
                MediaPlayback::Playing { progress }
            } else {
                MediaPlayback::Paused { progress }
            };
            controls.set_playback(playback)?;
        }

        #[cfg(target_os = "macos")]
        if last.is_none_or(|prev| prev.shuffle != state.shuffle) {
            controls.set_shuffle(Some(state.shuffle))?;
        }

        Ok(())
    }

    fn event_action(event: MediaControlEvent, current_track_id: &Mutex<String>) -> Option<Action> {
        match event {
            MediaControlEvent::Play => Some(Action::MprisPlay),
            MediaControlEvent::Pause => Some(Action::MprisPause),
            MediaControlEvent::Toggle => Some(Action::MprisPlayPause),
            MediaControlEvent::Next => Some(Action::MprisNext),
            MediaControlEvent::Previous => Some(Action::MprisPrev),
            MediaControlEvent::Stop => Some(Action::MprisPause),
            MediaControlEvent::Seek(direction) => Some(Action::MprisSeek(signed_millis(
                direction,
                Duration::from_secs(10),
            ))),
            MediaControlEvent::SeekBy(direction, duration) => {
                Some(Action::MprisSeek(signed_millis(direction, duration)))
            }
            MediaControlEvent::SetPosition(position) => Some(Action::MprisSetPosition {
                track: current_track_id.lock().unwrap().clone(),
                position_ms: u64_millis(position.0),
            }),
            MediaControlEvent::SetVolume(volume) => {
                Some(Action::MprisSetVolume(volume.clamp(0.0, 1.0)))
            }
            MediaControlEvent::SetShuffle(shuffle) => Some(Action::MprisSetShuffle(shuffle)),
            MediaControlEvent::OpenUri(uri) => Some(Action::MprisOpenUri(uri)),
            MediaControlEvent::Quit => Some(Action::MprisQuit),
            // A terminal client has no window to raise.
            MediaControlEvent::Raise => None,
            _ => None,
        }
    }

    fn signed_millis(direction: SeekDirection, duration: Duration) -> i64 {
        let millis = duration.as_millis().min(i64::MAX as u128) as i64;
        match direction {
            SeekDirection::Forward => millis,
            SeekDirection::Backward => -millis,
        }
    }

    fn u64_millis(duration: Duration) -> u64 {
        duration.as_millis().min(u64::MAX as u128) as u64
    }

    #[cfg(target_os = "macos")]
    fn create_controls() -> Result<MediaControls> {
        MediaControls::new(PlatformConfig {
            display_name: "termify",
            dbus_name: "termify",
            hwnd: None,
        })
        .context("creating macOS media controls")
    }

    #[cfg(target_os = "macos")]
    fn pump_messages() {}

    #[cfg(target_os = "windows")]
    fn create_controls() -> Result<MediaControls> {
        use std::ffi::c_void;

        use windows::Win32::Foundation::HINSTANCE;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CreateWindowExW, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE,
            WNDCLASSW,
        };
        use windows::core::PCWSTR;

        // SMTC binds to a window. Register a private, invisible class on this
        // thread and keep pumping its message queue while media controls live.
        let class_name: Vec<u16> = "TERMIFY_MEDIA_CONTROLS\0".encode_utf16().collect();
        let class = PCWSTR::from_raw(class_name.as_ptr());
        unsafe {
            let module = GetModuleHandleW(None).context("getting termify module handle")?;
            let instance = HINSTANCE(module.0);
            let window_class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance,
                lpszClassName: class,
                ..Default::default()
            };
            if RegisterClassW(&window_class) == 0 {
                bail!("could not register the hidden media controls window class");
            }
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                class,
                WINDOW_STYLE::default(),
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )
            .context("creating hidden media controls window")?;

            MediaControls::new(PlatformConfig {
                display_name: "termify",
                dbus_name: "termify",
                hwnd: Some(hwnd.0 as *mut c_void),
            })
            .context("creating Windows media controls")
        }
    }

    #[cfg(target_os = "windows")]
    fn pump_messages() {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        };
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    #[cfg(target_os = "windows")]
    unsafe extern "system" fn wnd_proc(
        hwnd: windows::Win32::Foundation::HWND,
        message: u32,
        wparam: windows::Win32::Foundation::WPARAM,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        use windows::Win32::Foundation::LRESULT;
        use windows::Win32::UI::WindowsAndMessaging::{
            DefWindowProcW, PostQuitMessage, WM_DESTROY,
        };
        if message == WM_DESTROY {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        } else {
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub use native::{MediaControlsHandle, spawn};
