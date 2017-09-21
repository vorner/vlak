#![feature(clippy, never_type, conservative_impl_trait, generators, plugin, proc_macro)]
#![recursion_limit = "256"]

extern crate env_logger;
#[macro_use]
extern crate error_chain;
extern crate futures_await as futures;
extern crate hyper;
extern crate hyper_tls;
#[macro_use]
extern crate log;
extern crate native_tls;
extern crate tokio_core;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate serde_urlencoded;
extern crate websocket;

use std::cell::RefCell;
use std::env;
use std::fmt::Debug;
use std::io::Error as IoError;
use std::process;
use std::rc::Rc;

use futures::{Future, IntoFuture, Stream};
use futures::future::Either;
use futures::prelude::*;
use hyper::{Client, Method, Request, StatusCode, Uri};
use hyper::client::Connect;
use hyper::header::{ContentLength, ContentType};
use hyper::error::{Error as HyperError, UriError};
use hyper_tls::HttpsConnector;
use log::SetLoggerError;
use native_tls::Error as NativeTlsError;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Error as JsonError, Value};
use serde_urlencoded::ser::Error as UrlSerError;
use tokio_core::reactor::{Core, Handle};
use websocket::client::{ClientBuilder, ParseError};
use websocket::{OwnedMessage, WebSocketError};

error_chain! {
    foreign_links {
        Hyper(HyperError);
        Io(IoError);
        Json(JsonError);
        NativeTls(NativeTlsError);
        Log(SetLoggerError);
        Uri(UriError);
        WSUriError(ParseError);
        WSError(WebSocketError);
        UrlSerError(UrlSerError);
    }

    errors {
        MissingToken {
            description("Missing the token parameter")
            display("Missing the token parameter")
        }
        HttpError(status: StatusCode) {
            description("Unexpected HTTP response")
            display("Unexpected HTTP response {}", status)
        }
        SlackError(error: String) {
            description("Error from Slack")
            display("Error from Slack: {}", error)
        }
        UnknownFormat(value: Value) {
            description("Unknown response format")
            display("Unknown response format: {}", value)
        }
        BadAttempts {
            description("Too many failed connection attempts")
            display("Too many failed connection attempts")
        }
    }
}

const CONNECT_URI: &str = "https://slack.com/api/rtm.start";
const MAX_ATTEPMS: usize = 3;

fn request_build<Req: Serialize + Debug>(uri: Uri, req: &Req) -> Result<Request> {
    trace!("Building request {:?}/{:?}", uri, req);
    let body = serde_urlencoded::to_string(req)?;
    let mut request = Request::new(Method::Post, uri);
    request.headers_mut().set(ContentType::form_url_encoded());
    request.headers_mut().set(ContentLength(body.len() as u64));
    request.set_body(body);
    Ok(request)
}

fn request<C, Req, Resp>(client: &Client<C>, uri: Uri, req: &Req)
    -> impl Future<Item = Resp, Error = Error>
where
    C: Connect,
    Req: Serialize + Debug,
    Resp: DeserializeOwned + Debug + 'static,
{
    request_build(uri, req)
        // We got a request, this one sends it
        .map(|request| {
             client
                .request(request)
                .map_err(Error::from)
        })
        // Result<…> → FutureResult<…> (just a formality)
        .into_future()
        // We have Result<Future<…>>, we want Future<…>
        .flatten()
        // Check what arrived
        .and_then(|response| {
            trace!("Received response {:?}", response);
            // Sanity check of the response
            let status = response.status();
            if status != StatusCode::Ok {
                // We want to return an error right away.
                // But this wrapping is a bit… uncomfortable
                return Either::A(Err(ErrorKind::HttpError(status).into()).into_future());
            }
            // Technically, we probably should check that it's claimed to be JSON.
            let result = response
                // Wait for the whole response
                .body()
                .concat2()
                .map_err(Error::from)
                // Parse it once the whole thing arrives
                .and_then(|body| {
                    #[derive(Debug, Deserialize)]
                    #[serde(untagged)]
                    enum MaybeResponse<R> {
                        Correct(R),
                        Error {
                            error: String,
                        },
                        Broken(Value),
                    }
                    let response = serde_json::from_slice::<MaybeResponse<Resp>>(&body)?;
                    trace!("Parsed response {:?}", response);
                    match response {
                        MaybeResponse::Correct(resp) => Ok(resp),
                        MaybeResponse::Error { error } => {
                            Err(Error::from(ErrorKind::SlackError(error)))
                        },
                        MaybeResponse::Broken(value) => {
                            Err(Error::from(ErrorKind::UnknownFormat(value)))
                        },
                    }
                });
            Either::B(result)
        })
}

#[derive(Debug, Serialize)]
struct SimpleAuth {
    token: String,
}

#[derive(Debug, Deserialize)]
struct WsAddr {
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Presence {
    Active,
    Away,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Notification {
    PresenceChange {
        presence: Presence,
        user: String,
    },
    Message {
        channel: String,
        user: String,
        text: String,
        ts: f64,
    },
    ReconnectUrl {
        url: String,
    },
    Hello,
    Error {
        code: i64,
        msg: String,
    },
}

impl Notification {
    #[async]
    fn into_action(self) -> Result<Option<Event>> {
        match self {
            _ => (),
        }
        Ok(None)
    }
}

#[derive(Debug, Serialize)]
struct UserInfoReq {
    token: String,
    user: String,
}

#[derive(Debug, Deserialize)]
struct User {

}

struct Channel {

}

enum Event {
    UserChange(User),
    Message {
        user: User,
        channel: Channel,
        text: String,
    }
}

struct CacheInner {
    client: Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>,
    connect_uri: Option<String>,
    bad_attempts: usize,
}

#[derive(Clone)]
struct Cache(Rc<RefCell<CacheInner>>);

impl Cache {
    fn new(handle: Handle) -> Result<Self> {
        let client = Client::configure()
            .connector(HttpsConnector::new(2, &handle)?)
            .build(&handle);
        let inner = CacheInner {
            client,
            connect_uri: None,
            bad_attempts: 0,
        };
        Ok(Cache(Rc::new(RefCell::new(inner))))
    }

    #[async]
    fn connect_uri(self) -> Result<String> {
        let ws_uri = {
            let me = self.0.borrow();
            if let Some(ref uri) = me.connect_uri {
                return Ok(uri.clone());
            }
            info!("Requesting address");
            let token = env::args()
                .nth(1)
                .ok_or(ErrorKind::MissingToken)?;
            let auth = SimpleAuth {
                token
            };
            request(&me.client, CONNECT_URI.parse()?, &auth)
        };
        let ws_uri: WsAddr = await!(ws_uri)?;
        let result = ws_uri.url.clone();
        self.0.borrow_mut().connect_uri = Some(ws_uri.url);
        Ok(result)
    }

    fn update_connect_url(&self, url: String) {
        let mut borrow = self.0.borrow_mut();
        borrow.connect_uri = Some(url);
        borrow.bad_attempts = 0;
    }

    fn bad_url(&self) -> Result<()> {
        let mut borrow = self.0.borrow_mut();
        borrow.connect_uri = None;
        borrow.bad_attempts += 1;
        if borrow.bad_attempts > MAX_ATTEPMS {
            Err(ErrorKind::BadAttempts.into())
        } else {
            Ok(())
        }
    }

    fn invalidate(&self) {
        // Nothing to invalidate yet. But some info may become stale on reconnect, so wipe it then.
    }
}

#[async]
fn connection(handle: Handle, cache: Cache) -> Result<()> {
    cache.invalidate();
    let uri = await!(cache.clone().connect_uri())?;
    debug!("Connecting to {}", uri);
    let connection = ClientBuilder::new(&uri)?
        .async_connect_secure(None, &handle);
    let connection = match await!(connection) {
        Ok((connection, _headers)) => connection,
        Err(e) => {
            error!("Connection error: {}", e);
            return cache.bad_url();
        }
    };
    let (sink, stream) = connection.split();
    let mut sink = sink;
    #[async]
    for msg in stream {
        trace!("Unparsed: {:?}", msg);
        match msg {
            OwnedMessage::Ping(data) => {
                sink = await!(sink.send(OwnedMessage::Pong(data)))?;
            },
            OwnedMessage::Text(text) => match serde_json::from_str::<Notification>(&text) {
                Ok(Notification::ReconnectUrl { url }) => {
                    debug!("Updating connect url to {}", url);
                    cache.update_connect_url(url);
                },
                Ok(Notification::Error { msg,.. }) => {
                    error!("Error from slack: {}", msg);
                    return cache.bad_url();
                },
                Ok(notif) => info!("Notification {:?}", notif),
                Err(_) => warn!("Unknown msg '{}'", text),
            },
            _ => warn!("Unknown msg {:?}", msg),
        }
    }
    Ok(())
}

#[async]
fn keep_running(handle: Handle) -> Result<!> {
    let cache = Cache::new(handle.clone())?;
    loop {
        await!(connection(handle.clone(), cache.clone()))?;
    }
}

fn core_loop() -> Result<!> {
    env_logger::init()?;
    let mut core = Core::new()?;
    let handle = core.handle();
    core.run(keep_running(handle))
}

fn main() {
    let Err(e) = core_loop();
    eprintln!("{}", e);
    process::exit(1);
}
