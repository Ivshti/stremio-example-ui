use stremio_state_ng::middlewares::*;
use stremio_state_ng::state_types::*;
use futures::{future, Future};
use futures::future::lazy;
use tokio::runtime::current_thread::run;
use tokio::executor::current_thread::spawn;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::cell::RefCell;
use std::rc::Rc;
use std::thread;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use enclose::*;

// for now, we will spawn conrod in a separate thread and communicate via channels
// otherwise, we may find a better solution here:
// https://github.com/tokio-rs/tokio-core/issues/150

// TODO
// * update the UI while we're receiving stuff
// * figure out state shape and can we avoid copying groups

#[derive(Debug, Default)]
struct AppState {
    count: u32
}

#[derive(Debug)]
struct App {
    state: Mutex<AppState>,
    is_dirty: AtomicBool
}
impl App {
    fn new() -> Self {
        App {
            state: Mutex::new(Default::default()),
            is_dirty: AtomicBool::new(false),
        }
    }
}

fn main() {
    let (tx, rx) = channel();

    // @TODO should this be a new type
    let app = Arc::new(App::new());

    let container = Rc::new(RefCell::new(Container::with_reducer(
        CatalogGrouped::new(),
        &catalogs_reducer,
    )));
    #[derive(Debug, Clone)]
    enum ContainerId {
        Board,
    };
    let muxer = Rc::new(ContainerMuxer::new(
        vec![
            Box::new(ContextMiddleware::<Env>::new()),
            Box::new(AddonsMiddleware::<Env>::new()),
        ],
        vec![(ContainerId::Board, container.clone())],
        Box::new(enclose!((app, container) move |ev| {
            if let Event::NewState(ContainerId::Board) = ev {
                let mut state = app.state.lock().expect("unable to lock app");
                state.count = container.borrow().get_state().groups.iter().filter(|g| {
                    match g.1 {
                        Loadable::Ready(_) => true,
                        _ => false
                    }
                }).count() as u32;
                app.is_dirty.store(true, Ordering::Relaxed);
            }
            //eventTx.send(ev).map_err(|_| ());
        })),
    ));
    
    // Spawn the UI
    let ui_thread = thread::spawn(enclose!((app) || run_ui(app, tx)));

    // @TODO: this here is not right,
    // cause we won't be able to react to any new actions while we're still processing the last one
    // to fix it, turn the rx.recv() into a stream and give that stream to the executor
    while let Ok(action) = rx.recv() {
        run(lazy(enclose!((muxer) move || {
            muxer.dispatch(&action);
            future::ok(())
        })));
    }

    ui_thread.join().unwrap();
}


const WIN_H: u32 = 600;
const WIN_W: u32 = 1000;
use gfx::Device;
use conrod_core::widget_ids;
use conrod_core::{widget, Labelable, Positionable, Colorable, Sizeable, Widget};

const CLEAR_COLOR: [f32; 4] = [0.2, 0.2, 0.2, 1.0];

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

fn run_ui(app: Arc<App>, tx: std::sync::mpsc::Sender<Action>) {
    // Builder for window
    let builder = glutin::WindowBuilder::new()
        .with_title("Stremio Example UI")
        .with_dimensions((WIN_W, WIN_H).into());

    let context = glutin::ContextBuilder::new()
        .with_multisampling(4);

    let mut events_loop = winit::EventsLoop::new();

    // Initialize gfx things
    let (window, mut device, mut factory, rtv, _) =
        gfx_window_glutin::init::<conrod_gfx::ColorFormat, DepthFormat>(builder, context, &events_loop).unwrap();
    let mut encoder: gfx::Encoder<_, _> = factory.create_command_buffer().into();

    let mut renderer = conrod_gfx::Renderer::new(&mut factory, &rtv, window.get_hidpi_factor() as f64).unwrap();

    // Create Ui and Ids of widgets to instantiate
    let mut ui = conrod_core::UiBuilder::new([WIN_W as f64, WIN_H as f64])
        .build();

    // load the font
    ui.fonts.insert_from_file("./assets/fonts/NotoSans-Regular.ttf").unwrap();

    // Generate the widget identifiers.
    widget_ids!(struct Ids { canvas, counter });
    let ids = Ids::new(ui.widget_id_generator());

    let image_map = conrod_core::image::Map::new();

    'main: loop {
        // If the window is closed, this will be None for one tick, so to avoid panicking with
        // unwrap, instead break the loop
        let (win_w, win_h): (u32, u32) = match window.get_inner_size() {
            Some(s) => s.into(),
            None => break 'main,
        };

        let dpi_factor = window.get_hidpi_factor() as f32;

        if let Some(primitives) = ui.draw_if_changed() {
            let dims = (win_w as f32 * dpi_factor, win_h as f32 * dpi_factor);

            //Clear the window
            renderer.clear(&mut encoder, CLEAR_COLOR);

            renderer.fill(&mut encoder,dims,dpi_factor as f64,primitives,&image_map);

            renderer.draw(&mut factory,&mut encoder,&image_map);

            encoder.flush(&mut device);
            window.swap_buffers().unwrap();
            device.cleanup();
        }

        let mut should_quit = false;
        events_loop.poll_events(|event| {
            // Convert winit event to conrod event, requires conrod to be built with the `winit` feature
            if let Some(event) = conrod_winit::convert_event(event.clone(), &WindowRef(window.window())) {
                ui.handle_event(event);
            }

            // Close window if the escape key or the exit button is pressed
            match event {
                winit::Event::WindowEvent{event, .. } =>
                    match event {
                        winit::WindowEvent::KeyboardInput{ input: winit::KeyboardInput{ virtual_keycode: Some(winit::VirtualKeyCode::Escape),..}, ..} |
                        winit::WindowEvent::CloseRequested => should_quit = true,
                        winit::WindowEvent::Resized(logical_size) => {
                            let hidpi_factor = window.get_hidpi_factor();
                            let physical_size = logical_size.to_physical(hidpi_factor);
                            window.resize(physical_size);
                            let (new_color, _) = gfx_window_glutin::new_views::<conrod_gfx::ColorFormat, DepthFormat>(&window);
                            renderer.on_resize(new_color);
                        }
                        _ => {},
                    },
                _ => {},
            }
        });
        if should_quit {
            break 'main;
        }

        // Update widgets if any event has happened
        if ui.global_input().events().next().is_some() || app.is_dirty.load(Ordering::Relaxed) {
            let ui = &mut ui.set_widgets();
            // Create a background canvas upon which we'll place the button.
            widget::Canvas::new()
                .pad(40.0)
                .color(conrod_core::color::BLUE)
                .set(ids.canvas, ui);

            // Draw the button and increment `count` if pressed.
            let state = app.state.lock().expect("unable to lock app from ui");
            for _click in widget::Button::new()
                .middle_of(ids.canvas)
                .w_h(80.0, 80.0)
                .label(&state.count.to_string())
                .set(ids.counter, ui)
            {
                let action = Action::Load(ActionLoad::CatalogGrouped { extra: vec![] });
                tx.send(action).expect("failed sending action");
            }

            app.is_dirty.store(false, Ordering::Relaxed);
        }
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
