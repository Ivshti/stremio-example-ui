#![windows_subsystem = "windows"]
use enclose::*;
use futures::{future, Future, Stream};
use serde::de::DeserializeOwned;
use serde::Serialize;
use stremio_core::state_types::*;
// required to make stremio_derive work :(
pub use stremio_core::state_types;
use stremio_derive::*;
use std::thread;
use stremio_core::types::MetaPreview;
use stremio_core::types::addons::{ResourceRef, ResourceRequest};
use tokio::executor::current_thread::spawn;
use tokio::runtime::current_thread::run;
use futures::sync::mpsc::{channel, Sender};

// TODO
// * list of all widgets for a simple, mvp UI
// * EGL might help initializing zero-copy vaapi; see https://www.qtav.org/blog/1.9.0.html ; and EGL_EXT_image_dma_buf_import
// * investigate CPU load on windows (with mpv symbols)
// * implement Streams (in the UI)
// * implement a primitive UI
// * optimization of storage (currently sled-based): takes around 400ms to load a 1300 item lib;
// also needs to be async
// * mpv: safer/better crate
// * cache, images
// * if we're gonna cache images with sled, we need to do it in a separate thread
// * optimization: only draw when there is a new frame
// * optimization: do not draw the UI when it's not showing (player)
// * optimization: see https://github.com/mpv-player/mpv/blob/master/libmpv/render_gl.h#L81
// * optimization: look into d3d11 + hw accel

#[derive(Model, Debug, Default)]
struct Model {
    ctx: Ctx<Env>,
    catalogs: CatalogFiltered,
}

fn main() {
    let app = Model::default();
    let (runtime, runtime_receiver) = Runtime::<Env, Model>::new(app, 1000);
    let (tx, action_receiver) = channel(1000);

    // Spawn the UI thread
    let ui_thread = thread::spawn(enclose!((runtime, tx) || run_ui(runtime, tx)));

    run(
        action_receiver.for_each(enclose!((runtime) move |action| {
            spawn(runtime.dispatch(&Msg::Action(action)));
            future::ok(())
        }))
        .join(runtime_receiver.for_each(|_msg| {
            dbg!(&_msg);
            //if let RuntimeEv::NewModel(_) = ev {
            //}
            future::ok(())
        }))
        .map(|(_, _)| ())
    );

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
unsafe extern "C" fn get_proc_address(arg: *mut c_void,
                                      name: *const c_char) -> *mut c_void {
    let arg: &glutin::GlWindow = &*(arg as *mut glutin::GlWindow);
    let name = CStr::from_ptr(name).to_str().unwrap();
    arg.get_proc_address(name) as *mut c_void
}

fn run_ui(runtime: Runtime<Env, Model>, tx: Sender<Action>) {
    // Trigger loading a catalog ASAP

    // WARNING: we can't spawn actions on the main thread
    let resource_req = ResourceRequest {
        base: "https://v3-cinemeta.strem.io/manifest.json".to_owned(),
        path: ResourceRef::without_extra("catalog", "movie", "top"),
    };
    let action = Action::Load(ActionLoad::CatalogFiltered { resource_req });
    tx.clone().try_send(action).unwrap();

    // Builder for window
    let builder = glutin::WindowBuilder::new()
        .with_title("Stremio Example UI")
        .with_dimensions((WIN_W, WIN_H).into());

    let context = glutin::ContextBuilder::new()
        // tried GLES to enable vaapi egl, but it's not so simple since it requires a XCB extension
        // also seems to fail with gfx
        // but see https://github.com/jbg/conrod-android-skeleton
        //.with_gl(glutin::GlRequest::Specific(glutin::Api::OpenGlEs, (3, 0)));
        .with_multisampling(4);
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
    mpv_builder.set_option("terminal", "yes").expect("failed setting terminal");
    mpv_builder.set_option("msg-level", "all=v").expect("failed setting msg-level");
    let mut mpv: Box<mpv::MpvHandlerWithGl> = mpv_builder
        .build_with_gl(Some(get_proc_address), ptr)
        .expect("Error while initializing MPV with opengl");
    //let video_path = "/home/ivo/storage/bbb_sunflower_1080p_30fps_normal.mp4";
    let video_path = "http://distribution.bbb3d.renderfarming.net/video/mp4/bbb_sunflower_1080p_30fps_normal.mp4";
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
        let mut needs_swap_buffers = false;
        let dpi_factor = window.get_hidpi_factor() as f32;
        let dims = (win_w as f32 * dpi_factor, win_h as f32 * dpi_factor);

        // @TODO set mpv_is_playing to something reasonable
        let mpv_is_playing = true;
        if mpv_is_playing {
            mpv.draw(0, dims.0 as i32, -(dims.1 as i32)).expect("failed to draw on conrod window");
            needs_swap_buffers = true;
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
            needs_swap_buffers = true;
        }
        if needs_swap_buffers {
            // see vsync at https://docs.rs/glium/0.19.0/glium/glutin/struct.GlAttributes.html 
            window.swap_buffers().unwrap();
            device.cleanup();
        }

        // Consume mpv events
        //while let Some(ev) = mpv.wait_event(0.0) {dbg!(ev);}

        // Update widgets if any event has happened
        if ui.global_input().events().next().is_some() {
            let ui = &mut ui.set_widgets();

            widget::Canvas::new()
                .color(conrod_core::color::TRANSPARENT)
                .set(ids.canvas, ui);

            let app = runtime.app.read().expect("unable to lock app from ui");

            let items: Vec<&MetaPreview> = app
                .catalogs
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
        }

        // Only tick each FRAME_MILLIS; wait if we're faster than that
        let to_wait = Duration::from_millis(FRAME_MILLIS)
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::new(0, 0));
        thread::sleep(to_wait);
    }
}

// Define the environment
use lazy_static::*;
use sled::Db;
lazy_static! {
    static ref STORAGE: sled::Db = {
        Db::start_default("./storage-stremio-example-ui").expect("failed to start sled")
    };
}
struct Env {}
impl Environment for Env {
    fn fetch_serde<IN, OUT>(in_req: Request<IN>) -> EnvFuture<OUT>
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
            .map_err(|e| e.into());
        Box::new(fut)
    }
    fn exec(fut: Box<dyn Future<Item = (), Error = ()>>) {
        spawn(fut);
    }
    fn get_storage<T: 'static + DeserializeOwned>(key: &str) -> EnvFuture<Option<T>> {
        let opt = match STORAGE.get(key.as_bytes()) {
            Ok(s) => s,
            Err(e) => return Box::new(future::err(e.into())),
        };
        Box::new(future::ok(
            opt.map(|v| serde_json::from_slice(&*v).unwrap()),
        ))
    }
    fn set_storage<T: 'static + Serialize>(key: &str, value: Option<&T>) -> EnvFuture<()> {
        let res = match value {
            Some(v) => STORAGE.set(key.as_bytes(), serde_json::to_string(v).unwrap().as_bytes()),
            None => STORAGE.del(key),
        };
        match res {
            Ok(_) => Box::new(future::ok(())),
            Err(e) => Box::new(future::err(e.into())),
        }
    }
}
