use crate::database::{DbStream, StreamId, StreamKind};

use super::{HandleData, DirectedId, DynamicProtocol, Cx, Db, DbResult};

pub struct State<Inner> {
    stream_id: u64,
    stream_forward: bool,
    error: bool,
    stream: Option<DbStream>,
    inner: Option<Inner>,
    hl: hl::State,
}

// high level state machine
mod hl {
    use std::{borrow::Cow, str::Utf8Error};

    use super::ll;

    #[derive(Debug, Default)]
    pub struct Output<'a> {
        pub tokens: Vec<String>,
        pub error: Option<(Utf8Error, Vec<u8>)>,
        pub agreed: Option<(String, Cow<'a, [u8]>)>,
    }

    #[derive(Default)]
    pub struct State {
        incoming: OneDirection,
        outgoing: OneDirection,
    }

    #[derive(Default)]
    struct OneDirection {
        inner: ll::State,
        simultaneous_connect: bool,
        done: Option<String>,
    }

    impl State {
        pub fn poll<'a, 'b>(&'a mut self, incoming: bool, bytes: &'b [u8]) -> Output<'b> {
            let (this, other) = if incoming {
                (&mut self.incoming, &mut self.outgoing)
            } else {
                (&mut self.outgoing, &mut self.incoming)
            };

            this.inner.append(bytes);
            let mut output_ = Output::default();
            if let (Some(lp), Some(rp)) = (&this.done, &other.done) {
                if *lp == *rp {
                    output_.agreed = Some((lp.clone(), this.inner.end(bytes)));
                }
            }

            while let Some(output) = this.inner.poll() {
                match output {
                    Err(err) => {
                        output_.error = Some(err);
                        break;
                    },
                    Ok(ll::Output::String(s)) => {
                        output_.tokens.push(s.clone());
                        if s.starts_with("/multistream/") {
                            //
                        } else if s.starts_with("/libp2p/simultaneous-connect") {
                            this.simultaneous_connect = true;
                        } else if s == "na" {
                            if other.simultaneous_connect {
                                other.simultaneous_connect = false;
                            } else {
                                other.done = None;
                            }
                        } else if s.starts_with("select") {
                            this.simultaneous_connect = false;
                        } else {
                            if !this.simultaneous_connect && !other.simultaneous_connect {
                                this.done = Some(s);
                            }
                        }
                    }
                    Ok(ll::Output::InitiatorToken) => output_.tokens.push("initiator".to_string()),
                    Ok(ll::Output::ResponderToken) => output_.tokens.push("responder".to_string()),
                }
            }

            output_
        }
    }
}

// low level parser
mod ll {
    use std::{borrow::Cow, mem, str, str::Utf8Error};

    pub enum Output {
        String(String),
        InitiatorToken,
        ResponderToken,
    }

    pub struct State {
        acc: Vec<u8>,
    }

    impl Default for State {
        fn default() -> Self {
            State {
                // enough for most multistream select packet
                acc: Vec::with_capacity(128),
            }
        }
    }

    impl State {
        pub fn end<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
            if self.acc.is_empty() {
                Cow::Borrowed(bytes)
            } else {
                let mut acc = mem::take(&mut self.acc);
                acc.extend_from_slice(bytes);
                Cow::Owned(acc)
            }
        }

        pub fn append(&mut self, bytes: &[u8]) {
            self.acc.extend_from_slice(bytes);
        }

        pub fn poll(&mut self) -> Option<Result<Output, (Utf8Error, Vec<u8>)>> {
            use unsigned_varint::decode;

            if self.acc.starts_with(b"\ninitiator\n") {
                self.acc.drain(..11);
                Some(Ok(Output::InitiatorToken))
            } else if self.acc.starts_with(b"\nresponder\n") {
                self.acc.drain(..11);
                Some(Ok(Output::ResponderToken))
            } else {
                let (result, new) = {
                    let (length, remaining) = decode::usize(&self.acc).ok()?;
                    if remaining.len() < length {
                        return None;
                    }
    
                    let (msg, remaining) = remaining.split_at(length);
                    let result = str::from_utf8(msg)
                        .map(|s| s.trim_end_matches('\n').to_string())
                        .map(Output::String)
                        .map_err(|err| (err, msg.to_vec()));
    
                    (result, remaining.to_vec())
                };
                self.acc = new;
                Some(result)
            }
        }
    }
}

impl<Inner> From<(u64, bool)> for State<Inner> {
    fn from((stream_id, stream_forward): (u64, bool)) -> Self {
        State {
            stream_id,
            stream_forward,
            error: false,
            stream: None,
            inner: None,
            hl: hl::State::default(),
        }
    }
}

impl<Inner> HandleData for State<Inner>
where
    Inner: HandleData + DynamicProtocol,
{
    #[inline(never)]
    fn on_data(&mut self, id: DirectedId, bytes: &mut [u8], cx: &mut Cx, db: &Db) -> DbResult<()> {
        if self.error {
            return Ok(());
        }

        let output = self.hl.poll(id.incoming, bytes);

        if !output.tokens.is_empty() {
            let stream = self.stream.get_or_insert_with(|| {
                let stream_id = if self.stream_forward {
                    StreamId::Forward(self.stream_id)
                } else {
                    StreamId::Backward(self.stream_id)
                };
                db.add(stream_id, StreamKind::Select)
            });
            for token in output.tokens {
                stream.add(id.incoming, id.metadata.time, token.as_bytes())?;
            }
        }

        if let Some((error, msg)) = output.error {
            log::error!(
                "{id}, {}, stream_id: {}, unparsed {}, {error}",
                db.id(),
                self.stream_id,
                hex::encode(msg)
            );
            self.error = true;
        }

        if let Some((protocol, mut data)) = output.agreed {
            if let StreamKind::Unknown = protocol.parse().expect("cannot fail") {
                log::error!("{id} {}, bad protocol name {protocol}", db.id());
            }
            let inner = self.inner.get_or_insert_with(|| {
                Inner::from_name(&protocol, self.stream_id, self.stream_forward)
            });
            inner.on_data(id, data.to_mut(), cx, db)?;
        }

        Ok(())
    }
}

#[cfg(test)]
#[test]
#[rustfmt::skip]
fn simultaneous_connect_test() {
    let mut state = State::<()>::from((0, false));

    let mut data = hex::decode("132f6d756c746973747265616d2f312e302e300a1d2f6c69627032702f73696d756c74616e656f75732d636f6e6e6563740a072f6e6f6973650a").expect("valid constant");
    let result = state.hl.poll(false, &mut data);
    assert!(dbg!(result).agreed.is_none());

    let mut data = hex::decode("132f6d756c746973747265616d2f312e302e300a1d2f6c69627032702f73696d756c74616e656f75732d636f6e6e6563740a072f6e6f6973650a1c73656c6563743a31383333363733363237323438313935323033380a").expect("valid constant");
    let result = state.hl.poll(true, &mut data);
    assert!(dbg!(result).agreed.is_none());

    let mut data = hex::decode("1c73656c6563743a31343838333538303531393436383433383239370a0a726573706f6e6465720a").expect("valid constant");
    let result = state.hl.poll(false, &mut data);
    assert!(dbg!(result).agreed.is_none());

    let mut data = hex::decode("0a696e69746961746f720a072f6e6f6973650a").expect("valid constant");
    let result = state.hl.poll(true, &mut data);
    assert!(dbg!(result).agreed.is_none());

    let mut data = hex::decode("072f6e6f6973650a").expect("valid constant");
    let result = state.hl.poll(false, &mut data);
    assert!(dbg!(result).agreed.is_none());

    let mut data = hex::decode("0020c29c4aa9bc861ac3163bfc562ab3f1ca984440f50ca7944ab1fcb40b398bac34").expect("valid constant");
    let result = state.hl.poll(true, &mut data);
    assert!(dbg!(result).agreed.is_some());
}

#[cfg(test)]
#[test]
#[rustfmt::skip]
fn simultaneous_connect_with_accumulator_test() {
    let mut state = State::<()>::from((0, false));

    let mut data = hex::decode("132f6d756c746973747265616d2f312e302e300a1d2f6c69627032702f73696d756c74616e656f75732d636f6e6e6563740a072f6e6f6973650a").expect("valid constant");
    let result = state.hl.poll(false, &mut data);
    assert!(dbg!(result).agreed.is_none());

    println!();

    let mut data = hex::decode("132f6d756c746973747265616d2f312e302e300a1d2f6c69627032702f73696d756c74616e656f75732d636f6e6e6563740a072f6e6f6973650a1c73656c6563743a31383333363733363237323438313935323033380a").expect("valid constant");
    let chunks = [1, 19, 1, 29, 1, 7, 1, 28];
    for chunk in chunks {
        let mut chunk_data = data.drain(..chunk).collect::<Vec<u8>>();
        let result = state.hl.poll(true, &mut chunk_data);
        assert!(dbg!(result).agreed.is_none());
    }

    println!();

    let mut data = hex::decode("1c73656c6563743a31343838333538303531393436383433383239370a0a726573706f6e6465720a").expect("valid constant");
    let chunks = [29, 11];
    for chunk in chunks {
        let mut chunk_data = data.drain(..chunk).collect::<Vec<u8>>();
        let result = state.hl.poll(false, &mut chunk_data);
        assert!(dbg!(result).agreed.is_none());
    }

    println!();

    let mut data = hex::decode("0a696e69746961746f720a072f6e6f6973650a").expect("valid constant");
    let chunks = [1, 10, 1, 7];
    for chunk in chunks {
        let mut chunk_data = data.drain(..chunk).collect::<Vec<u8>>();
        let result = state.hl.poll(true, &mut chunk_data);
        assert!(dbg!(result).agreed.is_none());
    }

    println!();

    let mut data = hex::decode("072f6e6f6973650a").expect("valid constant");
    let result = state.hl.poll(false, &mut data);
    assert!(dbg!(result).agreed.is_none());

    println!();

    let mut data = hex::decode("0020c29c4aa9bc861ac3163bfc562ab3f1ca984440f50ca7944ab1fcb40b398bac34").expect("valid constant");
    let result = state.hl.poll(true, &mut data);
    assert!(dbg!(result).agreed.is_some());
}
