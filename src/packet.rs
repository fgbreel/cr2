use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use error::Error;
use std::io::{Read, Write};

pub type RoutingKey = u64;

#[derive(Debug, Clone, PartialEq)]
pub enum RoutingDirection {
    Initiator2Responder,
    Responder2Initiator,
}

pub struct EncryptedPacket {
    pub version: u8,
    pub route: RoutingKey,
    pub direction: RoutingDirection,
    pub counter: u64,
    pub payload: Vec<u8>,
}

impl EncryptedPacket {
    pub fn decode(mut inbuf: &[u8]) -> Result<EncryptedPacket, Error> {
        let version = inbuf.read_u8()?;
        let mut reserved = [0; 3];
        inbuf.read_exact(&mut reserved)?;

        let mut route = [0; 8];
        inbuf.read_exact(&mut route)?;
        let direction = match route[7] & 0b00000001 {
            0 => RoutingDirection::Initiator2Responder,
            1 => RoutingDirection::Responder2Initiator,
            _ => unreachable!(),
        };
        route[7] &= 0b11111110;
        let route = route.as_ref().read_u64::<BigEndian>()?;
        let counter = inbuf.read_u64::<BigEndian>()?;

        if version != 0x08 || reserved != [0xff, 0xff, 0xff] {
            return Err(Error::InvalidVersion { version }.into());
        }

        let payload = inbuf.to_vec();

        Ok(EncryptedPacket {
            version,
            route,
            direction,
            counter,
            payload,
        })
    }

    pub fn encode(mut self) -> Vec<u8> {
        let mut w = [self.version].to_vec();
        w.extend_from_slice(&[0xff; 3]);

        let mut route = [0; 8];
        route.as_mut().write_u64::<BigEndian>(self.route).unwrap();
        match self.direction {
            RoutingDirection::Initiator2Responder => route[7] &= 0b11111110,
            RoutingDirection::Responder2Initiator => route[7] |= 0b00000001,
        };
        w.write(&route).unwrap();
        w.write_u64::<BigEndian>(self.counter).unwrap();
        w.append(&mut self.payload);
        w
    }
}

#[test]
fn decode_with_payload() {
    let pl = EncryptedPacket::decode(&[
        0x08, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // routing key
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // packet counter
        0xf0, 0x0d, // payload
    ])
    .unwrap();
    assert_eq!(pl.payload.as_slice(), &[0xf0, 0x0d]);
}

#[test]
fn decode_invalid_packets() {
    assert!(EncryptedPacket::decode(&[]).is_err());
    assert!(EncryptedPacket::decode(&[0; 128]).is_err());
    assert!(EncryptedPacket::decode(&[0x08; 128]).is_err());
}

#[derive(PartialEq)]
pub enum Frame {
    Header {
        stream: u32,
        payload: Vec<u8>,
    },
    Stream {
        stream: u32,
        order: u64,
        payload: Vec<u8>,
    },
    Ack {
        delay: u64,
        acked: Vec<u64>,
    },
    Ping,
    Disconnect,
    Close {
        stream: u32,
        order: u64,
    },
    Config {
        timeout: Option<u16>,
        sleeping: bool,
    },
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Frame::Header { stream, payload } => {
                write!(f, "Header[s:{},p:{}]", stream, payload.len())
            }
            Frame::Stream {
                stream,
                order,
                payload,
            } => write!(f, "Stream[s:{},o:{},p:{}]", stream, order, payload.len()),
            Frame::Ack { delay, acked } => write!(f, "Ack[d:{},a:{}]", delay, acked.len()),
            Frame::Ping => write!(f, "Ping"),
            Frame::Disconnect => write!(f, "Disconnect"),
            Frame::Close { stream, order } => write!(f, "Close[s:{},o:{}]", stream, order),
            Frame::Config { timeout, sleeping } => {
                write!(f, "Close[t:{:?},s:{}]", timeout, sleeping)
            }
        }
    }
}

impl Frame {
    pub fn len(&self) -> usize {
        match self {
            Frame::Header { payload, .. } => 1 + 4 + 2 + payload.len(),
            Frame::Stream { payload, .. } => 1 + 4 + 8 + 2 + payload.len(),
            Frame::Ack { acked, .. } => 1 + 2 + 2 + 8 * acked.len(),
            Frame::Ping => 1,
            Frame::Disconnect => 1,
            Frame::Close { .. } => 1 + 4 + 8,
            Frame::Config { timeout, .. } => 1 + 1 + 2 + if timeout.is_some() { 2 } else { 0 },
        }
    }

    pub fn is_ping(&self) -> bool {
        match self {
            Frame::Ping { .. } => true,
            _ => false,
        }
    }
    pub fn is_ack(&self) -> bool {
        match self {
            Frame::Ack { .. } => true,
            _ => false,
        }
    }

    pub fn order(&self) -> u64 {
        match self {
            Frame::Header { .. } => 1,
            Frame::Stream { order, .. } => *order,
            Frame::Close { order, .. } => *order,
            _ => panic!("trying to order unordered frame"),
        }
    }

    pub fn encode<W: Write>(&self, mut w: W) -> Result<usize, Error> {
        let len = self.len();
        match self {
            Frame::Header { stream, payload } => {
                assert!(payload.len() + 12 < u16::max_value() as usize);
                w.write_u8(0x04)?;
                w.write_u32::<BigEndian>(*stream)?;
                w.write_u16::<BigEndian>(payload.len() as u16)?;
                assert_eq!(w.write(payload)?, payload.len());
            }
            Frame::Stream {
                stream,
                order,
                payload,
            } => {
                assert!(payload.len() + 12 < u16::max_value() as usize);
                w.write_u8(0x05)?;
                w.write_u32::<BigEndian>(*stream)?;
                w.write_u64::<BigEndian>(*order)?;
                w.write_u16::<BigEndian>(payload.len() as u16)?;
                assert_eq!(w.write(payload)?, payload.len());
            }
            Frame::Ack { delay, acked } => {
                assert!(acked.len() < u16::max_value() as usize / 8);
                w.write_u8(0x01)?;
                w.write_u16::<BigEndian>(*delay as u16)?;
                w.write_u16::<BigEndian>(acked.len() as u16)?;
                let mut acked = acked.clone();
                acked.sort_unstable();
                for ack in acked {
                    w.write_u64::<BigEndian>(ack)?;
                }
            }
            Frame::Ping => {
                w.write_u8(0x02)?;
            }
            Frame::Disconnect => {
                w.write_u8(0x03)?;
            }
            Frame::Close { stream, order } => {
                w.write_u8(0x06)?;
                w.write_u32::<BigEndian>(*stream)?;
                w.write_u64::<BigEndian>(*order)?;
            }
            Frame::Config { timeout, sleeping } => {
                w.write_u8(0x07)?;
                let mut flags: u8 = 0x00;
                let mut datalen: u16 = 0;

                if let Some(_) = timeout {
                    flags |= 0b10000000;
                    datalen += 2;
                }

                if *sleeping {
                    flags |= 0b01000000;
                }

                w.write_u8(flags)?;
                w.write_u16::<BigEndian>(datalen)?;

                if let Some(timeout) = timeout {
                    w.write_u16::<BigEndian>(*timeout)?;
                }
            }
        }
        Ok(len)
    }

    pub fn decode<R: Read>(mut r: R) -> Result<Vec<Frame>, Error> {
        let mut f = Vec::new();

        loop {
            match r.read_u8() {
                Err(_) => return Ok(f),
                Ok(0x00) => (),
                Ok(0x01) => {
                    let delay = r.read_u16::<BigEndian>()? as u64;
                    let count = r.read_u16::<BigEndian>()?;
                    let mut acked = Vec::new();
                    for _ in 0..count {
                        acked.push(r.read_u64::<BigEndian>()?);
                    }
                    f.push(Frame::Ack { delay, acked });
                }
                Ok(0x02) => {
                    f.push(Frame::Ping);
                }
                Ok(0x03) => {
                    f.push(Frame::Disconnect);
                }
                Ok(0x04) => {
                    let stream = r.read_u32::<BigEndian>()?;
                    let len = r.read_u16::<BigEndian>()?;
                    let mut payload = vec![0; len as usize];
                    r.read_exact(&mut payload)?;
                    f.push(Frame::Header { stream, payload });
                }
                Ok(0x05) => {
                    let stream = r.read_u32::<BigEndian>()?;
                    let order = r.read_u64::<BigEndian>()?;
                    let len = r.read_u16::<BigEndian>()?;
                    let mut payload = vec![0; len as usize];
                    r.read_exact(&mut payload)?;
                    f.push(Frame::Stream {
                        stream,
                        order,
                        payload,
                    });
                }
                Ok(0x06) => {
                    let stream = r.read_u32::<BigEndian>()?;
                    let order = r.read_u64::<BigEndian>()?;
                    f.push(Frame::Close { stream, order });
                }
                Ok(0x07) => {
                    let flags = r.read_u8()?;
                    let datalen = r.read_u16::<BigEndian>()?;

                    let mut data = vec![0; datalen as usize];
                    r.read_exact(&mut data)?;
                    let mut r = &data[..];

                    let timeout = if flags & 0b10000000 > 0 {
                        Some(r.read_u16::<BigEndian>()?)
                    } else {
                        None
                    };

                    let sleeping = flags & 0b01000000 > 0;

                    f.push(Frame::Config { timeout, sleeping });
                }
                Ok(typ) => return Err(Error::InvalidFrameType { typ }.into()),
            };
        }
    }
}

#[test]
fn config_frames() {
    let frame = Frame::Config {
        timeout: None,
        sleeping: false,
    };
    let mut w = Vec::new();
    let written = frame.encode(&mut w).unwrap();
    assert_eq!(written, w.len());
    assert_eq!(w, &[0x07, 0x00, 0x00, 0x00]);

    let frames = Frame::decode(&w[..]).unwrap();
    assert_eq!(frames.len(), 1);
    if let Frame::Config {
        timeout: None,
        sleeping: false,
    } = frames[0]
    {
    } else {
        assert!(false, "expected config frame");
    }

    let frame = Frame::Config {
        timeout: Some(1292),
        sleeping: true,
    };
    let mut w = Vec::new();
    let written = frame.encode(&mut w).unwrap();
    assert_eq!(written, w.len());
    assert_eq!(w, &[0x07, 0b11000000, 0, 2, 5, 12]);

    let frames = Frame::decode(&w[..]).unwrap();
    assert_eq!(frames.len(), 1);
    if let Frame::Config {
        timeout: Some(1292),
        sleeping: true,
    } = frames[0]
    {
    } else {
        assert!(false, "expected config frame");
    }
}

#[test]
fn encode_frame() {
    let frame = Frame::Stream {
        order: 0x1223,
        payload: b"hello".to_vec(),
        stream: 0x63,
    };

    let mut w = Vec::new();
    let written = frame.encode(&mut w).unwrap();
    assert_eq!(written, w.len());
    assert_eq!(
        w,
        &[
            0x05, 0x00, 0x00, 0x00, 0x63, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x12, 0x23, 0x00,
            0x05, b'h', b'e', b'l', b'l', b'o'
        ]
    );

    let frame = Frame::Ack {
        delay: 0x01,
        acked: vec![0x872],
    };
    let mut w = Vec::new();
    let written = frame.encode(&mut w).unwrap();
    assert_eq!(written, w.len());
    assert_eq!(written, 1 + 2 + 2 + 8);
    assert_eq!(
        w,
        &[0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x72,]
    );
}

#[test]
fn decode_frame() {
    let r = [
        0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x63, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x12,
        0x23, 0x00, 0x05, b'h', b'e', b'l', b'l', b'o', 0x00, 0x01, 0x00, 0x05, 0x00, 0x02, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x12, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x12, 0x23,
        0x00, 0x00, 0x00,
    ];

    let frames = Frame::decode(&r[..]).unwrap();
    assert_eq!(frames.len(), 2);
    if let Frame::Stream {
        order,
        ref payload,
        stream,
    } = frames[0]
    {
        assert_eq!(order, 0x1223);
        assert_eq!(payload, b"hello");
        assert_eq!(stream, 0x63);
    } else {
        assert!(false, "expected stream frame");
    }
    if let Frame::Ack { delay, ref acked } = frames[1] {
        assert_eq!(delay, 0x05);
        assert_eq!(acked, &[0x1224, 0x1223]);
    } else {
        assert!(false, "expected ack frame");
    }
}
