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

struct AppSt {
    container: Arc<ContainerHolder>,
    is_dirty: AtomicBool,
}

const MAX_ACTION_BUFFER: usize = 1024;

fn main() {
    let container = Arc::new(ContainerHolder::new(CatalogGrouped::new()));

    let app = Arc::new(AppSt {
        container: container.clone(),
        is_dirty: AtomicBool::new(false),
    });

    #[derive(Debug, Clone)]
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

use azul::{prelude::*, widgets::{label::Label, button::Button}};
fn run_ui(app: Arc<AppSt>, dispatch: Box<Fn(Action)>) {
    // Trigger loading a catalog ASAP
    let action = Action::Load(ActionLoad::CatalogGrouped { extra: vec![] });
    dispatch(action);

    let mut app = App::new(DataModel { counter: 0 }, AppConfig::default()).unwrap();
    let window = app.create_window(WindowCreateOptions::default(), css::native()).unwrap();
    app.run(window).unwrap();
}

struct DataModel {
  counter: usize,
}

impl Layout for DataModel {
    fn layout(&self, _info: LayoutInfo<Self>) -> Dom<Self> {
        let label = Label::new(format!("{}", self.counter)).dom();
        let button = Button::with_label("Update counter").dom()
            .with_callback(On::MouseUp, Callback(update_counter));

        Dom::div()
            .with_child(label)
            .with_child(button)
    }
}

fn update_counter(app_state: &mut AppState<DataModel>, _: &mut CallbackInfo<DataModel>) -> UpdateScreen {
    app_state.data.modify(|state| state.counter += 1)?;
    Redraw
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
