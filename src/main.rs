use stremio_state_ng::middlewares::*;
use stremio_state_ng::state_types::*;
use futures::{future, Future};
use futures::future::lazy;
use tokio::runtime::current_thread::Runtime;
use tokio::executor::current_thread::spawn;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::cell::RefCell;
use std::rc::Rc;

fn main() {
    let container = Rc::new(RefCell::new(Container::with_reducer(
        CatalogGrouped::new(),
        &catalogs_reducer,
    )));
    #[derive(Debug)]
    enum ContainerId {
        Board,
    };
    let muxer = Rc::new(ContainerMuxer::new(
        vec![
            Box::new(ContextMiddleware::<Env>::new()),
            Box::new(AddonsMiddleware::<Env>::new()),
        ],
        vec![(ContainerId::Board, container.clone())],
        Box::new(|_event| {
            if let Event::NewState(_) = _event {
                dbg!(_event);
            }
        }),
    ));

    let mut rt = Runtime::new().expect("failed to create tokio runtime");
    rt.spawn(lazy(move || {
        // this is the dispatch operation
        let action = &Action::Load(ActionLoad::CatalogGrouped { extra: vec![] });
        muxer.dispatch(action);
        future::ok(())
    }));
    rt.run().expect("failed to run tokio runtime");
}

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
