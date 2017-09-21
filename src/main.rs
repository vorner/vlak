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
    }
}

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
                        MaybeResponse::Error { error } => Err(Error::from(ErrorKind::SlackError(error))),
                        MaybeResponse::Broken(value) => Err(Error::from(ErrorKind::UnknownFormat(value))),
                    }
                });
            Either::B(result)
        })
}

const CONNECT_URI: &str = "https://slack.com/api/rtm.start";

#[derive(Debug, Serialize)]
struct SimpleAuth {
    token: String,
}

#[derive(Debug, Deserialize)]
struct Team {
    domain: String,
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct User {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct WsAddr {
    #[serde(rename = "self")]
    me: User,
    team: Team,
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
enum Message {
    Message {
        user: String,
        text: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Notification {
    PresenceChange {
        presence: Presence,
        user: String,
    },
    Message {
        message: Message,
    }
}

struct CacheInner {
    client: Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>,
    connect_uri: Option<String>,
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
        self.0.borrow_mut().connect_uri = Some(ws_uri.url);
        await!(self.connect_uri())
    }
}

#[async]
fn connection(handle: Handle, cache: Cache) -> Result<()> {
    let uri = await!(cache.clone().connect_uri())?;
    let connection = ClientBuilder::new(&uri)?
        .async_connect_secure(None, &handle);
    let (connection, _headers) = await!(connection)?;
    let (sink, stream) = connection.split();
    let process = stream
        .filter_map(|msg| {
            match msg {
                OwnedMessage::Ping(data) => Some(OwnedMessage::Pong(data)),
                OwnedMessage::Text(text) => {
                    match serde_json::from_str::<Notification>(&text) {
                        Ok(notif) => println!("Notification {:?}", notif),
                        Err(_) => println!("Unknown msg '{}'", text),
                    }
                    None
                },
                _ => {
                    println!("Unknown msg {:?}", msg);
                    None
                },
            }
        })
        .forward(sink)
        .map(|_| ());
    Ok(await!(process)?)
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
