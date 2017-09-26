use std::io::Error as IoError;
use std::num::ParseFloatError;

use hyper::StatusCode;
use hyper::error::{Error as HyperError, UriError};
use log::SetLoggerError;
use native_tls::Error as NativeTlsError;
use serde_json::{Error as JsonError, Value};
use serde_urlencoded::ser::Error as UrlSerError;
use term::Error as TermError;
use websocket::client::ParseError;
use websocket::WebSocketError;

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
        Term(TermError);
        ParseFloat(ParseFloatError);
    }

    errors {
        MissingToken {
            description("Missing the token parameter")
            display("Missing the token parameter")
        }
        MissingTerm {
            description("Missing the terminal")
            display("Missing the terminal")
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

