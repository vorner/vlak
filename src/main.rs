#![feature(clippy, never_type, conservative_impl_trait, generators, plugin, proc_macro)]
#![recursion_limit = "256"]

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
extern crate simplelog;
extern crate websocket;

use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::fmt::{Debug, Display, Formatter, Result as FmtResult};
use std::fs::File;
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
use simplelog::{Config as LogConfig, LogLevelFilter, WriteLogger};
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
const USER_INFO_REQ:  &str = "https://slack.com/api/users.info";
const CHANNEL_INFO_REQ: &str = "https://slack.com/api/conversations.info";
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
        ts: String, // WTF? Why is it string if it can be float?
    },
    ReconnectUrl {
        url: String,
    },
    Hello,
    // UserChanged, ChannelChanged
    Error {
        code: i64,
        msg: String,
    },
}

impl Notification {
    #[async]
    fn into_event(self, cache: Cache) -> Result<Option<Event>> {
        let result = match self {
            Notification::PresenceChange { presence, user } => {
                let user = await!(cache.user(user))?;
                Some(Event::PresenceChange { presence, user })
            },
            Notification::Message { channel, user, text, .. } => {
                // TODO: The time stamp?
                let user = cache.clone().user(user);
                let channel = cache.channel(channel);
                let (user, channel) = await!(user.join(channel))?;
                Some(Event::Message{ user, channel, text })
            }
            _ => None,
        };
        Ok(result)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Profile {
    #[serde(default)]
    status_text: String,
    real_name_normalized: String,
    real_name: String,
    display_name_normalized: String,
    display_name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct User {
    id: String,
    team_id: String,
    profile: Profile,
    name: String,
    real_name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Channel {
    id: String,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChannelWrapper {
    channel: Channel,
}

#[derive(Debug)]
enum Event {
    PresenceChange {
        user: User,
        presence: Presence,
    },
    UserChange(User),
    Message {
        user: User,
        channel: Channel,
        text: String,
    }
}

impl Display for Event {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        match *self {
            Event::PresenceChange { ref user, ref presence } => {
                write!(f,
                       "@{} [{}] is now {:?}",
                       user.profile.display_name,
                       user.profile.real_name,
                       presence)
            },
            Event::Message { ref user, ref channel, ref text } => {
                let chname = channel.name
                    .as_ref()
                    .map(|name| format!("#{}\t", name))
                    .unwrap_or_else(String::new);
                write!(f,
                       "{}@{} [{}]: {}",
                       chname,
                       user.profile.display_name,
                       user.profile.real_name,
                       text)
            }
            _ => (self as &Debug).fmt(f)
        }
    }
}

struct CacheInner {
    client: Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>,
    connect_uri: Option<String>,
    bad_attempts: usize,
    token: String,
    users: HashMap<String, User>,
    channels: HashMap<String, Channel>,
}

#[derive(Clone)]
struct Cache(Rc<RefCell<CacheInner>>);

impl Cache {
    fn new(handle: Handle) -> Result<Self> {
        let token = env::args()
            .nth(1)
            .ok_or(ErrorKind::MissingToken)?;
        let client = Client::configure()
            .connector(HttpsConnector::new(2, &handle)?)
            .build(&handle);
        let inner = CacheInner {
            client,
            connect_uri: None,
            bad_attempts: 0,
            token,
            users: HashMap::new(),
            channels: HashMap::new(),
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
            let auth = SimpleAuth {
                token: me.token.clone()
            };
            request(&me.client, CONNECT_URI.parse()?, &auth)
        }; // Unlock self before going async
        let ws_uri: WsAddr = await!(ws_uri)?;
        let result = ws_uri.url.clone();
        self.0.borrow_mut().connect_uri = Some(ws_uri.url);
        Ok(result)
    }

    fn update_connect_url(&self, url: String) {
        let mut me = self.0.borrow_mut();
        me.connect_uri = Some(url);
        me.bad_attempts = 0;
    }

    fn bad_url(&self) -> Result<()> {
        let mut me = self.0.borrow_mut();
        me.connect_uri = None;
        me.bad_attempts += 1;
        if me.bad_attempts > MAX_ATTEPMS {
            Err(ErrorKind::BadAttempts.into())
        } else {
            Ok(())
        }
    }

    fn invalidate(&self) {
        let mut me = self.0.borrow_mut();
        me.users.clear();
    }

    #[async]
    fn user(self, id: String) -> Result<User> {
        let req = {
            let me = self.0.borrow();
            if let Some(user) = me.users.get(&id) {
                return Ok(user.clone());
            }
            let token = me.token.clone();
            #[derive(Debug, Serialize)]
            struct Req {
                user: String,
                token: String,
            }
            let req = Req {
                user: id.clone(),
                token,
            };
            request(&me.client, USER_INFO_REQ.parse()?, &req)
        };
        #[derive(Debug, Deserialize)]
        struct Wrapper { user: User }
        let Wrapper { user }  = await!(req)?;
        self.0.borrow_mut().users.insert(id, user.clone());
        Ok(user)
    }

    #[async]
    fn channel(self, id: String) -> Result<Channel> {
        let req = {
            let me = self.0.borrow();
            if let Some(channel) = me.channels.get(&id) {
                return Ok(channel.clone());
            }
            let token = me.token.clone();
            #[derive(Debug, Serialize)]
            struct Req {
                channel: String,
                token: String,
            }
            let req = Req {
                channel: id.clone(),
                token,
            };
            request(&me.client, CHANNEL_INFO_REQ.parse()?, &req)
        };
        #[derive(Debug, Deserialize)]
        struct Wrapper { channel: Channel }
        let Wrapper { channel }  = await!(req)?;
        self.0.borrow_mut().channels.insert(id, channel.clone());
        Ok(channel)
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
            OwnedMessage::Text(text) => {
                let notif = serde_json::from_str::<Notification>(&text);
                match notif {
                    Ok(Notification::ReconnectUrl { url }) => {
                        debug!("Updating connect url to {}", url);
                        cache.update_connect_url(url);
                    },
                    Ok(Notification::Error { msg,.. }) => {
                        error!("Error from slack: {}", msg);
                        return cache.bad_url();
                    },
                    Ok(notif) => {
                        info!("Notification {:?}", notif);
                        if let Some(event) = await!(notif.into_event(cache.clone()))? {
                            println!("{}", event);
                        }
                    },
                    Err(_) => warn!("Unknown msg '{}'", text),
                }
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
    let f = File::create("log.txt")?;
    WriteLogger::init(LogLevelFilter::Debug, LogConfig::default(), f)?;
    let mut core = Core::new()?;
    let handle = core.handle();
    core.run(keep_running(handle))
}

fn main() {
    let Err(e) = core_loop();
    eprintln!("{}", e);
    process::exit(1);
}
