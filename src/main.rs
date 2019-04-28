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
use tokio::executor::current_thread::spawn;
use tokio::runtime::current_thread::run;

// for now, we will spawn conrod in a separate thread and communicate via channels
// otherwise, we may find a better solution here:
// https://github.com/tokio-rs/tokio-core/issues/150

// TODO
// * implement a primitive UI
// * fix window resizing
// * decide the cache/storage layer; perhaps paritydb
// * cache, images

struct ContainerHolder(Mutex<CatalogGrouped>);

impl ContainerHolder {
    pub fn new(container: CatalogGrouped) -> Self {
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
    let container = Arc::new(ContainerHolder::new(CatalogGrouped::new()));

    let app = Arc::new(App {
        container: container.clone(),
        is_dirty: AtomicBool::new(false),
    });

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
    enum ContainerId {
        Board,
    };
    let muxer = Rc::new(ContainerMuxer::new(
        vec![
            Box::new(ContextMiddleware::<Env>::new()),
            Box::new(AddonsMiddleware::<Env>::new()),
        ],
        vec![(
            ContainerId::Board,
            container.clone() as Arc<dyn ContainerInterface>,
        )],
        Box::new(enclose!((app) move |ev| {
            if let Event::NewState(ContainerId::Board) = ev {
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
use conrod_core::{widget, Colorable, Positionable, Sizeable, Widget};
use gfx::Device;
use std::time::{Duration, Instant};

const FRAME_MILLIS: u64 = 15;

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

fn run_ui(app: Arc<App>, dispatch: Box<Fn(Action)>) {
    // Trigger loading a catalog ASAP
    let action = Action::Load(ActionLoad::CatalogGrouped { extra: vec![] });
    dispatch(action);

    // Builder for window
    let builder = glutin::WindowBuilder::new()
        .with_title("Stremio Example UI")
        .with_dimensions((WIN_W, WIN_H).into());

    let context = glutin::ContextBuilder::new().with_multisampling(4);

    let mut events_loop = winit::EventsLoop::new();

    // Initialize gfx things
    let (window, mut device, mut factory, rtv, _) = gfx_window_glutin::init::<
        conrod_gfx::ColorFormat,
        DepthFormat,
    >(builder, context, &events_loop)
    .unwrap();
    let mut encoder: gfx::Encoder<_, _> = factory.create_command_buffer().into();

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
        if let Some(primitives) = ui.draw_if_changed() {
            let dpi_factor = window.get_hidpi_factor() as f32;
            let dims = (win_w as f32 * dpi_factor, win_h as f32 * dpi_factor);

            //Clear the window
            renderer.clear(&mut encoder, CLEAR_COLOR);

            renderer.fill(
                &mut encoder,
                dims,
                dpi_factor as f64,
                primitives,
                &image_map,
            );

            renderer.draw(&mut factory, &mut encoder, &image_map);

            encoder.flush(&mut device);
            window.swap_buffers().unwrap();
            device.cleanup();
        }

        // Update widgets if any event has happened
        if ui.global_input().events().next().is_some() || app.is_dirty.load(Ordering::Relaxed) {
            let ui = &mut ui.set_widgets();

            widget::Canvas::new()
                .color(conrod_core::color::DARK_CHARCOAL)
                .set(ids.canvas, ui);

            let container = app.container.0.lock().expect("unable to lock app from ui");

            let (mut groups_items, scrollbar) = widget::List::flow_down(container.groups.len())
                .item_size(50.0)
                .scrollbar_on_top()
                .middle_of(ids.canvas)
                .wh_of(ids.canvas)
                .set(ids.list, ui);
            while let Some(group_item) = groups_items.next(ui) {
                let group = &container.groups[group_item.i];

                let g = self::catalog_group::CatalogGroup::new(&group)
                    .color(conrod_core::color::LIGHT_BLUE);
                group_item.set(g, ui);
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


#[macro_use] extern crate conrod_core;
// CatalogGroup
mod catalog_group {
    use conrod_core::{self, widget_ids, widget, Colorable, Labelable, Point, Widget};
    use stremio_state_ng::types::MetaPreview;
    use stremio_state_ng::types::addons::ResourceRequest;
    use stremio_state_ng::state_types::{Loadable, Message};
   
    type Group = (ResourceRequest, Loadable<Vec<MetaPreview>, Message>);

    #[derive(WidgetCommon)]
    pub struct CatalogGroup<'a> {
        /// An object that handles some of the dirty work of rendering a GUI. We don't
        /// really have to worry about it.
        #[conrod(common_builder)]
        common: widget::CommonBuilder,
        /// Optional label string for the button.
        group: &'a Group,
        /// See the Style struct below.
        style: Style,
    }

    #[derive(Copy, Clone, Debug, Default, PartialEq, WidgetStyle)]
    pub struct Style {
        /// Color of the button's label.
        #[conrod(default = "theme.shape_color")]
        pub color: Option<conrod_core::Color>,
    }

    widget_ids! {
        struct Ids {
            button,
            list,
        }
    }

    pub struct State {
        ids: Ids,
    }

    impl<'a> CatalogGroup<'a> {
        /// Create a button context to be built upon.
        pub fn new(group: &'a Group) -> Self {
            CatalogGroup {
                common: widget::CommonBuilder::default(),
                style: Style::default(),
                group,
            }
        }
    }

    
    /// A custom Conrod widget must implement the Widget trait. See the **Widget** trait
    /// documentation for more details.
    impl<'a> Widget for CatalogGroup<'a> {
        /// The State struct that we defined above.
        type State = State;
        /// The Style struct that we defined using the `widget_style!` macro.
        type Style = Style;
        /// The event produced by instantiating the widget.
        ///
        /// `Some` when clicked, otherwise `None`.
        type Event = Option<()>;

        fn init_state(&self, id_gen: widget::id::Generator) -> Self::State {
            State { ids: Ids::new(id_gen) }
        }

        fn style(&self) -> Self::Style {
            self.style.clone()
        }

        fn is_over(&self) -> widget::IsOverFn {
            use conrod_core::graph::Container;
            use conrod_core::Theme;
            fn is_over_widget(widget: &Container, _: Point, _: &Theme) -> widget::IsOver {
                let unique = widget.state_and_style::<State, Style>().unwrap();
                unique.state.ids.button.into()
            }
            is_over_widget
        }

        /// Update the state of the button by handling any input that has occurred since the last
        /// update.
        fn update(self, args: widget::UpdateArgs<Self>) -> Self::Event {
            let widget::UpdateArgs { id, state, ui, style, .. } = args;

            let label = match &self.group.1 {
                Loadable::Loading => "loading".to_owned(),
                Loadable::Message(ref m) => m.to_owned(),
                Loadable::Ready(i) => format!("items: {}", i.len()),
                Loadable::ReadyEmpty => "empty".to_owned(),
            };
            let button = widget::Button::new()
                .label(&label)
                .label_color(conrod_core::color::WHITE)
                .color(style.color(&ui.theme))
                .set(state.ids.button, ui);
            /*
            if let Loadable::Ready(meta_items) = &self.group.1 {
                // @TODO: calculate dynamically
                let to_render_count = std::cmp::min(8, meta_items.len());
                let (mut items, _) = widget::List::flow_right(to_render_count)
                    .item_size(60.0)
                    .set(state.ids.list, ui);
                while let Some(item) = items.next(ui) {
                    let meta_item = &meta_items[item.i];
                    let button = widget::Button::new()
                        .label(&meta_item.name)
                        .label_color(conrod_core::color::WHITE)
                        .color(style.color(&ui.theme));
                    item.set(button, ui);
                    //for _v in item.set(toggle, ui) {
                    //    let action = Action::Load(ActionLoad::CatalogGrouped { extra: vec![] });
                    //    dispatch(action);
                    //};
                }
            }
            */
            ui.widget_input(id).clicks().left().next().map(|_| ()) 
        }

    }

    /// Provide the chainable color() configuration method.
    impl<'a> Colorable for CatalogGroup<'a> {
        fn color(mut self, color: conrod_core::Color) -> Self {
            self.style.color = Some(color);
            self
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
