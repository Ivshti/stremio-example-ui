use enclose::*;
use futures::sync::mpsc::channel;
use futures::{future, Future, Stream};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use stremio_state_ng::middlewares::*;
use stremio_state_ng::state_types::*;
use stremio_state_ng::types::MetaPreview;
use stremio_state_ng::types::addons::{ResourceRef, ResourceRequest};
use tokio::executor::current_thread::spawn;
use tokio::runtime::current_thread::run;

// for now, we will spawn conrod in a separate thread and communicate via channels
// otherwise, we may find a better solution here:
// https://github.com/tokio-rs/tokio-core/issues/150

// TODO
// * investigate why CPU usage keeps rising throughout the lifetime of the process, while playing a
// video; perhaps the MPV event queue is filling up?
// * implement a primitive UI
// * mpv: safer/better crate
// * decide the cache/storage layer; perhaps paritydb
// * cache, images

struct ContainerHolder(Mutex<CatalogFiltered>);

impl ContainerHolder {
    pub fn new(container: CatalogFiltered) -> Self {
        ContainerHolder(Mutex::new(container))
    }
}

impl ContainerInterface for ContainerHolder {
    fn dispatch(&self, action: &Action) -> bool {
        let mut state = self.0.lock().expect("failed to lock container");
        match state.dispatch(action) {
            Some(s) => {
                *state = *s;
                true
            }
            None => false,
        }
    }
    fn get_state_serialized(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self.0.lock().expect("failed to lock container").deref())
    }
}

struct App {
    container: Arc<ContainerHolder>,
    is_dirty: AtomicBool,
}

const MAX_ACTION_BUFFER: usize = 1024;

fn main() {
    let container = Arc::new(ContainerHolder::new(CatalogFiltered::new()));

    let app = Arc::new(App {
        container: container.clone(),
        is_dirty: AtomicBool::new(false),
    });

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
    enum ContainerId {
        //Board,
        Discover,
    };
    let muxer = Rc::new(ContainerMuxer::new(
        vec![
            Box::new(ContextMiddleware::<Env>::new()),
            Box::new(AddonsMiddleware::<Env>::new()),
        ],
        vec![(
            ContainerId::Discover,
            container.clone() as Arc<dyn ContainerInterface>,
        )],
        Box::new(enclose!((app) move |ev| {
            if let Event::NewState(_) = ev {
                app.is_dirty.store(true, Ordering::Relaxed);
            }
        })),
    ));

    // Actions channel
    let (tx, rx) = channel(MAX_ACTION_BUFFER);

    // Spawn the UI
    let dispatch = Box::new(move |action| {
        tx.clone().try_send(action).expect("failed sending");
    });
    let ui_thread = thread::spawn(enclose!((app) || run_ui(app, dispatch)));

    run(rx.for_each(enclose!((muxer) move |action| {
        muxer.dispatch(&action);
        future::ok(())
    })));

    ui_thread.join().expect("failed joining ui_thread");
}

const WIN_H: u32 = 600;
const WIN_W: u32 = 1000;
use conrod_core::widget_ids;
use conrod_core::{widget, Colorable, Positionable, Labelable, Sizeable, Widget};
use gfx::Device;
use std::time::{Duration, Instant};

const FRAME_MILLIS: u64 = 15;

//const CLEAR_COLOR: [f32; 4] = [0.2, 0.2, 0.2, 0.5];

type DepthFormat = gfx::format::DepthStencil;

// A wrapper around the winit window that allows us to implement the trait necessary for enabling
// the winit <-> conrod conversion functions.
struct WindowRef<'a>(&'a winit::Window);

// Implement the `WinitWindow` trait for `WindowRef` to allow for generating compatible conversion
// functions.
impl<'a> conrod_winit::WinitWindow for WindowRef<'a> {
    fn get_inner_size(&self) -> Option<(u32, u32)> {
        winit::Window::get_inner_size(&self.0).map(Into::into)
    }
    fn hidpi_factor(&self) -> f32 {
        winit::Window::get_hidpi_factor(&self.0) as _
    }
}

use std::os::raw::{c_void,c_char};
use std::ffi::CStr;
use glutin::GlContext;
use std::collections::HashMap;
use std::cell::RefCell;
thread_local! {
    static ADDRS: RefCell<HashMap<String, usize>> = {
        Default::default()
    };
}
unsafe extern "C" fn get_proc_address(arg: *mut c_void,
                                      name: *const c_char) -> *mut c_void {
    let arg: &glutin::GlWindow = &*(arg as *mut glutin::GlWindow);
    let name = CStr::from_ptr(name).to_str().unwrap();
    ADDRS.with(|map| {
        let mut map = map.borrow_mut();

        match map.get(&name.to_string()) {
            Some(e) => *e as *mut c_void,
            None => {
                let e = arg.get_proc_address(name);
                map.insert(name.to_owned(), e as usize);
                e as *mut c_void
            }
        }
    })
}

fn run_ui(app: Arc<App>, dispatch: Box<Fn(Action)>) {
    // Trigger loading a catalog ASAP
    let resource_req = ResourceRequest {
        transport_url: "https://v3-cinemeta.strem.io/manifest.json".to_owned(),
        resource_ref: ResourceRef::without_extra("catalog", "movie", "top"),
    };
    let action = Action::Load(ActionLoad::CatalogFiltered { resource_req });
    dispatch(action);

    // Builder for window
    let builder = glutin::WindowBuilder::new()
        .with_title("Stremio Example UI")
        .with_dimensions((WIN_W, WIN_H).into());

    let context = glutin::ContextBuilder::new().with_multisampling(4);
    let mut events_loop = winit::EventsLoop::new();

    // Initialize gfx things
    let (mut window, mut device, mut factory, rtv, _) = gfx_window_glutin::init::<
        conrod_gfx::ColorFormat,
        DepthFormat,
    >(builder, context, &events_loop)
    .unwrap();
    let mut encoder: gfx::Encoder<_, _> = factory.create_command_buffer().into();

    // Initialize mpv
    let ptr = &mut window as *mut glutin::GlWindow as *mut c_void;
    let mut mpv_builder = mpv::MpvHandlerBuilder::new()
        .expect("Error while creating MPV builder");
    mpv_builder.try_hardware_decoding()
        .expect("failed setting hwdec");
    let mut mpv: Box<mpv::MpvHandlerWithGl> = mpv_builder
        .build_with_gl(Some(get_proc_address), ptr)
        .expect("Error while initializing MPV with opengl");
    let video_path = "/home/ivo/storage/bbb_sunflower_1080p_30fps_normal.mp4";
    mpv.command(&["loadfile", video_path])
        .expect("Error loading file");


    let mut renderer =
        conrod_gfx::Renderer::new(&mut factory, &rtv, window.get_hidpi_factor() as f64).unwrap();

    // Create UI and Ids of widgets to instantiate
    let mut ui = conrod_core::UiBuilder::new([WIN_W as f64, WIN_H as f64]).build();

    // load the font
    ui.fonts
        .insert_from_file("./assets/fonts/NotoSans-Regular.ttf")
        .unwrap();

    // Generate the widget identifiers.
    widget_ids!(struct Ids { canvas, list });
    let ids = Ids::new(ui.widget_id_generator());

    let image_map = conrod_core::image::Map::new();

    let mut last_tick;

    'main: loop {
        last_tick = Instant::now();

        let mut should_quit = false;
        events_loop.poll_events(|event| {
            // Convert winit event to conrod event, requires conrod to be built with the `winit` feature
            if let Some(event) =
                conrod_winit::convert_event(event.clone(), &WindowRef(window.window()))
            {
                ui.handle_event(event);
            }

            // Close window if the exit button is pressed
            if let winit::Event::WindowEvent { event, .. } = event {
                match event {
                    winit::WindowEvent::CloseRequested => should_quit = true,
                    winit::WindowEvent::Resized(logical_size) => {
                        let hidpi_factor = window.get_hidpi_factor();
                        let physical_size = logical_size.to_physical(hidpi_factor);
                        window.resize(physical_size);
                        let (new_color, _) = gfx_window_glutin::new_views::<
                            conrod_gfx::ColorFormat,
                            DepthFormat,
                        >(&window);
                        renderer.on_resize(new_color);
                    }
                    _ => {}
                }
            }
        });
        if should_quit {
            break 'main;
        }

        // If the window is closed, this will be None for one tick, so to avoid panicking with
        // unwrap, instead break the loop
        let (win_w, win_h): (u32, u32) = match window.get_inner_size() {
            Some(s) => s.into(),
            None => break 'main,
        };

        // Draw if anything has changed
        let mut needs_cleanup = false;
        let dpi_factor = window.get_hidpi_factor() as f32;
        let dims = (win_w as f32 * dpi_factor, win_h as f32 * dpi_factor);

        // @TODO set mpv_is_playing to something reasonable
        let mpv_is_playing = true;
        if mpv_is_playing {
            mpv.draw(0, dims.0 as i32, -(dims.1 as i32)).expect("failed to draw on conrod window");
            needs_cleanup = true;
        }
        let maybe_primitives = if mpv_is_playing { Some(ui.draw()) } else { ui.draw_if_changed() };
        if let Some(primitives) = maybe_primitives {
            //Clear the window
            // is this really needed?
            //renderer.clear(&mut encoder, CLEAR_COLOR);

            renderer.fill(
                &mut encoder,
                dims,
                dpi_factor as f64,
                primitives,
                &image_map,
            );

            renderer.draw(&mut factory, &mut encoder, &image_map);

            encoder.flush(&mut device);
            needs_cleanup = true;
        }
        if needs_cleanup {
            window.swap_buffers().unwrap();
            device.cleanup();
        }

        // Consume mpv events
        //while let Some(ev) = mpv.wait_event(0.0) {dbg!(ev);}

        // Update widgets if any event has happened
        if ui.global_input().events().next().is_some() || app.is_dirty.load(Ordering::Relaxed) {
            let ui = &mut ui.set_widgets();

            widget::Canvas::new()
                .color(conrod_core::color::TRANSPARENT)
                .set(ids.canvas, ui);

            let container = app.container.0.lock().expect("unable to lock app from ui");

            let items: Vec<&MetaPreview> = container
                .item_pages
                .iter()
                .filter_map(|g| match g {
                    Loadable::Ready(i) => Some(i),
                    _ => None
                })
                .flatten()
                .collect();
            let (mut list_items, scrollbar) = widget::List::flow_down(items.len())
                .item_size(50.0)
                .scrollbar_on_top()
                .middle_of(ids.canvas)
                .wh_of(ids.canvas)
                .set(ids.list, ui);
            while let Some(list_item) = list_items.next(ui) {
                let item = &items[list_item.i];

                let g = widget::Button::new()
                    .label(&item.name)
                    .color(conrod_core::Color::Rgba(0.45, 0.30, 0.7, 0.5));
                list_item.set(g, ui);
            }
            if let Some(s) = scrollbar {
                s.set(ui)
            };

            app.is_dirty.store(false, Ordering::Relaxed);
        }

        // Only tick each FRAME_MILLIS; wait if we're faster than that
        let to_wait = Duration::from_millis(FRAME_MILLIS)
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::new(0, 0));
        thread::sleep(to_wait);
    }
}

// Define the environment
struct Env {}
impl Environment for Env {
    fn fetch_serde<IN, OUT>(in_req: Request<IN>) -> EnvFuture<Box<OUT>>
    where
        IN: 'static + Serialize,
        OUT: 'static + DeserializeOwned,
    {
        let (parts, body) = in_req.into_parts();
        let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
            .expect("method is not valid for reqwest");
        let mut req = reqwest::r#async::Client::new().request(method, &parts.uri.to_string());
        // NOTE: both might be HeaderMap, so maybe there's a better way?
        for (k, v) in parts.headers.iter() {
            req = req.header(k.as_str(), v.as_ref());
        }
        // @TODO add content-type application/json
        // @TODO: if the response code is not 200, return an error related to that
        req = req.json(&body);
        let fut = req
            .send()
            .and_then(|mut res: reqwest::r#async::Response| res.json::<OUT>())
            .map(|res| Box::new(res))
            .map_err(|e| e.into());
        Box::new(fut)
    }
    fn exec(fut: Box<Future<Item = (), Error = ()>>) {
        spawn(fut);
    }
    fn get_storage<T: 'static + DeserializeOwned>(_key: &str) -> EnvFuture<Option<Box<T>>> {
        Box::new(future::ok(None))
    }
    fn set_storage<T: 'static + Serialize>(_key: &str, _value: Option<&T>) -> EnvFuture<()> {
        Box::new(future::err("unimplemented".into()))
    }
}
