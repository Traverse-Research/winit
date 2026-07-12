use std::{
    collections::VecDeque,
    ffi::c_void,
    fmt::{self, Debug},
    marker::PhantomData,
    ptr,
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};

use core_foundation::base::{Boolean, CFIndex, CFRelease};
use core_foundation::runloop::{
    kCFRunLoopAfterWaiting, kCFRunLoopBeforeWaiting, kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
    kCFRunLoopExit, CFRunLoopActivity, CFRunLoopAddObserver, CFRunLoopAddSource, CFRunLoopGetMain,
    CFRunLoopObserverCreate, CFRunLoopObserverRef, CFRunLoopRunInMode, CFRunLoopSourceContext,
    CFRunLoopSourceCreate, CFRunLoopSourceInvalidate, CFRunLoopSourceRef, CFRunLoopSourceSignal,
    CFRunLoopWakeUp,
};
use icrate::Foundation::{MainThreadMarker, NSString};
use objc2::ClassType;

use crate::{
    error::EventLoopError,
    event::Event,
    event_loop::{
        ControlFlow, DeviceEvents, EventLoopClosed,
        EventLoopWindowTarget as RootEventLoopWindowTarget,
    },
    platform::{ios::Idiom, pump_events::PumpStatus},
};

use super::{app_state, monitor, view, MonitorHandle};
use super::{
    app_state::AppState,
    uikit::{UIApplication, UIApplicationMain, UIDevice, UIScreen},
};

#[derive(Debug)]
pub struct EventLoopWindowTarget<T: 'static> {
    pub(super) mtm: MainThreadMarker,
    p: PhantomData<T>,
}

impl<T: 'static> EventLoopWindowTarget<T> {
    pub fn available_monitors(&self) -> VecDeque<MonitorHandle> {
        monitor::uiscreens(self.mtm)
    }

    pub fn primary_monitor(&self) -> Option<MonitorHandle> {
        Some(MonitorHandle::new(UIScreen::main(self.mtm)))
    }

    #[inline]
    pub fn listen_device_events(&self, _allowed: DeviceEvents) {}

    #[cfg(feature = "rwh_05")]
    #[inline]
    pub fn raw_display_handle_rwh_05(&self) -> rwh_05::RawDisplayHandle {
        rwh_05::RawDisplayHandle::UiKit(rwh_05::UiKitDisplayHandle::empty())
    }

    #[cfg(feature = "rwh_06")]
    #[inline]
    pub fn raw_display_handle_rwh_06(
        &self,
    ) -> Result<rwh_06::RawDisplayHandle, rwh_06::HandleError> {
        Ok(rwh_06::RawDisplayHandle::UiKit(
            rwh_06::UiKitDisplayHandle::new(),
        ))
    }

    pub(crate) fn set_control_flow(&self, control_flow: ControlFlow) {
        AppState::get_mut(self.mtm).set_control_flow(control_flow)
    }

    pub(crate) fn control_flow(&self) -> ControlFlow {
        AppState::get_mut(self.mtm).control_flow()
    }

    pub(crate) fn exit(&self) {
        // https://developer.apple.com/library/archive/qa/qa1561/_index.html
        // It is not possible to quit an iOS app gracefully and programatically, so `exit()` cannot
        // terminate the process. It does, however, request that a `run_on_demand` / `pump_events`
        // driver returns control to its caller, which is the meaningful notion of "exit" there.
        app_state::request_exit(self.mtm);
    }

    pub(crate) fn exiting(&self) -> bool {
        app_state::exit_requested(self.mtm)
    }

    pub(crate) fn clear_exit(&self) {
        app_state::clear_exit(self.mtm)
    }
}

pub struct EventLoop<T: 'static> {
    mtm: MainThreadMarker,
    sender: Sender<T>,
    receiver: Receiver<T>,
    window_target: RootEventLoopWindowTarget<T>,
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PlatformSpecificEventLoopAttributes {}

impl<T: 'static> EventLoop<T> {
    pub(crate) fn new(
        _: &PlatformSpecificEventLoopAttributes,
    ) -> Result<EventLoop<T>, EventLoopError> {
        let mtm = MainThreadMarker::new()
            .expect("On iOS, `EventLoop` must be created on the main thread");

        static mut SINGLETON_INIT: bool = false;
        unsafe {
            assert!(
                !SINGLETON_INIT,
                "Only one `EventLoop` is supported on iOS. \
                 `EventLoopProxy` might be helpful"
            );
            SINGLETON_INIT = true;
        }

        let (sender, receiver) = mpsc::channel();

        // this line sets up the main run loop before `UIApplicationMain`
        setup_control_flow_observers();

        Ok(EventLoop {
            mtm,
            sender,
            receiver,
            window_target: RootEventLoopWindowTarget {
                p: EventLoopWindowTarget {
                    mtm,
                    p: PhantomData,
                },
                _marker: PhantomData,
            },
        })
    }

    pub fn run<F>(self, event_handler: F) -> !
    where
        F: FnMut(Event<T>, &RootEventLoopWindowTarget<T>),
    {
        unsafe {
            let application = UIApplication::shared(self.mtm);
            assert!(
                application.is_none(),
                "\
                `EventLoop` cannot be `run` after a call to `UIApplicationMain` on iOS\n\
                 Note: `EventLoop::run` calls `UIApplicationMain` on iOS",
            );

            let event_handler = std::mem::transmute::<
                Box<dyn FnMut(Event<T>, &RootEventLoopWindowTarget<T>)>,
                Box<EventHandlerCallback<T>>,
            >(Box::new(event_handler));

            let handler = EventLoopHandler {
                f: event_handler,
                receiver: self.receiver,
                event_loop: self.window_target,
            };

            app_state::will_launch(self.mtm, Box::new(handler));

            // Ensure application delegate is initialized
            view::WinitApplicationDelegate::class();

            UIApplicationMain(
                0,
                ptr::null(),
                None,
                Some(&NSString::from_str("WinitApplicationDelegate")),
            );
            unreachable!()
        }
    }

    /// Pump a single slice of the run loop, dispatching any pending events to `event_handler`.
    ///
    /// Unlike [`run`](Self::run), this returns control to the caller. It must be called from within
    /// the entry point started by [`ios_application_main`] (i.e. after `UIApplicationMain` is
    /// already running), since it cannot start `UIApplicationMain` itself (that call never returns).
    pub fn pump_events<F>(&mut self, timeout: Option<Duration>, event_handler: F) -> PumpStatus
    where
        F: FnMut(Event<T>, &RootEventLoopWindowTarget<T>),
    {
        assert!(
            UIApplication::shared(self.mtm).is_some(),
            "`pump_events`/`run_on_demand` on iOS must be called from within the entry point passed \
             to `winit::platform::ios::ios_application_main`, after `UIApplicationMain` has started",
        );

        // Erase the borrowed closure's lifetime for the duration of this call. This is sound because
        // the handler is removed (and dropped) via `take_handler` before we return, so it never
        // outlives the borrow. Mirrors the `transmute` done in `run`.
        let event_handler = unsafe {
            std::mem::transmute::<
                Box<dyn FnMut(Event<T>, &RootEventLoopWindowTarget<T>)>,
                Box<EventHandlerCallback<T>>,
            >(Box::new(event_handler))
        };
        let handler = DriverEventLoopHandler {
            f: event_handler,
            receiver: &self.receiver,
            event_loop: RootEventLoopWindowTarget {
                p: EventLoopWindowTarget {
                    mtm: self.mtm,
                    p: PhantomData,
                },
                _marker: PhantomData,
            },
        };

        app_state::install_handler(self.mtm, Box::new(handler));

        // `returnAfterSourceHandled = false`: run for up to `seconds` so the `BeforeWaiting`
        // run-loop observers (which drive winit's `RedrawRequested`/`AboutToWait` cycle and the
        // transition back to a waiting state) actually fire before we return. An unbounded timeout
        // would suppress that cycle, so `None` is capped.
        //
        // TODO(ios): validate/tune this on device — the correct blocking behaviour vs. observer
        // firing is subtle and can only be verified against a real UIKit run loop.
        let seconds = match timeout {
            Some(timeout) => timeout.as_secs_f64(),
            None => 1.0,
        }
        .max(0.0);
        unsafe {
            let _ = CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds, false as Boolean);
        }

        // End the borrow before returning.
        drop(app_state::take_handler(self.mtm));

        if app_state::exit_requested(self.mtm) {
            PumpStatus::Exit(0)
        } else {
            PumpStatus::Continue
        }
    }

    /// Run the event loop until [`exit`](RootEventLoopWindowTarget::exit) is requested, then return
    /// control to the caller. See [`pump_events`](Self::pump_events) for the iOS entry-point
    /// requirement.
    pub fn run_on_demand<F>(&mut self, mut event_handler: F) -> Result<(), EventLoopError>
    where
        F: FnMut(Event<T>, &RootEventLoopWindowTarget<T>),
    {
        loop {
            // Choose how long a single slice may block based on the current `ControlFlow`. The
            // internal waker timer additionally wakes the loop for `Poll`/`WaitUntil`.
            let timeout = match self.window_target.p.control_flow() {
                ControlFlow::Poll => Some(Duration::ZERO),
                ControlFlow::Wait => None,
                ControlFlow::WaitUntil(instant) => {
                    Some(instant.saturating_duration_since(Instant::now()))
                }
            };
            if let PumpStatus::Exit(_) = self.pump_events(timeout, &mut event_handler) {
                break;
            }
        }
        // Allow the loop to be run again later.
        app_state::clear_exit(self.mtm);
        Ok(())
    }

    pub fn create_proxy(&self) -> EventLoopProxy<T> {
        EventLoopProxy::new(self.sender.clone())
    }

    pub fn window_target(&self) -> &RootEventLoopWindowTarget<T> {
        &self.window_target
    }
}

/// Owns the process `main` on iOS: starts `UIApplicationMain` (which never returns) and, once the
/// app has finished launching, invokes `entry` as a fresh run-loop callback — the same structure
/// SDL uses (`SDL_UIKitRunApp` → `postFinishLaunch` → the user's `main`). `entry` is where you
/// create the [`EventLoop`] and call [`run_on_demand`](EventLoop::run_on_demand) /
/// [`pump_events`](EventLoop::pump_events).
pub fn ios_application_main(entry: fn()) -> ! {
    let mtm =
        MainThreadMarker::new().expect("`ios_application_main` must be called on the main thread");
    unsafe {
        assert!(
            UIApplication::shared(mtm).is_none(),
            "`ios_application_main` cannot be called after `UIApplicationMain`",
        );
        PUMP_ENTRY = Some(entry);

        // Ensure application delegate is initialized
        view::WinitApplicationDelegate::class();

        UIApplicationMain(
            0,
            ptr::null(),
            None,
            Some(&NSString::from_str("WinitApplicationDelegate")),
        );
        unreachable!()
    }
}

// Set by `ios_application_main` before `UIApplicationMain`, read by the app delegate to (a) detect
// that we are in `pump`/`on-demand` mode rather than diverging `run` mode, and (b) run the user's
// entry point after launch. iOS UIKit is single-threaded (main thread only), so a plain `static mut`
// matches the existing `SINGLETON_INIT` pattern in this file.
static mut PUMP_ENTRY: Option<fn()> = None;

pub(crate) fn has_pump_entry() -> bool {
    unsafe { PUMP_ENTRY.is_some() }
}

pub(crate) fn run_pump_entry() {
    if let Some(entry) = unsafe { PUMP_ENTRY.take() } {
        entry();
    }
}

// EventLoopExtIOS
impl<T: 'static> EventLoop<T> {
    pub fn idiom(&self) -> Idiom {
        UIDevice::current(self.mtm).userInterfaceIdiom().into()
    }
}

pub struct EventLoopProxy<T> {
    sender: Sender<T>,
    source: CFRunLoopSourceRef,
}

unsafe impl<T: Send> Send for EventLoopProxy<T> {}

impl<T> Clone for EventLoopProxy<T> {
    fn clone(&self) -> EventLoopProxy<T> {
        EventLoopProxy::new(self.sender.clone())
    }
}

impl<T> Drop for EventLoopProxy<T> {
    fn drop(&mut self) {
        unsafe {
            CFRunLoopSourceInvalidate(self.source);
            CFRelease(self.source as _);
        }
    }
}

impl<T> EventLoopProxy<T> {
    fn new(sender: Sender<T>) -> EventLoopProxy<T> {
        unsafe {
            // just wake up the eventloop
            extern "C" fn event_loop_proxy_handler(_: *const c_void) {}

            // adding a Source to the main CFRunLoop lets us wake it up and
            // process user events through the normal OS EventLoop mechanisms.
            let rl = CFRunLoopGetMain();
            let mut context = CFRunLoopSourceContext {
                version: 0,
                info: ptr::null_mut(),
                retain: None,
                release: None,
                copyDescription: None,
                equal: None,
                hash: None,
                schedule: None,
                cancel: None,
                perform: event_loop_proxy_handler,
            };
            let source =
                CFRunLoopSourceCreate(ptr::null_mut(), CFIndex::max_value() - 1, &mut context);
            CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
            CFRunLoopWakeUp(rl);

            EventLoopProxy { sender, source }
        }
    }

    pub fn send_event(&self, event: T) -> Result<(), EventLoopClosed<T>> {
        self.sender
            .send(event)
            .map_err(|::std::sync::mpsc::SendError(x)| EventLoopClosed(x))?;
        unsafe {
            // let the main thread know there's a new event
            CFRunLoopSourceSignal(self.source);
            let rl = CFRunLoopGetMain();
            CFRunLoopWakeUp(rl);
        }
        Ok(())
    }
}

fn setup_control_flow_observers() {
    unsafe {
        // begin is queued with the highest priority to ensure it is processed before other observers
        extern "C" fn control_flow_begin_handler(
            _: CFRunLoopObserverRef,
            activity: CFRunLoopActivity,
            _: *mut c_void,
        ) {
            let mtm = MainThreadMarker::new().unwrap();
            #[allow(non_upper_case_globals)]
            match activity {
                kCFRunLoopAfterWaiting => app_state::handle_wakeup_transition(mtm),
                _ => unreachable!(),
            }
        }

        // Core Animation registers its `CFRunLoopObserver` that performs drawing operations in
        // `CA::Transaction::ensure_implicit` with a priority of `0x1e8480`. We set the main_end
        // priority to be 0, in order to send AboutToWait before RedrawRequested. This value was
        // chosen conservatively to guard against apple using different priorities for their redraw
        // observers in different OS's or on different devices. If it so happens that it's too
        // conservative, the main symptom would be non-redraw events coming in after `AboutToWait`.
        //
        // The value of `0x1e8480` was determined by inspecting stack traces and the associated
        // registers for every `CFRunLoopAddObserver` call on an iPad Air 2 running iOS 11.4.
        //
        // Also tested to be `0x1e8480` on iPhone 8, iOS 13 beta 4.
        extern "C" fn control_flow_main_end_handler(
            _: CFRunLoopObserverRef,
            activity: CFRunLoopActivity,
            _: *mut c_void,
        ) {
            let mtm = MainThreadMarker::new().unwrap();
            #[allow(non_upper_case_globals)]
            match activity {
                kCFRunLoopBeforeWaiting => app_state::handle_main_events_cleared(mtm),
                kCFRunLoopExit => {} // may happen when running on macOS
                _ => unreachable!(),
            }
        }

        // end is queued with the lowest priority to ensure it is processed after other observers
        extern "C" fn control_flow_end_handler(
            _: CFRunLoopObserverRef,
            activity: CFRunLoopActivity,
            _: *mut c_void,
        ) {
            let mtm = MainThreadMarker::new().unwrap();
            #[allow(non_upper_case_globals)]
            match activity {
                kCFRunLoopBeforeWaiting => app_state::handle_events_cleared(mtm),
                kCFRunLoopExit => {} // may happen when running on macOS
                _ => unreachable!(),
            }
        }

        let main_loop = CFRunLoopGetMain();

        let begin_observer = CFRunLoopObserverCreate(
            ptr::null_mut(),
            kCFRunLoopAfterWaiting,
            1, // repeat = true
            CFIndex::min_value(),
            control_flow_begin_handler,
            ptr::null_mut(),
        );
        CFRunLoopAddObserver(main_loop, begin_observer, kCFRunLoopDefaultMode);

        let main_end_observer = CFRunLoopObserverCreate(
            ptr::null_mut(),
            kCFRunLoopExit | kCFRunLoopBeforeWaiting,
            1, // repeat = true
            0, // see comment on `control_flow_main_end_handler`
            control_flow_main_end_handler,
            ptr::null_mut(),
        );
        CFRunLoopAddObserver(main_loop, main_end_observer, kCFRunLoopDefaultMode);

        let end_observer = CFRunLoopObserverCreate(
            ptr::null_mut(),
            kCFRunLoopExit | kCFRunLoopBeforeWaiting,
            1, // repeat = true
            CFIndex::max_value(),
            control_flow_end_handler,
            ptr::null_mut(),
        );
        CFRunLoopAddObserver(main_loop, end_observer, kCFRunLoopDefaultMode);
    }
}

#[derive(Debug)]
pub enum Never {}

type EventHandlerCallback<T> = dyn FnMut(Event<T>, &RootEventLoopWindowTarget<T>) + 'static;

pub trait EventHandler: Debug {
    fn handle_nonuser_event(&mut self, event: Event<Never>);
    fn handle_user_events(&mut self);
}

struct EventLoopHandler<T: 'static> {
    f: Box<EventHandlerCallback<T>>,
    receiver: Receiver<T>,
    event_loop: RootEventLoopWindowTarget<T>,
}

impl<T: 'static> Debug for EventLoopHandler<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventLoopHandler")
            .field("event_loop", &self.event_loop)
            .finish()
    }
}

impl<T: 'static> EventHandler for EventLoopHandler<T> {
    fn handle_nonuser_event(&mut self, event: Event<Never>) {
        (self.f)(event.map_nonuser_event().unwrap(), &self.event_loop);
    }

    fn handle_user_events(&mut self) {
        for event in self.receiver.try_iter() {
            (self.f)(Event::UserEvent(event), &self.event_loop);
        }
    }
}

// Like `EventLoopHandler`, but for the `run_on_demand`/`pump_events` drivers, which borrow (rather
// than own) the `EventLoop`. The receiver is borrowed via a pointer; the handler is always removed
// and dropped before the driver call returns, so the borrow stays valid.
struct DriverEventLoopHandler<T: 'static> {
    f: Box<EventHandlerCallback<T>>,
    receiver: *const Receiver<T>,
    event_loop: RootEventLoopWindowTarget<T>,
}

impl<T: 'static> Debug for DriverEventLoopHandler<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverEventLoopHandler")
            .field("event_loop", &self.event_loop)
            .finish()
    }
}

impl<T: 'static> EventHandler for DriverEventLoopHandler<T> {
    fn handle_nonuser_event(&mut self, event: Event<Never>) {
        (self.f)(event.map_nonuser_event().unwrap(), &self.event_loop);
    }

    fn handle_user_events(&mut self) {
        // SAFETY: the borrowed `EventLoop` outlives this handler (removed before the driver returns).
        let receiver = unsafe { &*self.receiver };
        for event in receiver.try_iter() {
            (self.f)(Event::UserEvent(event), &self.event_loop);
        }
    }
}
