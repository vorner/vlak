#![feature(conservative_impl_trait)]
extern crate env_logger;
#[macro_use]
extern crate error_chain;
extern crate futures;
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

use std::env;
use std::fmt::Debug;
use std::io::Error as IoError;
use std::process;

use futures::{Future, IntoFuture, Stream};
use futures::future::Either;
use hyper::{Client, Method, Request, StatusCode, Uri};
use hyper::client::Connect;
use hyper::header::{ContentLength, ContentType};
use hyper::error::{Error as HyperError, UriError};
use hyper_tls::HttpsConnector;
use log::SetLoggerError;
use native_tls::Error as NativeTlsError;
use tokio_core::reactor::Core;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Error as JsonError, Value};
use serde_urlencoded::ser::Error as UrlSerError;

error_chain! {
    foreign_links {
        Hyper(HyperError);
        Io(IoError);
        Json(JsonError);
        NativeTls(NativeTlsError);
        Log(SetLoggerError);
        Uri(UriError);
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

const CONNECT_URI: &str = "https://slack.com/api/rtm.connect";

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

fn run() -> Result<()> {
    env_logger::init()?;
    let token = env::args()
        .nth(1)
        .ok_or(ErrorKind::MissingToken)?;
    info!("Starting up");
    let mut core = Core::new()?;
    let handle = core.handle();
    let client = Client::configure()
        .connector(HttpsConnector::new(2, &handle)?)
        .build(&handle);
    let auth = SimpleAuth {
        token
    };
    let ws_uri = request(&client, CONNECT_URI.parse()?, &auth);
    let ws_uri: WsAddr = core.run(ws_uri)?;
    println!("{:?}", ws_uri);
    Ok(())
}

fn main() {
    match run() {
        Ok(()) => (),
        Err(e) => {
            eprintln!("{}", e);
            process::exit(1);
        }
    }
}
