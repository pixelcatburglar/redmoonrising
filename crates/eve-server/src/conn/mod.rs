//! One client connection: framing, request dispatch, replies, and the
//! notifications other connections queue for it.
//!
//! The login handshake lives in `auth`, session moves (character select, dock,
//! undock, jump) in `location`.

mod auth;
mod location;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use eve_marshal::{StringTable, Value};
use eve_proto::{
    decode_payload, encode_frame, llv, maybe_compress, packet::MACHONETERR_WRAPPEDEXCEPTION, FrameReader,
    MachoAddress, MachoPacket, MessageType, ProtoError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, error, field, info, info_span, trace, warn, Instrument, Span};

use crate::bulkdata::BulkData;
use crate::exceptions::{custom_error, service_not_found, user_error};
use crate::services::{self, Call, Ctx, Outcome};
use crate::session::Session;
use crate::svc::{bound, chat, inflight, space};
use crate::world::{Notification, Presence, Push, World};

/// State shared by every connection.
pub struct Server {
    pub table: StringTable,
    pub db: tokio_postgres::Client,
    pub db_url: String,
    pub node_id: i64,
    pub bulk: BulkData,
    pub dogma: crate::dogma::Catalog,
    pub services: services::Registry,
    pub world: World,
    pub dump_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error(transparent)]
    Marshal(#[from] eve_marshal::Error),
    #[error("database: {0}")]
    Db(#[from] tokio_postgres::Error),
    #[error("{0}")]
    Protocol(String),
    /// We sent the client a close reason and are hanging up.
    #[error("closed by the server: {0}")]
    Closed(String),
}

type Result<T = ()> = std::result::Result<T, ConnError>;

pub struct Conn {
    stream: TcpStream,
    reader: FrameReader,
    server: Arc<Server>,
    peer: SocketAddr,
    span: Span,
    client_id: i64,
    session: Session,
    authenticated: bool,
    pending_session_change: Option<MachoPacket>,
}

impl Conn {
    pub fn new(stream: TcpStream, peer: SocketAddr, server: Arc<Server>, seq: i64) -> Self {
        let client_id = seq * 10_000_000 + server.node_id;
        let span = info_span!(
            "conn",
            client = client_id,
            %peer,
            user = field::Empty,
            char = field::Empty,
        );
        Conn {
            stream,
            reader: FrameReader::new(),
            server,
            peer,
            span,
            client_id,
            session: Session::new(seq),
            authenticated: false,
            pending_session_change: None,
        }
    }

    pub async fn run(mut self) {
        let span = self.span.clone();
        async move {
            info!("connected");
            let notifications = self.server.world.lock().connect(self.client_id);
            let result = self.serve(notifications).await;
            self.leave_world();
            match result {
                Ok(()) => info!("disconnected"),
                Err(ConnError::Closed(reason)) => warn!("closed by the server: {reason}"),
                Err(ConnError::Io(e)) => warn!("connection lost: {}", crate::logging::chain(&e)),
                Err(e) => error!("connection dropped: {}", crate::logging::chain(&e)),
            }
        }
        .instrument(span)
        .await
    }

    async fn serve(&mut self, mut notifications: UnboundedReceiver<Push>) -> Result {
        self.handshake().await?;
        loop {
            // Both branches are cancellation-safe: a read that loses the race has
            // not consumed any bytes, and recv() loses nothing.
            let frame = tokio::select! {
                frame = self.read_frame() => match frame? {
                    Some(frame) => frame,
                    None => return Ok(()),
                },
                Some(push) = notifications.recv() => {
                    match push {
                        Push::Notify(n) => self.send_notification(&n).await?,
                        Push::Move(m) => self.forced_move(m).await?,
                    }
                    continue;
                }
            };
            let value = match decode_payload(&frame, &self.server.table) {
                Ok(v) => v,
                Err(ProtoError::PeerClosed { reason, .. }) => {
                    info!("client closed the connection: {reason}");
                    return Ok(());
                }
                Err(e) => {
                    self.dump_frame(&frame, &e);
                    return self.close(format!("Malformed packet: {e}")).await;
                }
            };
            let packet = match MachoPacket::from_value(&value) {
                Ok(p) => p,
                Err(e) => return self.close(format!("Malformed packet: {e}")).await,
            };
            if !packet.command.allowed_from_client() {
                return self.close("Invalid packet - command not allowed from a client").await;
            }
            self.dispatch(packet).await?;
        }
    }

    /// The character leaves the shared world: its ship leaves space and its chat
    /// channels hear it go.
    fn leave_world(&mut self) {
        let mut world = self.server.world.lock();
        let channels = world.disconnect(self.client_id);
        if let (Some(system), Some(ship_id), None) =
            (self.session.solar_system_id, self.session.ship_id, self.session.station_id)
        {
            // The pilot comes back docked, so the ship keeps only its damage.
            if let Some(fight) = space::remove_ship(&mut world, system, ship_id).and_then(|s| s.fight) {
                let server = Arc::clone(&self.server);
                tokio::spawn(async move { inflight::save_vitals(&server.db, ship_id, &fight, true).await });
            }
        }
        let who = chat::who(&self.session);
        for (channel, members) in channels {
            chat::announce(&world, &members, &channel, "LeaveChannel", &who, Vec::new());
        }
    }

    async fn dispatch(&mut self, mut packet: MachoPacket) -> Result {
        let call_id = packet.source.as_ref().and_then(|a| a.call_id());
        packet.source = Some(self.client_address(call_id));
        if self.authenticated {
            packet.user_id = self.session.user_id;
        }
        trace!(command = packet.command.name(), "recv");

        match packet.command {
            MessageType::AuthenticationReq => self.authenticate(packet).await,
            MessageType::PingReq => {
                let times = packet.body.first().cloned().unwrap_or(Value::None);
                let rsp = packet.respond(MessageType::PingRsp, vec![times]);
                self.send_packet(&rsp).await
            }
            MessageType::CallReq => self.service_call(packet).await,
            MessageType::Notification => {
                self.client_notification(&packet);
                Ok(())
            }
            other => {
                warn!("unhandled {} packet", other.name());
                Ok(())
            }
        }
    }

    async fn service_call(&mut self, packet: MachoPacket) -> Result {
        let call = match services::parse_call(&packet, &self.server.table) {
            Ok(c) => c,
            Err(e) => {
                warn!("malformed service call: {}", crate::logging::chain(&e));
                let err = custom_error("The server could not read that call.");
                return self.send_error(&packet, err).await;
            }
        };
        let span = info_span!("call", svc = %call.service, method = %call.method);
        self.handle_call(&packet, &call).instrument(span).await
    }

    async fn handle_call(&mut self, req: &MachoPacket, call: &Call) -> Result {
        let started = Instant::now();
        let arg = |i: usize| call.arg_id(i);

        // Calls that move the session are handled here, not by a service, because
        // they change connection state.
        if call.service == "character" && call.method == "SelectCharacterID" {
            return self.select_character(req, arg(0)).await;
        }
        if bound::owns(&call.service, "beyonce") {
            match call.method.as_str() {
                "Dock" => return self.dock(req, arg(0), false).await,
                // The GM shortcut: docks from anywhere in the system.
                "TurboDock" => return self.dock(req, arg(0), true).await,
                "StargateJump" => return self.stargate_jump(req, arg(0), arg(1)).await,
                _ => {}
            }
        }
        if bound::owns(&call.service, "ship") {
            match call.method.as_str() {
                "Undock" => return self.undock(req, None).await,
                "Board" => return self.board(req, arg(0), None).await,
                "MachoBindObject" => match bound::piggyback(call) {
                    Some((m, _)) if m == "Undock" => {
                        return self.undock(req, Some(bound::params_of(call))).await;
                    }
                    Some((m, args)) if m == "Board" => {
                        let ship = args.first().and_then(|v| v.as_i64()).unwrap_or(0);
                        return self.board(req, ship, Some(bound::params_of(call))).await;
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        let outcome = self.server.services.dispatch(call, &self.ctx()).await;
        match outcome {
            Outcome::Value(v) => {
                self.reply(req, &v).await?;
                if call.method == "GetCachableObject" {
                    trace!(took = ?started.elapsed(), "ok");
                } else {
                    debug!(took = ?started.elapsed(), "ok");
                }
            }
            Outcome::Unknown(what) => {
                warn!("not implemented: {what}");
                self.send_error(req, service_not_found(&what)).await?;
            }
            Outcome::UserError { msg, args } => {
                info!("refused with UserError {msg}");
                let pairs: Vec<(&str, Value)> =
                    args.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                self.send_error(req, user_error(&msg, &pairs)).await?;
            }
        }
        self.flush_pending_session_change(&call.service).await
    }

    fn client_notification(&self, packet: &MachoPacket) {
        let inner = packet
            .body
            .first()
            .and_then(|p| p.seq())
            .and_then(|p| match p.get(1) {
                Some(Value::MarshalStream(b)) | Some(Value::Bytes(b)) => {
                    eve_marshal::load(b, &self.server.table).ok()
                }
                _ => None,
            });
        let method = inner
            .as_ref()
            .and_then(|v| v.seq())
            .and_then(|p| p.get(1))
            .and_then(|m| m.as_bytes())
            .map(|b| String::from_utf8_lossy(b).into_owned());
        match method.as_deref() {
            Some("ClientHasReleasedTheseObjects") => {}
            Some(name) => debug!("client notification {name} ignored"),
            None => warn!("client notification with an unreadable payload"),
        }
    }

    /// The session change made at login is held back until the client's
    /// ConnectToServer has returned, which its first non-caching call signals.
    async fn flush_pending_session_change(&mut self, service: &str) -> Result {
        if service == "objectCaching" {
            return Ok(());
        }
        let Some(change) = self.pending_session_change.take() else { return Ok(()) };
        debug!("releasing the login session change");
        self.send_packet(&change).await
    }

    // ---- helpers -----------------------------------------------------------

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            bulk: &self.server.bulk,
            dogma: &self.server.dogma,
            session: &self.session,
            db: &self.server.db,
            db_url: &self.server.db_url,
            node_id: self.server.node_id,
            world: &self.server.world,
            client_id: self.client_id,
        }
    }

    fn client_address(&self, call_id: Option<i64>) -> MachoAddress {
        MachoAddress::Client { client_id: Some(self.client_id), call_id, service: None }
    }

    fn notification_source(&self) -> MachoAddress {
        MachoAddress::Node { node_id: Some(self.server.node_id), service: None, call_id: None }
    }

    fn session_change(&self, body: Vec<Value>) -> MachoPacket {
        let mut change = MachoPacket::new(MessageType::SessionChangeNotification, body);
        change.source = Some(self.notification_source());
        change.destination = Some(self.client_address(None));
        change.user_id = self.session.user_id;
        change
    }

    /// Tell the client how its session differs from `before`.
    async fn send_session_change_from(&mut self, before: &Session) -> Result {
        let change = self.session_change(self.session.change_body_from(before));
        self.server.world.lock().set_presence(self.client_id, self.presence());
        self.send_packet(&change).await
    }

    /// Where this connection's character is, for the shared [`World`].
    fn presence(&self) -> Option<Presence> {
        Some(Presence {
            char_id: self.session.char_id?,
            corp_id: self.session.corp_id?,
            solar_system_id: self.session.solar_system_id2?,
            docked: self.session.station_id.is_some(),
        })
    }

    // ---- wire --------------------------------------------------------------

    async fn read_frame(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(frame) = self.reader.next_frame()? {
                return Ok(Some(frame));
            }
            let mut buf = [0u8; 8192];
            let n = self.stream.read(&mut buf).await?;
            if n == 0 {
                return Ok(None);
            }
            self.reader.push(&buf[..n]);
        }
    }

    /// Marshal, compress when it pays (as the client does, frame.rs), frame
    /// and send. A frame past what the client accepts ends the connection.
    async fn write_value(&mut self, v: &Value) -> Result {
        let raw = eve_marshal::save(v, &self.server.table)?;
        let frame = encode_frame(&maybe_compress(&raw, 0))?;
        self.stream.write_all(&frame).await?;
        Ok(())
    }

    async fn send_packet(&mut self, p: &MachoPacket) -> Result {
        trace!(command = p.command.name(), "send");
        let sent = self.write_value(&p.to_value()?).await;
        if let Err(e) = &sent {
            error!(command = p.command.name(), "packet not sent: {}", crate::logging::chain(e));
        }
        sent
    }

    /// A broadcast-style notification narrowcast to this client: the client
    /// routes it by event name (`Do*` sent, `Process*` chained, else scattered).
    async fn send_notification(&mut self, n: &Notification) -> Result {
        trace!(event = n.event, "notify");
        let inner = Value::Tuple(vec![Value::Int(0), Value::Tuple(vec![Value::Int(1), n.args.clone()])]);
        let blob = eve_marshal::save(&inner, &self.server.table)?;
        let mut note = MachoPacket::notification(Value::Tuple(vec![Value::Int(0), Value::Bytes(blob)]));
        note.source = Some(self.notification_source());
        note.destination = Some(MachoAddress::Broadcast {
            broadcast_id: Value::bytes(n.event),
            narrowcast: Value::List(vec![Value::Int(self.client_id)]),
            id_type: Value::bytes("clientID"),
        });
        note.user_id = self.session.user_id;
        self.send_packet(&note).await
    }

    async fn reply(&mut self, req: &MachoPacket, value: &Value) -> Result {
        let rsp = services::make_response(req, value, &self.server.table)?;
        self.send_packet(&rsp).await
    }

    async fn send_error(&mut self, req: &MachoPacket, err: Value) -> Result {
        let blob = eve_marshal::save(&err, &self.server.table)?;
        let rsp = req.error_response(
            MACHONETERR_WRAPPEDEXCEPTION,
            Value::Tuple(vec![Value::Bytes(blob)]),
        );
        self.send_packet(&rsp).await
    }

    /// Send the client a close reason, then end the connection.
    async fn close(&mut self, reason: impl Into<String>) -> Result {
        let reason = reason.into();
        if let Ok(frame) = llv::close_reason(&reason, &self.server.table).and_then(|r| encode_frame(&r)) {
            let _ = self.stream.write_all(&frame).await;
        }
        let _ = self.stream.flush().await;
        Err(ConnError::Closed(reason))
    }

    fn dump_frame(&self, frame: &[u8], why: &ProtoError) {
        let path = self
            .server
            .dump_dir
            .join(format!("badframe-{}-{}.bin", self.client_id, frame.len()));
        let head: Vec<String> = frame.iter().take(48).map(|b| format!("{b:02x}")).collect();
        error!(bytes = frame.len(), head = %head.join(" "), "undecodable frame: {why}");
        match std::fs::write(&path, frame) {
            Ok(()) => info!("saved the frame to {}", path.display()),
            Err(e) => error!("could not save the frame to {}: {}", path.display(), crate::logging::chain(&e)),
        }
    }
}
