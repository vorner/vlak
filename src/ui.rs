use std::rc::Rc;

use futures::Stream;
use futures::prelude::*;
use term::{self, color};

use account::Event;
use error::{Error, ErrorKind, Result};

pub struct Ui<S> {
    stream: S,
}

impl<S: Stream<Item = Rc<Event>, Error = Error> + 'static> Ui<S> {
    pub fn new(stream: S) -> Self {
        Ui {
            stream,
        }
    }

    #[async]
    pub fn run(self) -> Result<!> {
        let mut t = term::stdout().ok_or(ErrorKind::MissingTerm)?;
        t.bg(color::BLACK)?;
        t.fg(color::WHITE)?;
        writeln!(t, "Starting up… log mode")?;
        t.reset()?;
        writeln!(t)?;
        #[async]
        for event in self.stream {
            t.cursor_up()?;
            t.carriage_return()?;
            t.delete_line()?;
            match *event {
                Event::PresenceChange { ref user, ref presence } => {
                    t.fg(color::MAGENTA)?;
                    writeln!(t, "@{} [{}] is now {:?}",
                             user.profile.display_name,
                             user.profile.real_name,
                             presence)?;
                    t.reset()?;
                },
                Event::Message { ref channel, ref user, ref text } => {
                    t.fg(color::RED)?;
                    let chname = channel.name
                        .as_ref()
                        .map(|name| format!("#{}\t", name))
                        .unwrap_or_else(String::new);
                    write!(t, "{}", chname)?;
                    t.fg(color::BLUE)?;
                    write!(t, "@{} [{}]: ",
                           user.profile.display_name,
                           user.profile.real_name)?;
                    t.reset()?;
                    writeln!(t, "{}", text)?;
                },
                ref other => {
                    writeln!(t, "{:?}", other)?;
                }
            }
            writeln!(t, "Status: <unsupported>")?;
        }
        panic!("Stream ended while it should be infinite");
    }
}
