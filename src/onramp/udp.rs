// Copyright 2018-2019, Wayfair GmbH
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::onramp::prelude::*;
use mio::net::UdpSocket;
use mio::{Events, Poll, PollOpt, Ready, Token};
use serde_yaml::Value;
use std::thread;
use std::time::Duration;

const ONRAMP: Token = Token(0);

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    /// The port to listen on.
    pub port: u32,
    pub host: String,
    /*
    #[serde(default = "dflt::d_false")]
    pub close_on_done: bool,
    */
}

pub struct Udp {
    pub config: Config,
}

impl OnrampImpl for Udp {
    fn from_config(config: &Option<Value>) -> Result<Box<dyn Onramp>> {
        if let Some(config) = config {
            let config: Config = serde_yaml::from_value(config.clone())?;
            Ok(Box::new(Udp { config }))
        } else {
            Err("Missing config for blaster onramp".into())
        }
    }
}

fn onramp_loop(
    rx: Receiver<OnrampMsg>,
    config: Config,
    preprocessors: Vec<String>,
    codec: String,
) -> Result<()> {
    let mut codec = codec::lookup(&codec)?;
    let mut preprocessors = make_preprocessors(&preprocessors)?;

    // Limit of a UDP package
    let mut buf = [0; 65535];

    let mut pipelines: Vec<(TremorURL, PipelineAddr)> = Vec::new();
    let mut id = 0;

    info!("[UDP Onramp] listening on {}:{}", config.host, config.port);
    let poll = Poll::new()?;

    let socket = UdpSocket::bind(&format!("{}:{}", config.host, config.port).parse()?)?;
    poll.register(&socket, ONRAMP, Ready::readable(), PollOpt::edge())?;

    let mut events = Events::with_capacity(1024);
    loop {
        while pipelines.is_empty() {
            match rx.recv()? {
                OnrampMsg::Connect(mut ps) => pipelines.append(&mut ps),
                OnrampMsg::Disconnect { tx, .. } => {
                    let _ = tx.send(true);
                    return Ok(());
                }
            };
        }
        match rx.try_recv() {
            Err(TryRecvError::Empty) => (),
            Err(_e) => error!("Crossbream receive error"),
            Ok(OnrampMsg::Connect(mut ps)) => pipelines.append(&mut ps),
            Ok(OnrampMsg::Disconnect { id, tx }) => {
                pipelines.retain(|(pipeline, _)| pipeline != &id);
                if pipelines.is_empty() {
                    let _ = tx.send(true);
                    return Ok(());
                } else {
                    let _ = tx.send(false);
                }
            }
        };

        poll.poll(&mut events, Some(Duration::from_millis(100)))?;
        for event in events.iter() {
            match event.token() {
                // Our ECHOER is ready to be read from.
                ONRAMP => loop {
                    use std::io::ErrorKind;
                    match socket.recv(&mut buf) {
                        Ok(n) => {
                            send_event(
                                &pipelines,
                                &mut preprocessors,
                                &mut codec,
                                id,
                                buf[0..n].to_vec(),
                            );
                            id += 1;
                        }
                        Err(e) => {
                            if e.kind() == ErrorKind::WouldBlock {
                                break;
                            } else {
                                return Err(e.into());
                            }
                        }
                    }
                },
                _ => unreachable!(),
            }
        }
    }
}

impl Onramp for Udp {
    fn start(&mut self, codec: String, preprocessors: Vec<String>) -> Result<OnrampAddr> {
        let (tx, rx) = bounded(0);
        let config = self.config.clone();
        thread::Builder::new()
            .name(format!("onramp-udp-{}", "???"))
            .spawn(move || {
                if let Err(e) = onramp_loop(rx, config, preprocessors, codec) {
                    error!("[Onramp] Error: {}", e)
                }
            })?;
        Ok(tx)
    }
    fn default_codec(&self) -> &str {
        "string"
    }
}