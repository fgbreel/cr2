#![feature(generators, generator_trait)]

extern crate bs58;
extern crate byteorder;
extern crate clear_on_drop;
extern crate crc8;
extern crate ed25519_dalek;
extern crate rand;
extern crate sha2;
extern crate snow;
extern crate x25519_dalek;
#[macro_use]
extern crate log;
extern crate prost;
#[macro_use]
extern crate prost_derive;
extern crate bytes;
extern crate dirs;
extern crate hpack;
extern crate mio;
extern crate osaka;
extern crate osaka_dns;
extern crate serde;
extern crate toml;
#[macro_use]
extern crate serde_derive;
extern crate interfaces;
extern crate libc;

extern crate axon;
extern crate which;
extern crate nix;
extern crate mio_extras;
extern crate num_cpus;
extern crate wait_timeout;
extern crate mtdparts;

#[macro_use]
#[cfg(target_arch = "wasm32")]
extern crate wasm_bindgen;

pub mod channel;
pub mod clock;
pub mod config;
pub mod dns;
pub mod endpoint;
pub mod error;
pub mod headers;
pub mod identity;
pub mod local_addrs;
pub mod noise;
pub mod packet;
pub mod recovery;
pub mod replay;
pub mod stream;
pub mod util;
pub mod certificate;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "android",
))]
pub mod publisher;
pub mod subscriber;

pub use identity::Identity;
pub use identity::Secret;
pub use error::Error;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/carrier.broker.v1.rs"));
    include!(concat!(env!("OUT_DIR"), "/carrier.certificate.v1.rs"));
    include!(concat!(env!("OUT_DIR"), "/carrier.sysinfo.v1.rs"));
}
