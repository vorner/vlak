use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Debug;
use std::rc::Rc;

use chrono::{DateTime, Local, TimeZone};
use futures::future::Either;
use futures::prelude::*;
use futures::stream::Stream;
use futures::unsync::mpsc::{self, UnboundedSender};
use hyper::{Client, Method, Request, StatusCode, Uri};
use hyper::client::{Connect, HttpConnector};
use hyper::header::{ContentLength, ContentType};
use hyper_tls::HttpsConnector;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{self, Value};
use serde_urlencoded;
use tokio_core::reactor::Handle;
use websocket::client::ClientBuilder;
use websocket::OwnedMessage;

use error::{Error, ErrorKind, Result};

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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Active,
    Away,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Profile {
    #[serde(default)]
    status_text: String,
    real_name_normalized: String,
    pub real_name: String,
    display_name_normalized: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    id: String,
    team_id: String,
    pub profile: Profile,
    pub name: String,
    real_name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Channel {
    id: String,
    pub name: Option<String>,
}

#[derive(Debug)]
pub enum Event {
    PresenceChange {
        user: User,
        presence: Presence,
    },
    Message {
        user: User,
        channel: Channel,
        text: String,
        time: DateTime<Local>,
    }
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
    fn into_event(self, account: Account) -> Result<Option<Event>> {
        let result = match self {
            Notification::PresenceChange { presence, user } => {
                let user = await!(account.user(user))?;
                Some(Event::PresenceChange { presence, user })
            },
            Notification::Message { channel, user, text, ts } => {
                // TODO: The time stamp?
                let user = account.clone().user(user);
                let channel = account.channel(channel);
                let (user, channel) = await!(user.join(channel))?;
                let ts = ts.parse::<f64>()?;
                let seconds = ts.trunc() as i64;
                let nanos = (ts.fract() * 1_000_000_000.0).round() as u32;
                let time = Local.timestamp(seconds, nanos);
                Some(Event::Message{
                     user,
                     channel,
                     text,
                     time,
                })
            }
            _ => None,
        };
        Ok(result)
    }
}

struct AccountInternal {
    handle: Handle,
    token: String,
    client: Client<HttpsConnector<HttpConnector>>,
    connect_uri: Option<String>,
    bad_attempts: usize,
    user: HashMap<String, User>,
    channel: HashMap<String, Channel>,
    event_streams: Vec<UnboundedSender<Rc<Event>>>,
}

#[derive(Clone)]
pub struct Account(Rc<RefCell<AccountInternal>>);

macro_rules! cached {
    ($name: ident, $res: ty, $uri: expr) => {
        #[async]
        fn $name(self, id: String) -> Result<$res> {
            let req = {
                let me = self.0.borrow();
                if let Some(val) = me.$name.get(&id) {
                    return Ok(val.clone());
                }
                let token = me.token.clone();
                #[derive(Debug, Serialize)]
                struct Req {
                    $name: String,
                    token: String,
                }
                let req = Req {
                    $name: id.clone(),
                    token,
                };
                request(&me.client, $uri.parse()?, &req)
            };
            #[derive(Debug, Deserialize)]
            struct Wrapper { $name: $res }
            let Wrapper { $name }  = await!(req)?;
            self.0.borrow_mut().$name.insert(id, $name.clone());
            Ok($name)
        }
    };
}

impl Account {
    pub fn new(handle: Handle, token: String) -> Result<Account> {
        let client = Client::configure()
            .connector(HttpsConnector::new(2, &handle)?)
            .build(&handle);
        let result = AccountInternal {
            handle,
            token,
            client,
            connect_uri: None,
            bad_attempts: 0,
            user: HashMap::new(),
            channel: HashMap::new(),
            event_streams: Vec::new(),
        };
        Ok(Account(Rc::new(RefCell::new(result))))
    }

    fn invalidate(&self) {
        let mut me = self.0.borrow_mut();
        me.user.clear();
        me.channel.clear();
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

    #[async]
    fn connect_uri(self) -> Result<String> {
        let ws_uri = {
            let me = self.0.borrow();
            if let Some(ref uri) = me.connect_uri {
                return Ok(uri.clone());
            }
            info!("Requesting address");
            #[derive(Debug, Serialize)]
            struct Auth {
                token: String,
            }
            let auth = Auth {
                token: me.token.clone()
            };
            request(&me.client, CONNECT_URI.parse()?, &auth)
        }; // Unlock self before going async
        #[derive(Debug, Deserialize)]
        struct WsAddr {
            url: String,
        }
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

    #[async]
    fn connection(self) -> Result<()> {
        self.invalidate();
        let uri = await!(self.clone().connect_uri())?;
        debug!("Connecting to {}", uri);
        let connection = ClientBuilder::new(&uri)?
            .async_connect_secure(None, &self.0.borrow().handle);
        let connection = match await!(connection) {
            Ok((connection, _headers)) => connection,
                Err(e) => {
                    error!("Connection error: {}", e);
                    return self.bad_url();
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
                            self.update_connect_url(url);
                        },
                        Ok(Notification::Error { msg,.. }) => {
                            error!("Error from slack: {}", msg);
                            return self.bad_url();
                        },
                        Ok(notif) => {
                            info!("Notification {:?}", notif);
                            if let Some(event) = await!(notif.into_event(self.clone()))? {
                                self.broadcast(event);
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
    pub fn keep_running(self) -> Result<!> {
        loop {
            await!(self.clone().connection())?;
        }
    }

    pub fn events(&self) -> impl Stream<Item = Rc<Event>, Error = Error> {
        let (sender, receiver) = mpsc::unbounded();
        self.0.borrow_mut().event_streams.push(sender);
        receiver.map_err(|()| panic!("No error possible"))
    }

    pub fn broadcast(&self, event: Event) {
        let ev = Rc::new(event);
        self.0
            .borrow_mut()
            .event_streams.
            retain(|sender| sender.unbounded_send(Rc::clone(&ev)).is_ok());
    }

    cached!(user, User, USER_INFO_REQ);
    cached!(channel, Channel, CHANNEL_INFO_REQ);
}
