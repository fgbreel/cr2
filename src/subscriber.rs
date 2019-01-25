use error::Error;
use config::{Config};
use osaka::{osaka, Poll};
use endpoint;
use headers;
use identity;
use proto;
use prost::Message;
use std::cell::RefCell;
use std::rc::Rc;

pub struct SubscriberBuilder {
    config:         Config,
    on_publish:     Option<Box<FnMut(identity::Identity)>>,
    on_unpublish:   Option<Box<FnMut(identity::Identity)>>,
}

pub fn new(config: Config) -> SubscriberBuilder{
    SubscriberBuilder{
        config,
        on_unpublish:   None,
        on_publish:     None,
    }
}


impl SubscriberBuilder {

    #[osaka]
    fn handler(this: Rc<RefCell<Self>>, _poll: Poll, mut stream: endpoint::Stream) {

        let m = osaka::sync!(stream);
        let headers = headers::Headers::decode(&m).unwrap();
        info!("pubres: {:?}", headers);

        loop {
            let v = match proto::SubscribeChange::decode(osaka::sync!(stream)) {
                Err(e) => {
                    warn!("{}", e);
                    return;
                }
                Ok(v) => v,
            };

            match v.m {
                Some(proto::subscribe_change::M::Publish(proto::Publish{identity, xaddr})) => {
                    if let Some(h) = &mut this.borrow_mut().on_publish {
                        match identity::Identity::from_bytes(&identity) {
                            Ok(v) => {
                                h(v);
                            }
                            Err(e) => {
                                warn!("SubscribeChange::Publish: {}", e);
                            }
                        };
                    }
                },
                Some(proto::subscribe_change::M::Unpublish(proto::Unpublish{identity})) => {
                    if let Some(h) = &mut this.borrow_mut().on_unpublish {
                        match identity::Identity::from_bytes(&identity) {
                            Ok(v) => {
                                h(v);
                            }
                            Err(e) => {
                                warn!("SubscribeChange::Publish: {}", e);
                            }
                        };
                    }
                },
                Some(proto::subscribe_change::M::Supersede(_)) => {
                    warn!("subscriber superseded");
                    return;
                }
                None => (),
            }
        }
    }


    pub fn on_unpublish<F>(mut self, f:F) -> Self
        where F: 'static + Fn(identity::Identity)
    {
        self.on_unpublish = Some(Box::new(f));
        self
    }

    pub fn on_publish<F>(mut self, f:F) -> Self
        where F: 'static + Fn(identity::Identity)
    {
        self.on_publish = Some(Box::new(f));
        self
    }

    #[osaka]
    pub fn subscribe(self, poll: Poll, shadow: identity::Address) -> Result<(), Error> {
        let mut ep = endpoint::EndpointBuilder::new(&self.config)?.connect(poll.clone());
        let mut ep = osaka::sync!(ep)?;

        let this = Rc::new(RefCell::new(self));

        let broker = ep.broker();
        ep.open(
            broker,
            headers::Headers::with_path("/carrier.broker.v1/broker/subscribe"),
            |poll, mut stream| {
                stream.small_message(proto::SubscribeRequest {
                    shadow: shadow.as_bytes().to_vec(),
                    filter: Vec::new(),
                });
                Self::handler(this.clone(), poll, stream)
            },
        );

        loop {
            match osaka::sync!(ep)? {
                endpoint::Event::Disconnect{..} => (),
                endpoint::Event::OutgoingConnect(_) => (),
                endpoint::Event::IncommingConnect(q) => {
                    info!("ignoring incomming connect {}", q.identity);
                }
            };
        }
    }
}
