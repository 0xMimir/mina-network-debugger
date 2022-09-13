use std::{
    path::Path,
    time::SystemTime,
    sync::{
        atomic::{AtomicU64, Ordering::SeqCst},
        Arc,
    },
};

use radiation::{Absorb, Emit};

use crate::{event::ConnectionInfo, decode::MessageType, custom_coding};

use super::{
    core::{DbCore, DbError},
    types::{Connection, ConnectionId, StreamFullId, Message, MessageId, StreamId, StreamKind},
};

pub struct DbFacade {
    cns: AtomicU64,
    messages: Arc<AtomicU64>,
    inner: DbCore,
}

impl DbFacade {
    pub fn open<P>(path: P) -> Result<Self, DbError>
    where
        P: AsRef<Path>,
    {
        let inner = DbCore::open(path)?;

        Ok(DbFacade {
            cns: AtomicU64::new(inner.total::<{ DbCore::CONNECTIONS_CNT }>()?),
            messages: Arc::new(AtomicU64::new(inner.total::<{ DbCore::MESSAGES_CNT }>()?)),
            inner,
        })
    }

    pub fn add(
        &self,
        info: ConnectionInfo,
        incoming: bool,
        timestamp: SystemTime,
    ) -> Result<DbGroup, DbError> {
        let id = ConnectionId(self.cns.fetch_add(1, SeqCst));
        let v = Connection {
            info,
            incoming,
            timestamp,
        };
        self.inner.put_cn(id, v)?;
        self.inner.set_total::<{ DbCore::CONNECTIONS_CNT }>(id.0)?;

        Ok(DbGroup {
            id,
            messages: self.messages.clone(),
            inner: self.inner.clone(),
        })
    }

    pub fn core(&self) -> DbCore {
        self.inner.clone()
    }
}

pub struct DbGroup {
    id: ConnectionId,
    messages: Arc<AtomicU64>,
    inner: DbCore,
}

impl DbGroup {
    pub fn add(&self, id: StreamId, kind: StreamKind) -> DbStream {
        DbStream {
            id: StreamFullId { cn: self.id, id },
            kind,
            messages: self.messages.clone(),
            inner: self.inner.clone(),
        }
    }

    pub fn id(&self) -> ConnectionId {
        self.id
    }

    pub fn add_raw(
        &self,
        incoming: bool,
        timestamp: SystemTime,
        bytes: &[u8],
    ) -> Result<(), DbError> {
        #[derive(Absorb, Emit)]
        struct ChunkHeader {
            size: u32,
            #[custom_absorb(custom_coding::time_absorb)]
            #[custom_emit(custom_coding::time_emit)]
            timestamp: SystemTime,
            incoming: bool,
        }

        let header = ChunkHeader {
            size: bytes.len() as u32,
            timestamp,
            incoming,
        };

        // header size is 17
        let b = Vec::with_capacity(bytes.len() + 17);
        let mut b = header.emit(b);
        b.extend_from_slice(bytes);

        let sb = self.inner.get_raw_stream(self.id)?;
        let mut file = sb.lock().expect("poisoned");
        let _ = file.write(&b).map_err(|err| DbError::IoCn(self.id, err))?;

        Ok(())
    }
}

impl Drop for DbGroup {
    fn drop(&mut self) {
        self.inner.remove_raw_stream(self.id);
    }
}

pub struct DbStream {
    id: StreamFullId,
    kind: StreamKind,
    messages: Arc<AtomicU64>,
    inner: DbCore,
}

impl Drop for DbStream {
    fn drop(&mut self) {
        self.inner.remove_stream(self.id);
    }
}

impl DbStream {
    pub fn add(&self, incoming: bool, timestamp: SystemTime, bytes: &[u8]) -> Result<(), DbError> {
        let sb = self.inner.get_stream(self.id)?;
        let mut file = sb.lock().expect("poisoned");
        let offset = file.write(bytes).map_err(|err| DbError::Io(self.id, err))?;
        drop(file);

        let tys = match self.kind {
            StreamKind::Meshsub => crate::decode::meshsub::parse_types(bytes)?,
            StreamKind::Kad => crate::decode::kademlia::parse_types(bytes)?,
            StreamKind::Handshake => crate::decode::noise::parse_types(bytes)?,
            StreamKind::Rpc => crate::decode::rpc::parse_types(bytes)?,
            StreamKind::IpfsId => vec![MessageType::Identify],
            StreamKind::IpfsPush => vec![MessageType::IdentifyPush],
            _ => vec![],
        };

        let id = MessageId(self.messages.fetch_add(1, SeqCst));
        let v = Message {
            connection_id: self.id.cn,
            stream_id: self.id.id,
            stream_kind: self.kind,
            incoming,
            timestamp,
            offset,
            size: bytes.len() as u32,
        };
        self.inner.put_message(id, v, tys)?;
        self.inner.set_total::<{ DbCore::MESSAGES_CNT }>(id.0)?;

        Ok(())
    }
}
